//! Magic Mirror — single Rust process serving the static frontend and the
//! `/api/v1` JSON API.
//!
//! Slice 1 (tracer bullet): the server boots, serves the authored static
//! frontend under `/` and `/assets/*`, and exposes the hardcoded
//! `GET /api/v1/dashboard` and `GET /api/v1/health` endpoints.

mod api;
mod backoff;
mod calendar;
mod config;
mod news;
mod persistence;
mod transit;
mod weather;

use std::net::SocketAddr;
use std::sync::Arc;

use axum::{
    body::Body,
    http::{header, Request},
    middleware,
    response::Response,
    routing::get,
    Router,
};
use tower_http::services::{ServeDir, ServeFile};
use tower_http::trace::TraceLayer;

use config::Config;

/// Minimal security headers applied to every response (spec §9).
async fn security_headers(req: Request<Body>, next: middleware::Next) -> Response {
    let mut res = next.run(req).await;
    let headers = res.headers_mut();
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        "nosniff".parse().expect("static header value"),
    );
    headers.insert(
        header::REFERRER_POLICY,
        "no-referrer".parse().expect("static header value"),
    );
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        "default-src 'self'; script-src 'self'; style-src 'self'; font-src 'self'; \
         img-src 'self' data:; connect-src 'self'"
            .parse()
            .expect("static header value"),
    );
    // The mirror is an always-on kiosk: without an explicit Cache-Control,
    // browsers heuristically cache assets (e.g. app.js) by Last-Modified age
    // and keep serving stale code after a deploy. `no-cache` forces
    // revalidation on every load (cheap: the server already sends
    // Last-Modified / ETag, so unchanged files are 304s).
    headers.insert(header::CACHE_CONTROL, "no-cache".parse().expect("static header value"));
    res
}

/// Build the full Axum router for the given configuration.
///
/// Extracted from `main` so the in-crate test module can drive the router
/// in-process via `tower::ServiceExt::oneshot` without binding a port.
///
/// Async because it performs Slice 6 restart recovery: it loads the persisted
/// last-good snapshot (if any) and seeds the in-memory caches so the first
/// dashboard render already has data — before the first network refresh
/// completes. A missing or malformed snapshot file is not fatal.
pub async fn build_router(cfg: Config) -> Router {
    let static_dir = cfg.static_dir.clone();
    let weather_cache = Arc::new(weather::WeatherCache::new());
    let news_cache = Arc::new(news::NewsCache::new());
    let calendar_cache = Arc::new(calendar::CalendarCache::new());
    let transit_cache = Arc::new(transit::TransitCache::new());

    // Slice 6 restart recovery: seed the caches from the persisted last-good
    // snapshot so a restart does not blank the screen. Each section is seeded
    // independently (a section that never succeeded is simply absent).
    if let Some(snap) = persistence::load(std::path::Path::new(&cfg.snapshot_path)) {
        if let Some(w) = &snap.weather {
            if let Some(forecast) = &w.forecast {
                weather_cache.seed_forecast(forecast.clone()).await;
            }
            if let Some(aqi) = w.aqi {
                weather_cache.seed_aqi(aqi).await;
            }
        }
        if let Some(news) = &snap.news {
            news_cache.seed(news.clone()).await;
        }
        if let Some(calendar) = &snap.calendar {
            calendar_cache.seed(calendar.clone()).await;
            calendar_cache.seed_calendars(snap.calendars.clone()).await;
        }
        if let Some(transit) = &snap.transit {
            transit_cache.seed(transit.clone()).await;
        }
    }

    let state = api::AppState {
        config: cfg,
        weather_cache,
        news_cache,
        calendar_cache,
        transit_cache,
    };

    Router::new()
        .route("/api/v1/dashboard", get(api::dashboard))
        .route("/api/v1/health", get(api::health))
        .route("/api/v1/calendars", get(api::calendars))
        .route_service("/", ServeFile::new(format!("{static_dir}/index.html")))
        .nest_service(
            "/assets",
            ServeDir::new(format!("{static_dir}/assets")),
        )
        .layer(middleware::from_fn(security_headers))
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

fn bind_addr(cfg: &Config) -> SocketAddr {
    // `Config::load` validates the bind address before we get here, so this
    // parse cannot fail (an invalid address is rejected at boot, Slice 7).
    cfg.server
        .bind
        .parse()
        .expect("bind address validated by Config::load")
}

#[tokio::main]
async fn main() -> std::io::Result<()> {
    // Slice 7: fail fast on invalid configuration. A missing config file is
    // fine (defaults apply), but a malformed file or invalid value is an error:
    // print an actionable message and exit non-zero so the operator (or systemd)
    // sees the problem immediately instead of the mirror running on wrong data.
    let cfg = match Config::load() {
        Ok(cfg) => cfg,
        Err(err) => {
            eprintln!("magic-mirror: configuration error: {err}");
            std::process::exit(1);
        }
    };
    let bind = bind_addr(&cfg);
    let app = build_router(cfg).await;

    let listener = tokio::net::TcpListener::bind(bind).await?;
    println!("magic-mirror: listening on http://{bind}");

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    println!("magic-mirror: stopped");
    Ok(())
}

async fn shutdown_signal() {
    if let Err(err) = tokio::signal::ctrl_c().await {
        eprintln!("magic-mirror: failed to install Ctrl-C handler: {err}");
        return;
    }
    println!("\nmagic-mirror: received shutdown signal");
}

#[cfg(test)]
mod tests {
    //! In-process endpoint verification.
    //!
    //! These tests drive the real router (including the security-header
    //! middleware and the static-file services) through
    //! `tower::ServiceExt::oneshot`, so no port is bound and no live server
    //! process is required.

    use std::path::Path;

    use axum::body::Body;
    use axum::http::{header, Request};
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    use super::build_router;
    use crate::config::Config;

    /// A config pointing at the repository's real `static/` directory so the
    /// static-file services resolve the same files a production run would.
    ///
    /// The snapshot path is a unique temp file per test process so the
    /// in-process tests never pollute the repository (or each other) with
    /// persisted last-good data.
    fn test_config() -> Config {
        let mut cfg = Config::default();
        cfg.static_dir = format!("{}/static", env!("CARGO_MANIFEST_DIR"));
        static COUNTER: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        cfg.snapshot_path = std::env::temp_dir()
            .join(format!("mm-test-snapshot-{}-{}.json", std::process::id(), n))
            .to_string_lossy()
            .into_owned();
        cfg
    }

    /// Issue a request against the router and collect the full response.
    async fn call(
        method: &str,
        uri: &str,
    ) -> (axum::http::StatusCode, axum::http::HeaderMap, String) {
        let app = build_router(test_config()).await;
        let req = Request::builder()
            .method(method)
            .uri(uri)
            .body(Body::empty())
            .expect("valid request");
        let res = app.oneshot(req).await.expect("router responded");
        let status = res.status();
        let headers = res.headers().clone();
        let body = res.into_body().collect().await.expect("body collected");
        let bytes = body.to_bytes();
        let text = String::from_utf8_lossy(&bytes).into_owned();
        (status, headers, text)
    }

    #[tokio::test]
    async fn dashboard_returns_spec_shape() {
        let (status, headers, body) = call("GET", "/api/v1/dashboard").await;
        assert_eq!(status, axum::http::StatusCode::OK);
        assert!(headers
            .get(header::CONTENT_TYPE)
            .map(|v| v.to_str().unwrap_or(""))
            .unwrap_or("")
            .starts_with("application/json"));

        let v: serde_json::Value = serde_json::from_str(&body).expect("valid JSON");

        // Top-level keys required by spec §5.
        for key in [
            "generated_at",
            "timezone",
            "calendar",
            "weather",
            "transit",
            "news",
        ] {
            assert!(v.get(key).is_some(), "missing top-level key: {key}");
        }

        // Calendar section.
        let cal = &v["calendar"];
        assert_eq!(cal["status"], "fresh");
        assert!(cal["events"].as_array().map(|a| !a.is_empty()).unwrap_or(false));
        let ev = &cal["events"][0];
        for key in ["id", "title", "start", "end", "all_day"] {
            assert!(ev.get(key).is_some(), "event missing key: {key}");
        }

        // Weather section. Slice 2 fetches live from Open-Meteo, so the status
        // is "fresh" when the network is reachable and degrades to "stale" /
        // "unavailable" (with mock fallback values) when it is not. The fields
        // must always be present and well-typed either way.
        let wx = &v["weather"];
        let wx_status = wx["status"].as_str().unwrap_or("");
        assert!(
            matches!(wx_status, "fresh" | "stale" | "unavailable"),
            "unexpected weather status: {wx_status:?}"
        );
        assert!(wx["temperature_c"].as_f64().is_some());
        assert!(wx["condition_code"].as_i64().is_some());
        assert!(wx["pressure_hpa"].as_i64().is_some());
        assert!(wx["european_aqi"].as_i64().is_some());

        // Transit section.
        let tr = &v["transit"];
        assert_eq!(tr["status"], "fresh");
        assert!(tr["departures"].as_array().map(|a| !a.is_empty()).unwrap_or(false));
        let dep = &tr["departures"][0];
        for key in ["stop_id", "route", "destination", "scheduled_time", "realtime", "delay_seconds"] {
            assert!(dep.get(key).is_some(), "departure missing key: {key}");
        }

        // News section. Slice 3 fetches live from the configured RSS/Atom
        // feeds, so the status is "fresh" when feeds are configured and
        // reachable, and degrades to "stale" / "unavailable" (with an empty
        // items array) when no feeds are configured or they are unreachable.
        // The fields must always be present and well-typed either way.
        let ns = &v["news"];
        let ns_status = ns["status"].as_str().unwrap_or("");
        assert!(
            matches!(ns_status, "fresh" | "stale" | "unavailable"),
            "unexpected news status: {ns_status:?}"
        );
        assert!(ns["items"].is_array(), "news items must be an array");
        // When items are present they must carry the spec §5 keys.
        if let Some(items) = ns["items"].as_array() {
            for item in items {
                for key in ["id", "source", "title", "published_at"] {
                    assert!(item.get(key).is_some(), "headline missing key: {key}");
                }
            }
        }
    }

    #[tokio::test]
    async fn calendar_list_is_empty_before_discovery() {
        let (status, _, body) = call("GET", "/api/v1/calendars").await;
        assert_eq!(status, axum::http::StatusCode::OK);
        assert_eq!(body, "[]");
    }

    #[tokio::test]
    async fn health_returns_ok() {
        let (status, _headers, body) = call("GET", "/api/v1/health").await;
        assert_eq!(status, axum::http::StatusCode::OK);
        let v: serde_json::Value = serde_json::from_str(&body).expect("valid JSON");
        assert_eq!(v["status"], "ok");
        assert_eq!(v["providers"]["calendar"], "fresh");
        assert_eq!(v["providers"]["weather"], "fresh");
        assert_eq!(v["providers"]["transit"], "fresh");
        assert_eq!(v["providers"]["news"], "fresh");
    }

    #[tokio::test]
    async fn root_serves_authored_html_without_cdn() {
        let (status, headers, body) = call("GET", "/").await;
        assert_eq!(status, axum::http::StatusCode::OK);
        assert!(headers
            .get(header::CONTENT_TYPE)
            .map(|v| v.to_str().unwrap_or(""))
            .unwrap_or("")
            .starts_with("text/html"));

        // No external CDN references (spec §6: self-hosted assets only).
        assert!(!body.contains("cdn.tailwindcss.com"), "Tailwind CDN found");
        assert!(!body.contains("fonts.googleapis.com"), "Google Fonts CDN found");
        assert!(!body.contains("fonts.gstatic.com"), "Google Fonts CDN found");
        assert!(!body.contains("unpkg.com"), "unpkg CDN found");
        assert!(!body.contains("jsdelivr.net"), "jsDelivr CDN found");

        // References the authored assets.
        assert!(body.contains("/assets/style.css"), "missing style.css reference");
        assert!(body.contains("/assets/app.js"), "missing app.js reference");
    }

    #[tokio::test]
    async fn assets_serve_css_and_fonts_with_correct_content_types() {
        // CSS.
        let (status, headers, _body) = call("GET", "/assets/style.css").await;
        assert_eq!(status, axum::http::StatusCode::OK);
        assert!(headers
            .get(header::CONTENT_TYPE)
            .map(|v| v.to_str().unwrap_or(""))
            .unwrap_or("")
            .starts_with("text/css"));

        // A font file (Chivo variable, latin subset).
        let font_path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("static/assets/fonts");
        let font_file = std::fs::read_dir(&font_path)
            .expect("fonts dir exists")
            .flatten()
            .find(|e| e.path().extension().map(|x| x == "woff2").unwrap_or(false))
            .expect("at least one woff2 font")
            .file_name()
            .to_string_lossy()
            .into_owned();
        let uri = format!("/assets/fonts/{font_file}");
        let (status, headers, _body) = call("GET", &uri).await;
        assert_eq!(status, axum::http::StatusCode::OK);
        assert!(headers
            .get(header::CONTENT_TYPE)
            .map(|v| v.to_str().unwrap_or(""))
            .unwrap_or("")
            .starts_with("font/woff2"));
    }

    #[tokio::test]
    async fn security_headers_present_on_all_responses() {
        for uri in ["/", "/api/v1/dashboard", "/api/v1/health", "/assets/style.css"] {
            let (_status, headers, _body) = call("GET", uri).await;
            assert_eq!(
                headers.get(header::X_CONTENT_TYPE_OPTIONS).map(|v| v.to_str().unwrap_or("")),
                Some("nosniff"),
                "missing X-Content-Type-Options on {uri}"
            );
            assert!(
                headers.get(header::CONTENT_SECURITY_POLICY).is_some(),
                "missing CSP on {uri}"
            );
            assert!(
                headers.get(header::REFERRER_POLICY).is_some(),
                "missing Referrer-Policy on {uri}"
            );
        }
    }
}
