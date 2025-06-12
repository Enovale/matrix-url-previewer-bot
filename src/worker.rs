use std::path::Path;
use std::sync::LazyLock;

use deadpool_sqlite::rusqlite::OptionalExtension;
use deadpool_sqlite::{Config, Pool, Runtime};
use eyre::{Report, Result};
use indexmap::IndexSet;
use matrix_sdk::Room;
use matrix_sdk::ruma::api::client::authenticated_media::get_media_preview;
use matrix_sdk::ruma::events::Mentions;
use matrix_sdk::ruma::events::relation::{Replacement, Thread};
use matrix_sdk::ruma::events::room::message::{Relation, RoomMessageEventContentWithoutRelation};
use matrix_sdk::ruma::{EventId, OwnedEventId};
use regex::Regex;
use serde::Deserialize;
use serde_with::{DefaultOnError, serde_as};
use tracing::{Instrument, error, info, instrument};
use url::Url;

use crate::common::{MAX_RESPONSE_TEXT_BYTES, MAX_URL_COUNTS_PER_MESSAGE, SAFE_URL_LENGTH};
use crate::html_escape;

#[derive(Clone)]
pub struct Worker {
    db: Pool,
}

#[serde_as]
#[derive(Clone, Default, Deserialize)]
struct OpenGraph {
    #[serde_as(deserialize_as = "DefaultOnError")]
    #[serde(rename = "og:description", default)]
    pub description: String,

    #[serde_as(deserialize_as = "DefaultOnError")]
    #[serde(rename = "og:site_name", default)]
    pub site_name: String,

    #[serde_as(deserialize_as = "DefaultOnError")]
    #[serde(rename = "og:title", default)]
    pub title: String,

    #[serde_as(deserialize_as = "DefaultOnError")]
    #[serde(rename = "og:url", default)]
    pub url: String,
}

impl Worker {
    #[instrument(skip_all)]
    pub async fn new(data_path: &Path) -> Result<Worker> {
        let cfg = Config::new(data_path.join("url-previewer.sqlite3"));
        let pool = cfg.create_pool(Runtime::Tokio1)?;
        let conn = pool.get().await?;
        conn.interact(|conn| {
            conn.execute_batch(
                "PRAGMA journal_mode = WAL;
PRAGMA optimize = 0x10002;
BEGIN TRANSACTION;
CREATE TABLE IF NOT EXISTS messages (
    id INTEGER PRIMARY KEY NOT NULL,
    room_id TEXT NOT NULL,
    event_id TEXT NOT NULL,
    response_id TEXT NOT NULL,
    UNIQUE(room_id, event_id)
);
CREATE INDEX IF NOT EXISTS idx_messages_room_event ON messages (room_id, event_id);
COMMIT;
PRAGMA optimize;
",
            )?;
            Ok::<_, Report>(())
        })
        .await
        .unwrap()?;
        Ok(Worker { db: pool })
    }

    #[instrument(skip_all)]
    pub async fn on_message(
        &self,
        room: Room,
        thread_id: Option<OwnedEventId>,
        original_event_id: OwnedEventId,
        urls: IndexSet<Url>,
    ) -> Result<Option<OwnedEventId>> {
        let stmt_query = "SELECT response_id FROM messages WHERE room_id = ? AND event_id = ?;";
        let stmt_insert =
            "INSERT OR REPLACE INTO messages (room_id, event_id, response_id) VALUES (?, ?, ?)";
        let conn = self.db.get().await?;

        let room_id_str = room.room_id().to_string();
        let original_event_id_str = original_event_id.to_string();
        let response_id = conn
            .interact(move |conn| {
                let mut stmt = conn.prepare_cached(stmt_query)?;
                Ok::<_, Report>(
                    stmt.query_row((room_id_str, original_event_id_str), |row| {
                        row.get::<_, String>(0)
                    })
                    .optional()?,
                )
            })
            .await
            .unwrap()?;

        let (response_id, is_edit) = if let Some(response_id) = response_id {
            (OwnedEventId::try_from(response_id)?, true)
        } else if urls.is_empty() {
            return Ok(None);
        } else {
            let relates_to = match thread_id {
                Some(thread_id) => Some(Relation::Thread(Thread::plain(
                    thread_id,
                    original_event_id.to_owned(),
                ))),
                _ => None,
            };
            let response = RoomMessageEventContentWithoutRelation::notice_html(
                "(Loading…)",
                format!(
                    "<blockquote><div class=\"m13253-url-preview-headline\"><a class=\"m13253-url-preview-backref\" href=\"https://matrix.to/#/{}/{}\">\u{1f517}\u{fe0f}</a> <span class=\"m13253-url-preview-loading\"><em>Loading…</em></span></div></blockquote>",
                    html_escape::attr(room.room_id().as_str()),
                    html_escape::attr(original_event_id.as_str())
                ),
            )
            .add_mentions(Mentions::new())
            .with_relation(relates_to);
            let response_id = room.send(response).await?.event_id;

            let room_id_str = room.room_id().to_string();
            let original_event_id_str = original_event_id.to_string();
            let response_id_str = response_id.to_string();

            conn.interact(move |conn| {
                let mut stmt = conn.prepare_cached(stmt_insert)?;
                stmt.execute((room_id_str, original_event_id_str, response_id_str))?;
                Ok::<_, Report>(())
            })
            .await
            .unwrap()?;

            (response_id, false)
        };

        tokio::spawn(Self::create_url_preview(
            room,
            original_event_id,
            response_id.clone(),
            is_edit,
            urls,
        ));

        Ok(Some(response_id))
    }

    #[instrument(skip_all)]
    pub async fn on_deletion(
        &self,
        room: Room,
        original_event_id: &EventId,
    ) -> Result<Option<OwnedEventId>> {
        let stmt_query = "SELECT response_id FROM messages WHERE room_id = ? AND event_id = ?;";
        let conn = self.db.get().await?;

        let room_id_str = room.room_id().to_string();
        let original_event_id_str = original_event_id.to_string();
        let response_id = conn
            .interact(move |conn| {
                let mut stmt = conn.prepare_cached(stmt_query)?;
                Ok::<_, Report>(
                    stmt.query_row((room_id_str, original_event_id_str), |row| {
                        row.get::<_, String>(0)
                    })
                    .optional()?,
                )
            })
            .await
            .unwrap()?;

        let response_id = if let Some(response_id) = response_id {
            OwnedEventId::try_from(response_id)?
        } else {
            return Ok(None);
        };

        let response_id_clone = response_id.clone();
        tokio::spawn(
            async move {
                if let Err(err) = room.redact(&response_id_clone, None, None).await {
                    error!("Failed to delete URL preview message: {err}");
                }
            }
            .in_current_span(),
        );

        Ok(Some(response_id))
    }

    #[instrument(skip_all)]
    async fn create_url_preview(
        room: Room,
        original_event_id: OwnedEventId,
        response_id: OwnedEventId,
        is_edit: bool,
        urls: IndexSet<Url>,
    ) {
        let mut reply_text = String::new();
        let mut reply_html = String::new();

        for url in urls.into_iter().take(MAX_URL_COUNTS_PER_MESSAGE) {
            info!("Fetching URL preview for: {url}");
            let request = get_media_preview::v1::Request::new(url.to_string());
            let response = match room
                .client()
                .send(request)
                .with_request_config(Some(room.client().request_config().clone().disable_retry()))
                .await
            {
                Ok(response) => response,
                Err(err) => {
                    error!("Failed to fetch URL preview for {url}: {err}");
                    continue;
                }
            };

            let Some(preview) = response.data.as_deref() else {
                continue;
            };
            info!("{preview}");
            let preview: OpenGraph = match serde_json::from_str(preview.get()) {
                Ok(preview) => preview,
                Err(err) => {
                    error!("Failed to parse URL preview for {url}: {err}");
                    continue;
                }
            };

            // Extract metadata from OpenGraph, while keeping length limited
            let mut bytes_remaining = MAX_RESPONSE_TEXT_BYTES;
            let title =
                Self::limit_text_length(Self::collapse_whitespace(&preview.title), bytes_remaining);
            bytes_remaining = bytes_remaining.saturating_sub(title.len());
            let site_name = Self::limit_text_length(
                Self::collapse_whitespace(&preview.site_name),
                bytes_remaining,
            );
            bytes_remaining = bytes_remaining.saturating_sub(title.len());
            let description = Self::limit_text_length(
                Self::collapse_whitespace(&preview.description),
                bytes_remaining,
            );
            let canonical_url = Url::parse(&preview.url)
                .ok()
                .filter(|url| url.as_str().len() <= SAFE_URL_LENGTH)
                .unwrap_or(url);

            if title.is_empty() {
                reply_html = format!(
                    "<blockquote><div class=\"m13253-url-preview-headline\"><a class=\"m13253-url-preview-backref\" href=\"https://matrix.to/#/{}/{}\">\u{1f517}\u{fe0f}</a> <em><a class=\"m13253-url-preview-empty-title\" href=\"{}\">No title</a></em>",
                    html_escape::attr(room.room_id().as_str()),
                    html_escape::attr(original_event_id.as_str()),
                    html_escape::attr(canonical_url.as_str())
                );
                reply_text = "(No title)".to_owned();
            } else {
                reply_html = format!(
                    "<blockquote><div class=\"m13253-url-preview-headline\"><a class=\"m13253-url-preview-backref\" href=\"https://matrix.to/#/{}/{}\">\u{1f517}\u{fe0f}</a> <strong><a class=\"m13253-url-preview-title\" href=\"{}\">{}</a></strong>",
                    html_escape::attr(room.room_id().as_str()),
                    html_escape::attr(original_event_id.as_str()),
                    html_escape::attr(canonical_url.as_str()),
                    html_escape::text(&title)
                );
                reply_text = title;
            }
            if !site_name.is_empty() {
                reply_text.push_str(" \u{2013} ");
                reply_text.push_str(&site_name);
                reply_html.push_str(" \u{2013} <span class=\"m13253-url-preview-site-name\">");
                reply_html.push_str(&html_escape::text(&site_name));
                reply_html.push_str("</span>");
            }
            reply_html.push_str("</div>");
            if !description.is_empty() {
                reply_text.push_str("\n> ");
                reply_text.push_str(&description);
                reply_html.push_str("<div class=\"m13253-url-preview-description\">");
                reply_html.push_str(&html_escape::text(&description));
                reply_html.push_str("</div>");
            }
            reply_html.push_str("</blockquote>");
            break;
        }

        if reply_text.is_empty() {
            if is_edit {
                return;
            }
            reply_text = "(URL preview is unavailable.)".to_string();
            reply_html = format!(
                "<blockquote><div class=\"m13253-url-preview-headline\"><a class=\"m13253-url-preview-backref\" href=\"https://matrix.to/#/{}/{}\">\u{1f517}\u{fe0f}</a> <span class=\"url-preview-error\"><em>URL preview is unavailable.</em></span></div></blockquote>",
                html_escape::attr(room.room_id().as_str()),
                html_escape::attr(original_event_id.as_str())
            );
        }

        let reply = RoomMessageEventContentWithoutRelation::notice_html(
            reply_text.clone(),
            reply_html.clone(),
        )
        .add_mentions(Mentions::new())
        .with_relation(Some(Relation::Replacement(Replacement::new(
            response_id,
            RoomMessageEventContentWithoutRelation::notice_html(reply_text, reply_html)
                .add_mentions(Mentions::new()),
        ))));
        if let Err(err) = room.send(reply).await {
            error!("Failed to send URL preview: {err}");
        }
    }

    fn collapse_whitespace(s: &str) -> String {
        // https://developer.mozilla.org/en-US/docs/Glossary/Whitespace
        static CONSECUTIVE_WHITESPACES: LazyLock<Regex> =
            LazyLock::new(|| Regex::new("[\t\n\x0c\r ]+").unwrap());
        CONSECUTIVE_WHITESPACES
            .replace_all(s, " ")
            .trim()
            .to_owned()
    }

    fn limit_text_length(mut s: String, max_bytes: usize) -> String {
        if s.len() <= max_bytes {
            return s;
        }
        for i in (0..max_bytes.saturating_sub(3)).rev() {
            if s.is_char_boundary(i) {
                s.drain(i..);
                if !s.ends_with("…") {
                    s.push_str("…");
                }
                return s;
            }
        }
        unreachable!();
    }
}
