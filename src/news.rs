//! News integration: RSS/Atom feed fetch, parse, dedup, and per-feed isolation
//! (Slice 3).
//!
//! The news section is populated from a config-driven list of RSS/Atom feeds.
//! Each feed is fetched and parsed independently so that a broken feed (network
//! failure, non-2xx status, or malformed XML) does not prevent the other feeds
//! from being delivered. The aggregated headlines are deduplicated by id and
//! items without a publish date are kept with an empty `published_at` (no
//! fabricated time). The section `status` reflects the worst feed outcome so
//! the frontend can render a "stale / unavailable" affordance rather than the
//! endpoint ever returning 500.
//!
//! This mirrors the provider pattern established in `weather.rs` (Slice 2):
//! a `*Cache` struct with an `RwLock<Option<Snapshot<T>>>`, a `Resolved<T>`
//! enum (`Fresh`/`Stale`/`Unavailable`) with `into_parts()`, and a
//! `build_*_section()` function. (News is a single aggregated provider, so it
//! reports one status and does not need weather's `worst_status` fold.)

use std::time::{Duration, Instant};

use quick_xml::events::Event;
use quick_xml::Reader;
use tokio::sync::RwLock;

use crate::api::{Headline, NewsSection, Status};
use crate::backoff::{retry_after_from_response, FetchError, RetryState};
use crate::config::{FeedConfig, NewsConfig};

/// A single feed item as parsed from RSS (`<item>`) or Atom (`<entry>`),
/// before it is normalized into the API's `Headline` shape.
#[derive(Debug, Default)]
struct FeedItem {
    title: String,
    link: String,
    guid: String,
    pub_date: String,
}

/// Normalize a parsed feed item into the API's `Headline` shape.
///
/// - `id` is the feed's `<guid>`/`<id>`, falling back to the `<link>`/`<link
///   href>`, falling back to a stable hash of the title. An item with no
///   title, link, or guid is malformed and is skipped (returns `None`).
/// - `source` is the configured feed label (or the feed host).
/// - `published_at` is the feed's `<pubDate>`/`<updated>` verbatim; when the
///   feed omits it we keep an empty string rather than fabricating a time.
fn build_headline(item: FeedItem, source: &str) -> Option<Headline> {
    let title = item.title.trim().to_string();
    if title.is_empty() && item.link.trim().is_empty() && item.guid.trim().is_empty() {
        // Nothing usable — a malformed item. Skip it.
        return None;
    }
    let id = if !item.guid.trim().is_empty() {
        item.guid.trim().to_string()
    } else if !item.link.trim().is_empty() {
        item.link.trim().to_string()
    } else {
        // Stable, collision-resistant id derived from the title so dedup still
        // works for feeds that omit guid/link.
        format!("title:{}", fnv1a(title.as_bytes()))
    };
    Some(Headline {
        id,
        source: source.to_string(),
        title,
        published_at: item.pub_date.trim().to_string(),
    })
}

/// A small, dependency-free 32-bit FNV-1a hash used only to derive a stable id
/// for items that lack a guid/link. Not cryptographic.
fn fnv1a(bytes: &[u8]) -> u32 {
    let mut hash: u32 = 0x811c_9dc5;
    for b in bytes {
        hash ^= *b as u32;
        hash = hash.wrapping_mul(0x0100_0193);
    }
    hash
}

/// Apply a captured text value to the current item's active field.
///
/// `title` is overwritten (the last text wins); the other fields keep the
/// first value seen (feeds may repeat `<link>`/`<id>` in different forms).
fn apply_field(item: &mut FeedItem, field: &str, value: String) {
    match field {
        "title" => item.title = value,
        "link" => {
            if item.link.is_empty() {
                item.link = value;
            }
        }
        "guid" => {
            if item.guid.is_empty() {
                item.guid = value;
            }
        }
        "pub_date" => {
            if item.pub_date.is_empty() {
                item.pub_date = value;
            }
        }
        _ => {}
    }
}

/// Parse an RSS 2.0 or Atom document into a list of headlines for the given
/// `source` label.
///
/// Malformed XML (a parse error) returns `Err`, which the caller treats as a
/// failed feed. Individual malformed *items* (no title/link/guid) are skipped
/// rather than failing the whole feed.
pub fn parse_feed(xml: &str, source: &str) -> Result<Vec<Headline>, String> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);

    let mut items: Vec<Headline> = Vec::new();
    let mut current: Option<FeedItem> = None;
    // Which field of the current item we are capturing text for.
    let mut in_field: Option<&'static str> = None;
    // Depth of open tags, used to detect malformed XML (unclosed tags) at EOF.
    let mut depth: u32 = 0;

    let mut buf = Vec::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(tag)) => {
                depth += 1;
                let local = tag.local_name();
                let name = local.as_ref();
                if name == b"item" || name == b"entry" {
                    current = Some(FeedItem::default());
                    in_field = None;
                } else if current.is_some() {
                    if name == b"title" {
                        in_field = Some("title");
                    } else if name == b"link" {
                        // Atom exposes the URL as a `href` attribute; RSS uses
                        // the element's text. Handle both.
                        if let Some(href) = tag
                            .attributes()
                            .flatten()
                            .find(|a| a.key.local_name().as_ref() == b"href")
                            .and_then(|a| a.unescape_value().ok())
                        {
                            if let Some(item) = current.as_mut() {
                                if item.link.is_empty() {
                                    item.link = href.to_string();
                                }
                            }
                        }
                        in_field = Some("link");
                    } else if name == b"guid" || name == b"id" {
                        in_field = Some("guid");
                    } else if name == b"pubDate" || name == b"updated" {
                        in_field = Some("pub_date");
                    } else {
                        in_field = None;
                    }
                }
            }
            Ok(Event::Text(text)) => {
                if let (Some(field), Some(item)) = (in_field, current.as_mut()) {
                    let s = text.unescape().unwrap_or_default().to_string();
                    apply_field(item, field, s);
                }
            }
            // CDATA carries the same content as Text (many feeds, e.g.
            // WordPress-generated ones like qubit.hu, wrap titles in CDATA).
            // Unlike Text, CDATA content is already literal (no entities), so
            // it is decoded to a string directly rather than unescaped.
            Ok(Event::CData(text)) => {
                if let (Some(field), Some(item)) = (in_field, current.as_mut()) {
                    let s = String::from_utf8_lossy(&text).to_string();
                    apply_field(item, field, s);
                }
            }
            Ok(Event::End(tag)) => {
                depth = depth.saturating_sub(1);
                let local = tag.local_name();
                let name = local.as_ref();
                if name == b"item" || name == b"entry" {
                    if let Some(item) = current.take() {
                        if let Some(headline) = build_headline(item, source) {
                            items.push(headline);
                        }
                    }
                    in_field = None;
                } else {
                    in_field = None;
                }
            }
            Ok(Event::Eof) => {
                if depth != 0 {
                    return Err(format!("malformed XML: {depth} unclosed tag(s)"));
                }
                break;
            }
            Ok(_) => {}
            Err(err) => return Err(format!("XML parse error: {err}")),
        }
        buf.clear();
    }
    Ok(items)
}

/// Deduplicate headlines by `id`, keeping the first occurrence and preserving
/// order.
pub fn dedup_headlines(items: Vec<Headline>) -> Vec<Headline> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::with_capacity(items.len());
    for item in items {
        if seen.insert(item.id.clone()) {
            out.push(item);
        }
    }
    out
}

/// The source label for a feed: the configured name, or the host of the feed
/// URL when the name is not set.
fn source_label(feed: &FeedConfig) -> String {
    if !feed.name.trim().is_empty() {
        return feed.name.trim().to_string();
    }
    host_of(&feed.url)
}

/// Extract the host from a URL string (no external dependency). Returns the
/// input unchanged if it has no recognizable host.
fn host_of(url: &str) -> String {
    let without_scheme = url
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(url);
    without_scheme
        .split('/')
        .next()
        .unwrap_or(url)
        .to_string()
}

/// Fetch and parse a single feed. Returns the parsed headlines on success.
///
/// Any failure — network error, non-2xx status, or malformed XML — is an
/// `Err`, which the caller isolates so it does not affect other feeds. A
/// non-2xx status is surfaced as a [`FetchError::Http`] carrying the
/// upstream's `Retry-After` hint (Slice 6).
async fn fetch_feed(
    client: &reqwest::Client,
    feed: &FeedConfig,
) -> Result<Vec<Headline>, FetchError> {
    let source = source_label(feed);
    let resp = client
        .get(&feed.url)
        .send()
        .await
        .map_err(FetchError::Network)?;
    if !resp.status().is_success() {
        return Err(FetchError::Http {
            status: resp.status(),
            retry_after: retry_after_from_response(&resp),
        });
    }
    let body = resp
        .text()
        .await
        .map_err(|e| FetchError::Other(format!("read body failed: {e}")))?;
    parse_feed(&body, &source).map_err(FetchError::Parse)
}

/// Fetch all configured feeds concurrently, isolating each feed's failures.
///
/// Returns `Ok((items, status))` when at least one feed succeeded:
/// - `Status::Fresh` when every feed succeeded,
/// - `Status::Stale` when some feeds succeeded and at least one failed.
///
/// Returns `Err` when there are no feeds configured or every feed failed, so
/// the caller can fall back to the last-good snapshot (or report unavailable).
/// When every feed failed, the returned [`FetchError`] carries the longest
/// `Retry-After` hint seen among the failures (Slice 6), so the next retry is
/// not scheduled sooner than the slowest upstream asked.
async fn fetch_all_feeds(
    client: &reqwest::Client,
    feeds: &[FeedConfig],
) -> Result<(Vec<Headline>, Status), FetchError> {
    if feeds.is_empty() {
        return Err(FetchError::Other("no feeds configured".to_string()));
    }

    let mut results: Vec<Option<Vec<Headline>>> = Vec::with_capacity(feeds.len());
    let mut failures = 0usize;
    // The longest Retry-After hint among the failed feeds (if any).
    let mut max_retry_after: Option<std::time::Duration> = None;

    // Fetch each feed independently. A failure in one feed is recorded but
    // never aborts the others (per-feed isolation).
    for feed in feeds {
        match fetch_feed(client, feed).await {
            Ok(items) => results.push(Some(items)),
            Err(err) => {
                failures += 1;
                if let Some(ra) = err.retry_after() {
                    max_retry_after = Some(match max_retry_after {
                        Some(cur) if cur >= ra => cur,
                        _ => ra,
                    });
                }
                eprintln!(
                    "magic-mirror: news feed {:?} failed: {}",
                    feed.url,
                    describe_fetch_error(&err)
                );
                results.push(None);
            }
        }
    }

    if failures == feeds.len() {
        return Err(FetchError::Http {
            status: reqwest::StatusCode::INTERNAL_SERVER_ERROR,
            retry_after: max_retry_after,
        });
    }

    // Aggregate in configured feed order, then dedup by id.
    let mut items: Vec<Headline> = Vec::new();
    for result in results {
        if let Some(mut feed_items) = result {
            items.append(&mut feed_items);
        }
    }
    let items = dedup_headlines(items);

    let status = if failures == 0 {
        Status::Fresh
    } else {
        Status::Stale
    };
    Ok((items, status))
}

/// A short, log-safe description of a fetch failure (never includes secrets —
/// the news feeds carry no credentials, but the message is kept generic so it
/// can be reused safely).
fn describe_fetch_error(err: &FetchError) -> String {
    match err {
        FetchError::Network(e) => format!("network: {e}"),
        FetchError::Http { status, .. } => format!("non-2xx status: {status}"),
        FetchError::Parse(msg) => format!("parse: {msg}"),
        FetchError::Other(msg) => msg.clone(),
    }
}

/// Fold the resolved headlines into the API's `NewsSection`.
///
/// `headlines` is `Some` when a reading is available (fresh or stale) and
/// `None` when there is no data at all (unavailable), in which case the
/// section degrades to an empty, well-shaped body so the endpoint never 500s.
pub fn build_news_section(
    now: String,
    status: Status,
    headlines: Option<Vec<Headline>>,
) -> NewsSection {
    match headlines {
        Some(items) => NewsSection {
            updated_at: now,
            status,
            items,
        },
        None => NewsSection {
            updated_at: now,
            status: Status::Unavailable,
            items: Vec::new(),
        },
    }
}

/// A last-known-good reading with the instant it was fetched.
#[derive(Debug, Clone)]
struct Snapshot<T> {
    fetched_at: Instant,
    value: T,
}

/// The outcome of resolving the news reading against its refresh interval.
#[derive(Debug)]
pub enum Resolved<T> {
    /// Fresh: fetched now, or served from a snapshot within the interval.
    Fresh(T),
    /// Stale: a refetch was attempted and partially/fully failed; serving the
    /// last-good value.
    Stale(T),
    /// Unavailable: no reading at all (never fetched, or fetch failed with no
    /// last-good to fall back to).
    Unavailable,
}

impl<T> Resolved<T> {
    /// The value, if any, plus the corresponding section status.
    pub fn into_parts(self) -> (Option<T>, Status) {
        match self {
            Resolved::Fresh(v) => (Some(v), Status::Fresh),
            Resolved::Stale(v) => (Some(v), Status::Stale),
            Resolved::Unavailable => (None, Status::Unavailable),
        }
    }
}

/// In-memory news snapshot, atomically replaced on each successful refresh
/// (architecture doc §Data). The aggregated headlines are cached as a single
/// snapshot so the independently-configured news refresh interval is honored
/// across polls (acceptance criterion: "the news refresh interval is
/// independently configurable").
#[derive(Debug, Default)]
pub struct NewsCache {
    headlines: RwLock<Option<Snapshot<Vec<Headline>>>>,
    /// Retry state (Slice 6): after a total fetch failure we skip refetches
    /// until the backoff cooldown elapses, serving the last-good snapshot as
    /// `Stale` in the meantime — so a failing upstream is not re-hit on every
    /// poll.
    retry: RwLock<RetryState>,
}

impl NewsCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// Seed the headlines snapshot from a persisted last-good value (Slice 6
    /// restart recovery). The snapshot is stamped with the current instant so
    /// it is immediately eligible to be served (as `Stale` if a refetch is
    /// attempted and fails, or `Fresh` if the refresh interval has not yet
    /// elapsed).
    pub async fn seed(&self, value: Vec<Headline>) {
        *self.headlines.write().await =
            Some(Snapshot { fetched_at: Instant::now(), value });
    }

    /// The last-good headlines, if any (for persistence).
    pub async fn last_good(&self) -> Option<Vec<Headline>> {
        self.headlines.read().await.as_ref().map(|s| s.value.clone())
    }

    /// Resolve the current headlines, honoring the news refresh interval.
    ///
    /// A snapshot younger than the interval is served as-is (`Fresh`).
    /// Otherwise a refetch is attempted: on success the snapshot is replaced
    /// and the value is `Fresh` (or `Stale` if some feeds failed); on total
    /// failure the last-good snapshot (if any) is served as `Stale`, else
    /// `Unavailable`.
    ///
    /// While the provider is in its backoff cooldown (a recent total failure)
    /// the refetch is skipped entirely and the last-good snapshot is served as
    /// `Stale` (Slice 6) — so a failing upstream is not re-hit on every poll.
    pub async fn resolve(
        &self,
        client: &reqwest::Client,
        cfg: &NewsConfig,
    ) -> Resolved<Vec<Headline>> {
        let interval = Duration::from_secs(cfg.news_refresh_secs);
        if let Some(snap) = self.headlines.read().await.as_ref() {
            if snap.fetched_at.elapsed() < interval {
                return Resolved::Fresh(snap.value.clone());
            }
        }
        // In backoff cooldown: skip the refetch and serve the last-good value.
        if self.retry.read().await.in_cooldown() {
            return match self.headlines.read().await.as_ref() {
                Some(snap) => Resolved::Stale(snap.value.clone()),
                None => Resolved::Unavailable,
            };
        }
        match fetch_all_feeds(client, &cfg.feeds).await {
            Ok((items, status)) => {
                self.retry.write().await.record_success();
                *self.headlines.write().await =
                    Some(Snapshot { fetched_at: Instant::now(), value: items.clone() });
                match status {
                    Status::Fresh => Resolved::Fresh(items),
                    _ => Resolved::Stale(items),
                }
            }
            Err(err) => {
                self.retry.write().await.record_failure(err.retry_after());
                match self.headlines.read().await.as_ref() {
                    Some(snap) => Resolved::Stale(snap.value.clone()),
                    None => Resolved::Unavailable,
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- parse_feed: dedup, missing-date, malformed -------------------------

    /// Two feeds sharing a guid collapse to a single headline after dedup.
    #[test]
    fn dedup_collapses_duplicate_guids() {
        let feed_a = r#"<?xml version="1.0"?>
<rss version="2.0"><channel>
  <item><title>Shared story</title><guid>http://a.example/1</guid><pubDate>Mon, 18 Oct 2026 12:00:00 GMT</pubDate></item>
  <item><title>Only in A</title><guid>http://a.example/2</guid></item>
</channel></rss>"#;
        let feed_b = r#"<?xml version="1.0"?>
<rss version="2.0"><channel>
  <item><title>Shared story (repost)</title><guid>http://a.example/1</guid><pubDate>Mon, 18 Oct 2026 12:05:00 GMT</pubDate></item>
</channel></rss>"#;

        let a = parse_feed(feed_a, "Feed A").expect("feed A parses");
        let b = parse_feed(feed_b, "Feed B").expect("feed B parses");
        let mut all = a;
        all.extend(b);
        let deduped = dedup_headlines(all);

        // "Shared story" appears once (first occurrence wins); "Only in A" kept.
        assert_eq!(deduped.len(), 2, "duplicate guid must collapse: {deduped:?}");
        assert!(deduped.iter().any(|h| h.id == "http://a.example/1"));
        assert!(deduped.iter().any(|h| h.id == "http://a.example/2"));
        assert_eq!(
            deduped.iter().filter(|h| h.id == "http://a.example/1").count(),
            1,
            "guid http://a.example/1 must appear exactly once"
        );
    }

    /// An item without a pubDate is kept with an empty published_at (no
    /// fabricated time), while a dated item keeps its date.
    #[test]
    fn missing_date_kept_with_empty_published_at() {
        let feed = r#"<?xml version="1.0"?>
<rss version="2.0"><channel>
  <item><title>Dated</title><guid>g1</guid><pubDate>Mon, 18 Oct 2026 12:00:00 GMT</pubDate></item>
  <item><title>Undated</title><guid>g2</guid></item>
</channel></rss>"#;
        let items = parse_feed(feed, "Feed").expect("parses");
        assert_eq!(items.len(), 2);

        let dated = items.iter().find(|h| h.id == "g1").expect("dated item present");
        assert_eq!(dated.published_at, "Mon, 18 Oct 2026 12:00:00 GMT");

        let undated = items.iter().find(|h| h.id == "g2").expect("undated item present");
        assert_eq!(undated.published_at, "", "missing date must not be fabricated");
    }

    /// A malformed item (no title, link, or guid) is skipped; well-formed
    /// items in the same feed are still delivered.
    #[test]
    fn malformed_item_is_skipped_not_fatal() {
        let feed = r#"<?xml version="1.0"?>
<rss version="2.0"><channel>
  <item></item>
  <item><title>Good item</title><guid>g1</guid></item>
</channel></rss>"#;
        let items = parse_feed(feed, "Feed").expect("parses");
        assert_eq!(items.len(), 1, "malformed item must be skipped: {items:?}");
        assert_eq!(items[0].title, "Good item");
    }

    /// Malformed XML (a parse error) is an Err, so the feed is isolated.
    #[test]
    fn malformed_xml_is_an_error() {
        let bad = "<rss><channel><item><title>unclosed";
        assert!(parse_feed(bad, "Feed").is_err(), "malformed XML must be an error");
    }

    /// Atom feeds parse: title, link href, id, and updated are captured.
    #[test]
    fn atom_feed_parses() {
        let feed = r#"<?xml version="1.0"?>
<feed xmlns="http://www.w3.org/2005/Atom">
  <entry>
    <title>Atom headline</title>
    <link href="http://atom.example/1"/>
    <id>atom:1</id>
    <updated>2026-10-18T12:00:00Z</updated>
  </entry>
  <entry>
    <title>Atom undated</title>
    <link href="http://atom.example/2"/>
    <id>atom:2</id>
  </entry>
</feed>"#;
        let items = parse_feed(feed, "Atom Feed").expect("atom parses");
        assert_eq!(items.len(), 2);
        let first = items.iter().find(|h| h.id == "atom:1").expect("atom:1 present");
        assert_eq!(first.title, "Atom headline");
        assert_eq!(first.source, "Atom Feed");
        assert_eq!(first.published_at, "2026-10-18T12:00:00Z");
        let second = items.iter().find(|h| h.id == "atom:2").expect("atom:2 present");
        assert_eq!(second.published_at, "");
    }

    /// Titles wrapped in CDATA sections (common in WordPress feeds, e.g.
    /// qubit.hu) must be captured, not dropped.
    #[test]
    fn cdata_title_is_captured() {
        let feed = r#"<?xml version="1.0" encoding="UTF-8"?>
<rss version="2.0"><channel>
  <item>
    <title><![CDATA[CDATA headline]]></title>
    <link>http://cdata.example/1</link>
    <pubDate>Fri, 23 Oct 2026 16:28:32 +0200</pubDate>
  </item>
</channel></rss>"#;
        let items = parse_feed(feed, "Feed").expect("parses");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].title, "CDATA headline", "CDATA title must not be dropped: {items:?}");
    }

    /// An item with no guid/link falls back to a stable title-derived id.
    #[test]
    fn id_falls_back_to_title_hash() {
        let feed = r#"<?xml version="1.0"?>
<rss version="2.0"><channel>
  <item><title>Only a title</title></item>
</channel></rss>"#;
        let items = parse_feed(feed, "Feed").expect("parses");
        assert_eq!(items.len(), 1);
        assert!(items[0].id.starts_with("title:"), "id should be title-derived: {:?}", items[0].id);
    }

    // --- build_news_section -------------------------------------------------

    /// Fresh headlines produce a Fresh section with the items.
    #[test]
    fn build_section_fresh() {
        let items = vec![Headline {
            id: "1".into(),
            source: "S".into(),
            title: "T".into(),
            published_at: "2026-10-18T12:00:00Z".into(),
        }];
        let section = build_news_section("now".into(), Status::Fresh, Some(items.clone()));
        assert_eq!(section.status, Status::Fresh);
        assert_eq!(section.items, items);
        assert_eq!(section.updated_at, "now");
    }

    /// No headlines (unavailable) yields an empty, Unavailable section.
    #[test]
    fn build_section_unavailable_is_empty() {
        let section = build_news_section("now".into(), Status::Unavailable, None);
        assert_eq!(section.status, Status::Unavailable);
        assert!(section.items.is_empty());
    }

    // --- Cache / refresh-interval behaviour (criterion: independently
    //     configurable refresh interval) -------------------------------------

    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    /// Start a minimal mock RSS server on a random localhost port.
    ///
    /// Returns the base URL plus a hit counter so tests can assert how many
    /// times the feed endpoint was hit.
    async fn start_mock_feed_server() -> (String, Arc<AtomicUsize>) {
        use axum::{routing::get, Router};

        let hits = Arc::new(AtomicUsize::new(0));
        let h = hits.clone();

        let feed_xml = r#"<?xml version="1.0"?>
<rss version="2.0"><channel>
  <item><title>Live headline</title><guid>live:1</guid><pubDate>Mon, 18 Oct 2026 12:00:00 GMT</pubDate></item>
</channel></rss>"#;

        let app = Router::new().route(
            "/feed",
            get(move |_: ()| {
                h.fetch_add(1, Ordering::SeqCst);
                async move { ([(axum::http::header::CONTENT_TYPE, "application/rss+xml")], feed_xml.to_string()) }
            }),
        );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock feed server");
        let addr = listener.local_addr().expect("local addr");
        tokio::spawn(async move {
            axum::serve(listener, app).await.expect("mock feed server ran");
        });

        (format!("http://{addr}"), hits)
    }

    fn news_cfg_pointing_at(base: &str, refresh_secs: u64) -> NewsConfig {
        NewsConfig {
            feeds: vec![FeedConfig {
                url: format!("{base}/feed"),
                name: "Mock Feed".into(),
            }],
            news_refresh_secs: refresh_secs,
        }
    }

    /// A reading within the refresh interval is served from cache: the second
    /// resolve does not hit the upstream feed again.
    #[tokio::test]
    async fn cache_serves_within_interval() {
        let (base, hits) = start_mock_feed_server().await;
        let cfg = news_cfg_pointing_at(&base, 3600); // 1 h interval
        let cache = NewsCache::new();
        let client = reqwest::Client::new();

        let r1 = cache.resolve(&client, &cfg).await;
        let r2 = cache.resolve(&client, &cfg).await;

        assert!(matches!(r1, Resolved::Fresh(_)));
        assert!(matches!(r2, Resolved::Fresh(_)));
        assert_eq!(hits.load(Ordering::SeqCst), 1, "second call must be cached");
    }

    /// An interval of zero forces a refetch every time (no caching).
    #[tokio::test]
    async fn zero_interval_always_refetches() {
        let (base, hits) = start_mock_feed_server().await;
        let cfg = news_cfg_pointing_at(&base, 0); // interval = 0
        let cache = NewsCache::new();
        let client = reqwest::Client::new();

        let _ = cache.resolve(&client, &cfg).await;
        let _ = cache.resolve(&client, &cfg).await;

        assert_eq!(hits.load(Ordering::SeqCst), 2, "zero interval must refetch");
    }

    // --- Per-feed isolation (criterion: a broken feed does not drop others) --

    /// Start a mock server with one healthy feed and one broken feed (500).
    async fn start_isolation_server() -> (String, Arc<AtomicUsize>) {
        use axum::{routing::get, Router};

        let healthy_hits = Arc::new(AtomicUsize::new(0));
        let hh = healthy_hits.clone();

        let healthy_xml = r#"<?xml version="1.0"?>
<rss version="2.0"><channel>
  <item><title>Healthy feed item</title><guid>healthy:1</guid><pubDate>Mon, 18 Oct 2026 12:00:00 GMT</pubDate></item>
</channel></rss>"#;

        let app = Router::new()
            .route(
                "/healthy",
                get(move |_: ()| {
                    hh.fetch_add(1, Ordering::SeqCst);
                    async move { ([(axum::http::header::CONTENT_TYPE, "application/rss+xml")], healthy_xml.to_string()) }
                }),
            )
            .route("/broken", get(|_: ()| async move { axum::http::StatusCode::INTERNAL_SERVER_ERROR }));

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind isolation server");
        let addr = listener.local_addr().expect("local addr");
        tokio::spawn(async move {
            axum::serve(listener, app).await.expect("isolation server ran");
        });

        (format!("http://{addr}"), healthy_hits)
    }

    /// A broken feed (500) does not prevent the healthy feed from being
    /// delivered; the section is Stale (partial) but still has the healthy
    /// item.
    #[tokio::test]
    async fn broken_feed_does_not_drop_healthy_feed() {
        let (base, healthy_hits) = start_isolation_server().await;
        let cfg = NewsConfig {
            feeds: vec![
                FeedConfig { url: format!("{base}/broken"), name: "Broken".into() },
                FeedConfig { url: format!("{base}/healthy"), name: "Healthy".into() },
            ],
            news_refresh_secs: 3600,
        };
        let cache = NewsCache::new();
        let client = reqwest::Client::new();

        let resolved = cache.resolve(&client, &cfg).await;
        let (items, status) = resolved.into_parts();

        assert_eq!(status, Status::Stale, "partial success must be Stale");
        let items = items.expect("healthy feed must still deliver items");
        assert_eq!(healthy_hits.load(Ordering::SeqCst), 1, "healthy feed must be fetched");
        assert!(
            items.iter().any(|h| h.id == "healthy:1"),
            "healthy feed item must be present: {items:?}"
        );
        assert!(
            !items.iter().any(|h| h.source == "Broken"),
            "broken feed must contribute no items"
        );
    }

    /// When every feed is broken, the section is Unavailable (no last-good).
    #[tokio::test]
    async fn all_feeds_broken_is_unavailable() {
        let (base, _hits) = start_isolation_server().await;
        let cfg = NewsConfig {
            feeds: vec![FeedConfig { url: format!("{base}/broken"), name: "Broken".into() }],
            news_refresh_secs: 3600,
        };
        let cache = NewsCache::new();
        let client = reqwest::Client::new();

        let resolved = cache.resolve(&client, &cfg).await;
        let (items, status) = resolved.into_parts();

        assert_eq!(status, Status::Unavailable);
        assert!(items.is_none());
    }

    /// A previously-good snapshot is served as Stale when a refetch fails
    /// (last-good retained).
    #[tokio::test]
    async fn last_good_retained_on_refetch_failure() {
        let (base, _hits) = start_isolation_server().await;
        let mut cfg = NewsConfig {
            feeds: vec![FeedConfig { url: format!("{base}/healthy"), name: "Healthy".into() }],
            news_refresh_secs: 3600,
        };
        let cache = NewsCache::new();
        let client = reqwest::Client::new();

        // First resolve succeeds and caches the snapshot.
        let first = cache.resolve(&client, &cfg).await;
        assert!(matches!(first, Resolved::Fresh(_)));

        // Point at a broken feed and force a refetch (zero interval).
        cfg.feeds[0].url = format!("{base}/broken");
        cfg.news_refresh_secs = 0;
        let second = cache.resolve(&client, &cfg).await;
        let (items, status) = second.into_parts();

        assert_eq!(status, Status::Stale, "last-good must be served as Stale");
        let items = items.expect("last-good items must be retained");
        assert!(items.iter().any(|h| h.id == "healthy:1"), "last-good item retained: {items:?}");
    }

    // --- Slice 6: restart recovery + backoff cooldown -----------------------

    /// Start a mock feed server that always fails (500), with a hit counter.
    async fn start_failing_feed_server() -> (String, Arc<AtomicUsize>) {
        use axum::{routing::get, Router};

        let hits = Arc::new(AtomicUsize::new(0));
        let h = hits.clone();

        let app = Router::new().route(
            "/feed",
            get(move |_: ()| {
                h.fetch_add(1, Ordering::SeqCst);
                async move { axum::http::StatusCode::INTERNAL_SERVER_ERROR }
            }),
        );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind failing feed server");
        let addr = listener.local_addr().expect("local addr");
        tokio::spawn(async move {
            axum::serve(listener, app).await.expect("failing feed server ran");
        });

        (format!("http://{addr}"), hits)
    }

    fn seeded_headline() -> Vec<Headline> {
        vec![Headline {
            id: "seeded:1".into(),
            source: "Seeded".into(),
            title: "Seeded headline".into(),
            published_at: "2026-10-18T12:00:00Z".into(),
        }]
    }

    /// Restart recovery: a seeded (persisted) last-good snapshot is served as
    /// `Stale` when the first refresh after restart fails.
    #[tokio::test]
    async fn restart_recovery_serves_last_good_as_stale() {
        let (base, _hits) = start_failing_feed_server().await;
        let cfg = news_cfg_pointing_at(&base, 0); // zero interval forces a refetch
        let cache = NewsCache::new();
        let client = reqwest::Client::new();

        // Simulate restart recovery: seed the last-good snapshot from disk.
        cache.seed(seeded_headline()).await;

        // The first refresh after restart fails; the seeded value is served as Stale.
        let resolved = cache.resolve(&client, &cfg).await;
        let (items, status) = resolved.into_parts();
        assert_eq!(status, Status::Stale, "seeded last-good must be served as Stale");
        let items = items.expect("seeded last-good must be retained");
        assert!(items.iter().any(|h| h.id == "seeded:1"), "seeded item retained: {items:?}");
    }

    /// Backoff cooldown: after a failure, the next resolve within the cooldown
    /// window does not hit the upstream.
    #[tokio::test]
    async fn backoff_cooldown_skips_refetch() {
        let (base, hits) = start_failing_feed_server().await;
        let cfg = news_cfg_pointing_at(&base, 0); // zero interval forces a refetch
        let cache = NewsCache::new();
        let client = reqwest::Client::new();

        // Seed last-good so the cooldown path serves Stale (not Unavailable).
        cache.seed(seeded_headline()).await;

        // First resolve: not in cooldown, attempts the (failing) fetch.
        let r1 = cache.resolve(&client, &cfg).await;
        assert!(matches!(r1, Resolved::Stale(_)));
        assert_eq!(hits.load(Ordering::SeqCst), 1, "first resolve must attempt the fetch");

        // Second resolve: now in cooldown, must NOT hit the upstream.
        let r2 = cache.resolve(&client, &cfg).await;
        assert!(matches!(r2, Resolved::Stale(_)));
        assert_eq!(hits.load(Ordering::SeqCst), 1, "cooldown must skip the refetch");
    }
}
