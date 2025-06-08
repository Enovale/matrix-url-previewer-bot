use std::path::{Path, PathBuf};

use eyre::Result;
use indexmap::IndexSet;
use matrix_sdk::config::SyncSettings;
use matrix_sdk::event_handler::Ctx;
use matrix_sdk::room::Receipts;
use matrix_sdk::ruma::OwnedEventId;
use matrix_sdk::ruma::api::client::filter::FilterDefinition;
use matrix_sdk::ruma::events::room::encrypted::OriginalSyncRoomEncryptedEvent;
use matrix_sdk::ruma::events::room::member::{MembershipState, SyncRoomMemberEvent};
use matrix_sdk::ruma::events::room::message::{
    MessageFormat, MessageType, OriginalSyncRoomMessageEvent, Relation,
};
use matrix_sdk::ruma::events::room::redaction::OriginalSyncRoomRedactionEvent;
use matrix_sdk::{Client, Room, RoomState};
use tracing::{debug, error, info, instrument, warn};
use tracing_subscriber::{EnvFilter, prelude::*};
use url::Url;

use crate::worker::Worker;

mod common;
mod extract_url;
mod html_escape;
mod worker;

#[derive(clap::Parser)]
struct Args {
    #[clap(subcommand)]
    command: Command,
}

#[derive(clap::Subcommand)]
enum Command {
    #[clap(about = "Perform initial setup of Matrix account")]
    Setup {
        #[clap(
            long = "data",
            value_name = "PATH",
            help = "Path to store Matrix data between sessions"
        )]
        data_dir: PathBuf,
        #[clap(
            long,
            value_name = "DEVICE_NAME",
            default_value = "matrixbot-ezlogin/matrix-url-previewer-bot",
            help = "Device name to use for this session"
        )]
        device_name: String,
    },
    #[clap(about = "Run the bot")]
    Run {
        #[clap(
            long = "data",
            value_name = "PATH",
            help = "Path to an existing Matrix session"
        )]
        data_dir: PathBuf,
    },
    #[clap(about = "Log out of the Matrix session, and delete the state database")]
    Logout {
        #[clap(
            long = "data",
            value_name = "PATH",
            help = "Path to an existing Matrix session"
        )]
        data_dir: PathBuf,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    color_eyre::install()?;
    matrixbot_ezlogin::DuplexLog::init();
    tracing_subscriber::registry()
        .with(tracing_error::ErrorLayer::default())
        .with({
            let mut filter = EnvFilter::new(concat!(
                "warn,",
                env!("CARGO_CRATE_NAME"),
                "=debug,matrixbot_ezlogin=info"
            ));
            if let Some(env) = std::env::var_os(EnvFilter::DEFAULT_ENV) {
                for segment in env.to_string_lossy().split(',') {
                    if let Ok(directive) = segment.parse() {
                        filter = filter.add_directive(directive);
                    }
                }
            }
            filter
        })
        .with(
            tracing_subscriber::fmt::layer().with_writer(matrixbot_ezlogin::DuplexLog::get_writer),
        )
        .init();

    let args: Args = clap::Parser::parse();

    match args.command {
        Command::Setup {
            data_dir,
            device_name,
        } => drop(matrixbot_ezlogin::setup_interactive(&data_dir, &device_name).await?),
        Command::Run { data_dir } => run(&data_dir).await?,
        Command::Logout { data_dir } => matrixbot_ezlogin::logout(&data_dir).await?,
    };
    Ok(())
}

async fn run(data_dir: &Path) -> Result<()> {
    let worker = Worker::new(data_dir).await?;
    let (client, sync_helper) = matrixbot_ezlogin::login(data_dir).await?;

    // We don't ignore joining and leaving events happened during downtime.
    client.add_event_handler(on_leave);

    // Enable room members lazy-loading, it will speed up the initial sync a lot with accounts in lots of rooms.
    // https://spec.matrix.org/v1.6/client-server-api/#lazy-loading-room-members
    let sync_settings =
        SyncSettings::default().filter(FilterDefinition::with_lazy_loading().into());

    info!(
        "Skipping messages since last logout. May take longer depending on the number of rooms joined."
    );
    sync_helper
        .sync_once(&client, sync_settings.clone())
        .await?;

    client.add_event_handler_context(worker);
    client.add_event_handler(on_message);
    client.add_event_handler(on_deletion);
    client.add_event_handler(on_utd);

    // Forget rooms that we already left
    let left_rooms = client.left_rooms();
    tokio::spawn(async move {
        for room in left_rooms {
            info!("Forgetting room {}.", room.room_id());
            match room.forget().await {
                Ok(_) => info!("Forgot room {}.", room.room_id()),
                Err(err) => error!("Failed to forget room {}: {:?}", room.room_id(), err),
            }
        }
    });

    info!("Starting sync.");
    sync_helper.sync(&client, sync_settings).await?;

    Ok(())
}

#[instrument(skip_all)]
fn set_read_marker(room: Room, event_id: OwnedEventId) {
    tokio::spawn(async move {
        if let Err(err) = room
            .send_multiple_receipts(
                Receipts::new()
                    .fully_read_marker(event_id.clone())
                    .public_read_receipt(event_id.clone()),
            )
            .await
        {
            error!(
                "Failed to set the read marker of room {} to event {}: {:?}",
                room.room_id(),
                event_id,
                err
            );
        }
    });
}

// https://spec.matrix.org/v1.14/client-server-api/#mroommessage
#[instrument(skip_all)]
async fn on_message(
    event: OriginalSyncRoomMessageEvent,
    room: Room,
    client: Client,
    ctx: Ctx<Worker>,
) -> Result<()> {
    if event.sender == client.user_id().unwrap() {
        // Ignore my own message
        return Ok(());
    }
    debug!("room = {}, event = {:?}", room.room_id(), event);
    set_read_marker(room.clone(), event.event_id.clone());
    if room.state() != RoomState::Joined {
        info!(
            "Ignoring room {}: Current room state is {:?}.",
            room.room_id(),
            room.state()
        );
        return Ok(());
    }

    let (original_event_id, thread_id, new_content) = match event.content.relates_to {
        Some(Relation::Replacement(replacement)) => {
            (replacement.event_id, None, replacement.new_content)
        }
        Some(Relation::Thread(ref thread)) => (
            event.event_id,
            Some(thread.event_id.clone()),
            event.content.into(),
        ),
        _ => (event.event_id, None, event.content.into()),
    };
    let MessageType::Text(text) = new_content.msgtype else {
        return Ok(());
    };
    let html = text
        .formatted
        .filter(|formatted| formatted.format == MessageFormat::Html);
    let urls = if let Some(html) = html {
        extract_url::extract_urls_from_html(&html.body)
    } else {
        let mut urls = IndexSet::new();
        for line in text.body.lines().skip_while(|line| line.starts_with("> ")) {
            urls.extend(extract_url::extract_urls_from_text(line));
        }
        urls
    };

    info!("URLs: {:?}", urls.iter().map(Url::as_str).collect::<Vec<_>>());
    ctx.on_message(room, thread_id, &original_event_id, urls)
        .await?;

    Ok(())
}

#[instrument(skip_all)]
async fn on_deletion(
    event: OriginalSyncRoomRedactionEvent,
    room: Room,
    client: Client,
    ctx: Ctx<Worker>,
) -> Result<()> {
    if event.sender == client.user_id().unwrap() {
        // Ignore my own message
        return Ok(());
    }
    debug!("room = {}, event = {:?}", room.room_id(), event);
    set_read_marker(room.clone(), event.event_id.clone());
    if room.state() != RoomState::Joined {
        info!(
            "Ignoring room {}: Current room state is {:?}.",
            room.room_id(),
            room.state()
        );
        return Ok(());
    }

    let room_version = room.clone_info().room_version_or_default();
    let original_event_id = event.redacts(&room_version);
    ctx.on_deletion(room, &original_event_id).await?;
    Ok(())
}

// https://spec.matrix.org/v1.14/client-server-api/#mroomencrypted
#[instrument(skip_all)]
async fn on_utd(event: OriginalSyncRoomEncryptedEvent, room: Room) {
    debug!("room = {}, event = {:?}", room.room_id(), event);
    error!("Unable to decrypt event {}.", event.event_id);
}

// https://spec.matrix.org/v1.14/client-server-api/#mroommember
#[instrument(skip_all)]
async fn on_leave(event: SyncRoomMemberEvent, room: Room) {
    if !matches!(
        event.membership(),
        MembershipState::Leave | MembershipState::Ban
    ) {
        return;
    }
    debug!("room = {}, event = {:?}", room.room_id(), event);

    match room.state() {
        RoomState::Joined => {
            tokio::spawn(async move {
                if let Err(err) = room.sync_members().await {
                    warn!("Failed to sync members of {}: {:?}", room.room_id(), err);
                }
                // Only I remain in the room.
                if room.joined_members_count() <= 1 {
                    info!("Leaving room {}.", room.room_id());
                    match room.leave().await {
                        Ok(_) => info!("Left room {}.", room.room_id()),
                        Err(err) => error!("Failed to leave room {}: {:?}", room.room_id(), err),
                    }
                }
            });
        }
        RoomState::Banned | RoomState::Left => {
            // Either I successfully left the room, or someone kicked me out.
            tokio::spawn(async move {
                info!("Forgetting room {}.", room.room_id());
                match room.forget().await {
                    Ok(_) => info!("Forgot room {}.", room.room_id()),
                    Err(err) => error!("Failed to forget room {}: {:?}", room.room_id(), err),
                }
            });
        }
        _ => (),
    }
}
