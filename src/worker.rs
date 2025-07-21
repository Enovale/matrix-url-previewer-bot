use std::borrow::Cow;
use std::str::FromStr;
use std::sync::{Arc, LazyLock};

use deadpool_sqlite::rusqlite::OptionalExtension;
use deadpool_sqlite::{Pool, Runtime};
use encoding_rs::Encoding;
use eyre::{Report, Result};
use indexmap::IndexSet;
use matrix_sdk::Room;
use matrix_sdk::ruma::events::Mentions;
use matrix_sdk::ruma::events::relation::{Replacement, Thread};
use matrix_sdk::ruma::events::room::message::{Relation, RoomMessageEventContentWithoutRelation};
use matrix_sdk::ruma::{EventId, OwnedEventId};
use mime::Mime;
use moka::future::{Cache, CacheBuilder};
use regex::Regex;
use scraper::{Html, Selector};
use tracing::{Instrument, debug, error, info, instrument, warn};
use url::Url;

use crate::common::{MAX_RESPONSE_TEXT_CHARS, MAX_URL_COUNTS_PER_MESSAGE, SAFE_URL_LENGTH};
use crate::{config, html_escape, limit};

pub struct Worker {
    cache: Cache<Url, Option<OpenGraph>>,
    config: Arc<config::Config>,
    db: Pool,
    reqwest_client: reqwest::Client,
    rewrite_url: Vec<(Regex, String)>,
}

#[derive(Clone, Debug)]
struct OpenGraph {
    pub description: String,
    pub site_name: String,
    pub title: String,
    pub url: String,
}

impl Worker {
    #[instrument(skip_all)]
    pub async fn new(config: Arc<config::Config>) -> Result<Arc<Worker>> {
        let cache = CacheBuilder::new(config.cache_entries)
            .time_to_live(config.cache_duration)
            .build();

        let db_config = deadpool_sqlite::Config::new(config.data_dir.join("url-previewer.sqlite3"));
        let db = db_config.create_pool(Runtime::Tokio1)?;
        let conn = db.get().await?;
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
COMMIT;
PRAGMA optimize;
",
            )?;
            Ok::<_, Report>(())
        })
        .await
        .unwrap()?;

        let mut reqwest_headers = reqwest::header::HeaderMap::new();
        reqwest_headers.insert(
            reqwest::header::ACCEPT_LANGUAGE,
            config.crawler_accept_language.parse()?,
        );
        let mut reqwest_builder = reqwest::ClientBuilder::new()
            .default_headers(reqwest_headers)
            .user_agent(&config.crawler_user_agent);
        if !config.crawler_proxy.is_empty() {
            reqwest_builder = reqwest_builder.proxy(reqwest::Proxy::all(&config.crawler_proxy)?);
        }
        let reqwest_client = reqwest_builder.build()?;

        let rewrite_url = config
            .rewrite_url
            .iter()
            .map(|[from, to]| Ok((Regex::new(from)?, to.clone())))
            .collect::<Result<Vec<_>>>()?;

        Ok(Arc::new(Worker {
            cache,
            config,
            db,
            reqwest_client,
            rewrite_url,
        }))
    }

    #[instrument(skip_all)]
    pub async fn on_message(
        self: Arc<Self>,
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

        // This is basically `room.matrix_to_event_permalink`, but can't fail.
        let original_event_link = room
            .room_id()
            .matrix_to_event_uri_via(
                original_event_id.clone(),
                room.route().await.unwrap_or_default(),
            )
            .to_string();

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
                    "<blockquote><div class=\"m13253-url-preview-headline\"><a class=\"m13253-url-preview-backref\" href=\"{}\">\u{23f3}\u{fe0f}</a> <span class=\"m13253-url-preview-loading\"><em>Loading…</em></span></div></blockquote>",
                    html_escape::attr(&original_event_link)
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

        tokio::spawn(self.create_url_preview(
            room,
            original_event_link,
            response_id.clone(),
            is_edit,
            urls,
        ));

        Ok(Some(response_id))
    }

    #[instrument(skip_all)]
    pub async fn on_deletion(
        self: Arc<Self>,
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
        self: Arc<Self>,
        room: Room,
        original_event_link: String,
        response_id: OwnedEventId,
        is_edit: bool,
        urls: IndexSet<Url>,
    ) {
        let mut reply_text = String::new();
        let mut reply_html = String::new();

        for mut url in urls.into_iter().take(MAX_URL_COUNTS_PER_MESSAGE) {
            info!("Fetching URL preview for: {url}");

            let mut url_str = Cow::from(url.as_str());
            for (from, to) in self.rewrite_url.iter() {
                match from.replace_all(&url_str, to) {
                    Cow::Borrowed(_) => (),
                    Cow::Owned(result) => {
                        debug!("URL rewrite: {url_str} => {from} => {result}");
                        url_str = result.into()
                    }
                }
            }
            if let Cow::Owned(url_str) = url_str {
                url = match Url::parse(&url_str) {
                    Ok(url) => url,
                    Err(err) => {
                        error!("Failed to parse the URL after rewrite: {err}");
                        continue;
                    }
                }
            }

            // Previously we used Synapse's URL preview API.
            //
            // let request = matrix_sdk::ruma::api::client::authenticated_media::get_media_preview::v1::Request::new(
            //     url.to_string()
            // );
            // let response = match room
            //     .client()
            //     .send(request)
            //     .with_request_config(Some(room.client().request_config().clone().disable_retry()))
            //     .await
            // {
            //     Ok(response) => response,
            //     Err(err) => {
            //         error!("Failed to fetch URL preview for {url}: {err}");
            //         continue;
            //     }
            // };
            // let Some(preview) = response.data.as_deref() else {
            //     continue;
            // };
            // info!("{preview}");
            // let preview: OpenGraph = match serde_json::from_str(preview.get()) {
            //     Ok(preview) => preview,
            //     Err(err) => {
            //         error!("Failed to parse URL preview for {url}: {err}");
            //         continue;
            //     }
            // };

            let Some(preview) = self
                .cache
                .get_with_by_ref(&url, self.clone().fetch_single_url_preview(url.clone()))
                .await
            else {
                warn!("URL has no preview.");
                continue;
            };
            info!("{preview:?}");

            // Extract metadata from OpenGraph, while keeping length limited
            let title = limit::length_in_chars(
                Self::collapse_whitespace(&preview.title),
                MAX_RESPONSE_TEXT_CHARS,
            );
            let site_name = limit::length_in_chars(
                Self::collapse_whitespace(&preview.site_name),
                MAX_RESPONSE_TEXT_CHARS,
            );
            let description = limit::length_in_chars(
                Self::collapse_whitespace(&preview.description),
                MAX_RESPONSE_TEXT_CHARS,
            );
            let canonical_url = Url::parse(&preview.url)
                .ok()
                .filter(|url| url.as_str().len() <= SAFE_URL_LENGTH)
                .unwrap_or(url);

            if title.is_empty() {
                reply_html = format!(
                    "<blockquote><div class=\"m13253-url-preview-headline\"><a class=\"m13253-url-preview-backref\" href=\"{}\">\u{1f517}\u{fe0f}</a> <em><a class=\"m13253-url-preview-empty-title\" href=\"{}\">No title</a></em>",
                    html_escape::attr(&original_event_link),
                    html_escape::attr(canonical_url.as_str())
                );
                reply_text = "(No title)".to_owned();
            } else {
                reply_html = format!(
                    "<blockquote><div class=\"m13253-url-preview-headline\"><a class=\"m13253-url-preview-backref\" href=\"{}\">\u{1f517}\u{fe0f}</a> <strong><a class=\"m13253-url-preview-title\" href=\"{}\">{}</a></strong>",
                    html_escape::attr(&original_event_link),
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
                "<blockquote><div class=\"m13253-url-preview-headline\"><a class=\"m13253-url-preview-backref\" href=\"{}\">\u{26a0}\u{fe0f}</a> <span class=\"m13253-url-preview-error\"><em>URL preview is unavailable.</em></span></div></blockquote>",
                html_escape::attr(&original_event_link)
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

    #[instrument(skip(self))]
    async fn fetch_single_url_preview(self: Arc<Self>, url: Url) -> Option<OpenGraph> {
        // Selectors
        static META_CHARSET: LazyLock<Selector> =
            LazyLock::new(|| Selector::parse("meta[charset]").unwrap());
        static META_HTTP_EQUIV_CONTENT_TYPE: LazyLock<Selector> =
            LazyLock::new(|| Selector::parse("meta[http-equiv=\"Content-Type\" i]").unwrap());
        static META_OG_DESCRIPTION: LazyLock<[Selector; 3]> = LazyLock::new(|| {
            [
                Selector::parse("meta[property=\"og:description\" i]").unwrap(),
                Selector::parse("meta[property=\"twitter:description\" i]").unwrap(),
                Selector::parse("meta[name=\"description\" i]").unwrap(),
            ]
        });
        static META_OG_SITE_NAME: LazyLock<Selector> =
            LazyLock::new(|| Selector::parse("meta[property=\"og:site_name\" i]").unwrap());
        static META_OG_TITLE: LazyLock<[Selector; 2]> = LazyLock::new(|| {
            [
                Selector::parse("meta[property=\"og:title\" i]").unwrap(),
                Selector::parse("meta[property=\"twitter:title\" i]").unwrap(),
            ]
        });
        static META_OG_TITLE_FALLBACK: LazyLock<[Selector; 4]> = LazyLock::new(|| {
            [
                Selector::parse("title").unwrap(),
                Selector::parse("h1").unwrap(),
                Selector::parse("h2").unwrap(),
                Selector::parse("h3").unwrap(),
            ]
        });
        static META_OG_URL: LazyLock<Selector> =
            LazyLock::new(|| Selector::parse("meta[property=\"og:url\" i]").unwrap());
        static META_OG_URL_FALLBACK: LazyLock<Selector> =
            LazyLock::new(|| Selector::parse("link[rel=\"canonical\" i]").unwrap());

        let timeout = tokio::time::sleep(self.config.crawler_timeout);
        tokio::pin!(timeout);

        // Send out the request
        let mut response = tokio::select! {
            _ = &mut timeout => {
                error!("Failed to fetch URL preview for {url}: Request timed out.");
                None
            },
            response = self.reqwest_client.get(url.clone()).send() => match response.and_then(|response| response.error_for_status()) {
                Ok(response) => Some(response),
                Err(err) => {
                    error!("Failed to fetch URL preview for {url}: {err}");
                    None
                }
            },
        }?;

        // Download the response
        let charset = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|content_type| {
                Encoding::for_label(
                    Mime::from_str(&String::from_utf8_lossy(content_type.as_bytes()))
                        .unwrap_or(mime::TEXT_HTML)
                        .get_param(mime::CHARSET)?
                        .as_str()
                        .as_bytes(),
                )
            })
            .unwrap_or(encoding_rs::UTF_8);
        let mut document = Vec::new();
        while document.len() < self.config.crawler_max_size {
            tokio::select! {
                _ = &mut timeout => {
                error!("Failed to fetch URL preview for {url}: Read timed out.");
                    break;
                },
                chunk = response.chunk() => match chunk {
                    Ok(Some(chunk)) => document.extend(chunk),
                    Ok(None) => break,
                    Err(err) => {
                        error!("Failed to fetch URL preview for {url}: {err}");
                        break;
                    }
                }
            };
        }
        document.truncate(self.config.crawler_max_size);

        // Determine the text encoding
        let mut dom = Html::parse_document(&encoding_rs::UTF_8.decode(&document).0);
        let charset = dom
            .select(&META_CHARSET)
            .filter_map(|element| Encoding::for_label(element.attr("charset")?.as_bytes()))
            .next()
            .or_else(|| {
                dom.select(&META_HTTP_EQUIV_CONTENT_TYPE)
                    .filter_map(|element| {
                        Encoding::for_label(
                            Mime::from_str(element.attr("content")?)
                                .ok()?
                                .get_param(mime::CHARSET)?
                                .as_str()
                                .as_bytes(),
                        )
                    })
                    .next()
            })
            .unwrap_or(charset);
        if charset != encoding_rs::UTF_8 {
            dom = Html::parse_document(&charset.decode(&document).0);
        }

        // Generate the output
        // Ref: https://github.com/element-hq/synapse/blob/v1.132.0/synapse/media/preview_html.py#L237
        Some(OpenGraph {
            description: META_OG_DESCRIPTION
                .iter()
                .flat_map(|selector| dom.select(selector))
                .filter_map(|element| element.attr("content"))
                .filter(|&content| !content.is_empty())
                .next()
                .unwrap_or_default()
                .to_owned(),
            site_name: dom
                .select(&META_OG_SITE_NAME)
                .filter_map(|element| element.attr("content"))
                .filter(|&content| !content.is_empty())
                .next()
                .unwrap_or_default()
                .to_owned(),
            title: META_OG_TITLE
                .iter()
                .flat_map(|selector| dom.select(selector))
                .filter_map(|element| element.attr("content"))
                .filter(|&content| !content.is_empty())
                .map(|content| content.to_owned())
                .next()
                .or_else(|| {
                    META_OG_TITLE_FALLBACK
                        .iter()
                        .flat_map(|selector| dom.select(selector))
                        .map(|element| element.text().collect::<String>())
                        .filter(|content| !content.is_empty())
                        .next()
                })
                .unwrap_or_default(),
            url: dom
                .select(&META_OG_URL)
                .filter_map(|element| element.attr("content"))
                .filter(|&content| !content.is_empty())
                .next()
                .or_else(|| {
                    dom.select(&META_OG_URL_FALLBACK)
                        .filter_map(|element| element.attr("href"))
                        .filter(|&content| !content.is_empty())
                        .next()
                })
                .unwrap_or_default()
                .to_owned(),
        })
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
}
