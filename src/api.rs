//! HTTP API: the combined dashboard payload and the health endpoint.
//!
//! Slice 1 returns fully-hardcoded mock data shaped exactly like spec §5.
//! Later slices replace the mock builders with real provider adapters while
//! keeping these response types (the frontend contract) stable.

use axum::{
    extract::State,
    http::header,
    response::IntoResponse,
    Json,
};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::calendar::CalendarCache;
use crate::config::Config;
use crate::news::NewsCache;
use crate::persistence::{self, DashboardSnapshot, WeatherData};
use crate::transit::TransitCache;
use crate::weather::WeatherCache;

/// Shared application state for the Axum router: the loaded configuration plus
/// the in-memory weather / air-quality snapshot cache (Slice 2), the news
/// snapshot cache (Slice 3), and the calendar snapshot cache (Slice 5). The
/// caches are shared across requests (via `Arc`) so the independently-configured
/// refresh intervals are honored across polls.
#[derive(Clone)]
pub struct AppState {
    pub config: Config,
    pub weather_cache: Arc<WeatherCache>,
    pub news_cache: Arc<NewsCache>,
    pub calendar_cache: Arc<CalendarCache>,
    pub transit_cache: Arc<TransitCache>,
}

/// Provider freshness, as defined in spec §5.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Fresh,
    Stale,
    Unavailable,
}

/// The combined dashboard payload returned by `GET /api/v1/dashboard`.
#[derive(Debug, Serialize)]
pub struct Dashboard {
    pub generated_at: String,
    pub timezone: String,
    pub calendar: CalendarSection,
    pub weather: WeatherSection,
    pub transit: TransitSection,
    pub news: NewsSection,
}

#[derive(Debug, Serialize)]
pub struct CalendarSection {
    pub updated_at: String,
    pub status: Status,
    pub events: Vec<Event>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Event {
    pub id: String,
    pub title: String,
    pub start: String,
    pub end: String,
    pub all_day: bool,
    pub location: Option<String>,
    pub description: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct WeatherSection {
    pub updated_at: String,
    pub status: Status,
    pub temperature_c: f64,
    pub condition_code: i32,
    pub pressure_hpa: i32,
    pub european_aqi: i32,
}

#[derive(Debug, Serialize)]
pub struct TransitSection {
    pub updated_at: String,
    pub status: Status,
    pub departures: Vec<Departure>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Departure {
    pub stop_id: String,
    pub route: String,
    pub destination: String,
    pub scheduled_time: String,
    pub predicted_time: Option<String>,
    pub realtime: bool,
    pub delay_seconds: i64,
    pub platform: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct NewsSection {
    pub updated_at: String,
    pub status: Status,
    pub items: Vec<Headline>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Headline {
    pub id: String,
    pub source: String,
    pub title: String,
    pub published_at: String,
}

/// Minimal health payload: overall status plus per-provider status.
#[derive(Debug, Serialize)]
pub struct Health {
    pub status: String,
    pub providers: ProviderHealth,
}

#[derive(Debug, Serialize)]
pub struct ProviderHealth {
    pub calendar: Status,
    pub weather: Status,
    pub transit: Status,
    pub news: Status,
}

/// Current UTC time formatted as an RFC 3339 `...Z` timestamp (second
/// precision), matching the spec §5 examples.
fn now_utc() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

/// Build the hardcoded mock dashboard payload.
///
/// `generated_at` and every section's `updated_at` reflect the real current
/// time so the frontend's "age" display is meaningful; the section *content*
/// (events, weather values, departures, headlines) is fixed mock data.
fn mock_dashboard(cfg: &Config) -> Dashboard {
    let now = now_utc();

    Dashboard {
        generated_at: now.clone(),
        timezone: cfg.display.timezone.clone(),
        calendar: CalendarSection {
            updated_at: now.clone(),
            status: Status::Fresh,
            events: vec![
                Event {
                    id: "mock:cal:studio-session".into(),
                    title: "Studio session".into(),
                    start: "2026-10-18T14:00:00+02:00".into(),
                    end: "2026-10-18T15:30:00+02:00".into(),
                    all_day: false,
                    location: Some("Studio A".into()),
                    description: Some("Recording".into()),
                },
                Event {
                    id: "mock:cal:calibration".into(),
                    title: "Acoustic resonance calibration".into(),
                    start: "2026-10-18T15:30:00+02:00".into(),
                    end: "2026-10-18T16:15:00+02:00".into(),
                    all_day: false,
                    location: Some("Main soundstage".into()),
                    description: Some("Sub-frequency pink noise array".into()),
                },
                Event {
                    id: "mock:cal:film-review".into(),
                    title: "Film print review".into(),
                    start: "2026-10-18T17:15:00+02:00".into(),
                    end: "2026-10-18T18:00:00+02:00".into(),
                    all_day: false,
                    location: Some("Projection lab".into()),
                    description: None,
                },
                Event {
                    id: "mock:cal:sound-design".into(),
                    title: "Sound design session".into(),
                    start: "2026-10-18T19:00:00+02:00".into(),
                    end: "2026-10-18T20:30:00+02:00".into(),
                    all_day: false,
                    location: Some("Synth suite 02".into()),
                    description: Some("Industrial drone".into()),
                },
                Event {
                    id: "mock:cal:telemetry".into(),
                    title: "Low-light telemetry sync".into(),
                    start: "2026-10-18T21:30:00+02:00".into(),
                    end: "2026-10-18T22:00:00+02:00".into(),
                    all_day: false,
                    location: None,
                    description: None,
                },
            ],
        },
        weather: WeatherSection {
            updated_at: now.clone(),
            status: Status::Fresh,
            temperature_c: 11.2,
            condition_code: 45,
            pressure_hpa: 1014,
            european_aqi: 22,
        },
        transit: TransitSection {
            updated_at: now.clone(),
            status: Status::Fresh,
            departures: vec![
                Departure {
                    stop_id: "BKK_MOCK_TRAM_4".into(),
                    route: "4".into(),
                    destination: "Kálvin tér".into(),
                    scheduled_time: "2026-10-18T16:55:00+02:00".into(),
                    predicted_time: Some("2026-10-18T16:55:00+02:00".into()),
                    realtime: true,
                    delay_seconds: 0,
                    platform: Some("2".into()),
                },
                Departure {
                    stop_id: "BKK_MOCK_S8".into(),
                    route: "S8".into(),
                    destination: "Keleti pályaudvar".into(),
                    scheduled_time: "2026-10-18T17:12:00+02:00".into(),
                    predicted_time: None,
                    realtime: false,
                    delay_seconds: 0,
                    platform: Some("3".into()),
                },
                Departure {
                    stop_id: "BKK_MOCK_33".into(),
                    route: "33".into(),
                    destination: "Fővám tér".into(),
                    scheduled_time: "2026-10-18T17:18:00+02:00".into(),
                    predicted_time: None,
                    realtime: false,
                    delay_seconds: 0,
                    platform: None,
                },
            ],
        },
        news: NewsSection {
            updated_at: now.clone(),
            status: Status::Fresh,
            items: vec![
                Headline {
                    id: "mock:feed1:item1".into(),
                    source: "Example feed".into(),
                    title: "Atlantic subsea cable reports transient latency shift at low tide.".into(),
                    published_at: "2026-10-18T12:42:00Z".into(),
                },
                Headline {
                    id: "mock:feed1:item2".into(),
                    source: "Example feed".into(),
                    title: "Analog silicon fabs resume legacy vacuum-tube test cycles.".into(),
                    published_at: "2026-10-18T12:30:00Z".into(),
                },
            ],
        },
    }
}

/// `GET /api/v1/dashboard` — the combined current view.
///
/// Calendar and transit are still mock data (later slices). Weather and
/// air-quality are resolved from Open-Meteo (Slice 2) using the config-driven
/// location, and news is resolved from the configured RSS/Atom feeds (Slice
/// 3). Each honors its independently-configured refresh interval via the
/// shared snapshot caches. The providers are resolved concurrently and may
/// each fail independently; on failure a section degrades to its last-good
/// snapshot (or the mock fallback) with a `stale` / `unavailable` status
/// rather than the endpoint returning 500.
pub async fn dashboard(State(state): State<AppState>) -> impl IntoResponse {
    let cfg = &state.config;
    let mut body = mock_dashboard(cfg);
    let now = now_utc();

    // Bounded so a slow / blackholed network can't hang the dashboard (or the
    // in-process tests). 5 s is generous for a multi-feed poll.
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .expect("valid reqwest client");
    let weather_cfg = cfg.weather.clone();
    let news_cfg = cfg.news.clone();

    // Calendar (Slice 5): resolve from CalDAV only when enabled. The CalDAV
    // client carries the app password (an env-only secret) and is separate
    // from the weather/news reqwest client.
    let calendar_cfg = cfg.calendar.clone();
    let caldav_client = if calendar_cfg.enabled {
        Some(crate::calendar::CalDavClient::new(
            calendar_cfg.username.clone(),
            calendar_cfg.app_password.clone(),
            std::time::Duration::from_secs(5),
        ))
    } else {
        None
    };

    // Transit (Slice 4): resolve from BKK FUTÁR only when enabled. The API key
    // (an env-only secret) travels as a query parameter on the outbound request
    // and never appears in the response or logs.
    let transit_cfg = cfg.transit.clone();

    // Resolve forecast, AQI, news, calendar, and transit against their
    // independent refresh intervals, concurrently.
    let (forecast_res, aqi_res, news_res, calendar_res, transit_res) = tokio::join!(
        state.weather_cache.resolve_forecast(&client, &weather_cfg),
        state.weather_cache.resolve_aqi(&client, &weather_cfg),
        state.news_cache.resolve(&client, &news_cfg),
        async {
            if let Some(c) = caldav_client {
                Some(state.calendar_cache.resolve(&c, &calendar_cfg, chrono::Utc::now()).await)
            } else {
                None
            }
        },
        async {
            if transit_cfg.enabled {
                Some(state.transit_cache.resolve(&client, &transit_cfg).await)
            } else {
                None
            }
        },
    );
    let (forecast, forecast_status) = forecast_res.into_parts();
    let (aqi, aqi_status) = aqi_res.into_parts();
    let (headlines, news_status) = news_res.into_parts();

    let mut section = crate::weather::build_weather_section(now.clone(), forecast, aqi, body.weather);
    // The section status reflects the worst provider outcome (a stale forecast
    // with a fresh AQI is still a stale section).
    section.status = crate::weather::worst_status(forecast_status, aqi_status);
    body.weather = section;

    // News: fold the resolved headlines into the section. `headlines` is
    // `None` when there is no data (unavailable), in which case the section
    // degrades to an empty, well-shaped body.
    body.news = crate::news::build_news_section(now.clone(), news_status, headlines);

    // Calendar: fold the resolved events into the section. When the calendar
    // is disabled (`calendar_res` is `None`) the section keeps its mock body;
    // when enabled but there is no data the section degrades to the mock body
    // with `Status::Unavailable`.
    if let Some(cal_res) = calendar_res {
        let (events, cal_status) = cal_res.into_parts();
        body.calendar =
            crate::calendar::build_calendar_section(now.clone(), cal_status, events, body.calendar);
        crate::calendar::filter_display_events(
            &mut body.calendar.events,
            chrono::Utc::now(),
            calendar_cfg.lookahead_days,
        );
    }

    // Transit: fold the resolved departures into the section. When the transit
    // section is disabled (`transit_res` is `None`) the section keeps its mock
    // body; when enabled but there is no data the section degrades to the mock
    // body with `Status::Unavailable`.
    if let Some(tr_res) = transit_res {
        let (departures, tr_status) = tr_res.into_parts();
        body.transit =
            crate::transit::build_transit_section(now.clone(), tr_status, departures, body.transit);
    }

    // Slice 6: persist the last-good snapshot so a restart can restore it
    // before the first refresh. We read the caches' last-good values (not the
    // just-resolved ones) so a section that failed this poll still persists its
    // previous good data. We only write when at least one section has data, so
    // a fully-empty poll never wipes a previously-good file. A persistence
    // failure is logged but not fatal — the dashboard still serves.
    let last_forecast = state.weather_cache.last_good_forecast().await;
    let last_aqi = state.weather_cache.last_good_aqi().await;
    let last_news = state.news_cache.last_good().await;
    let last_calendar = state.calendar_cache.last_good().await;
    let last_transit = state.transit_cache.last_good().await;

    let weather_data = match (last_forecast, last_aqi) {
        (Some(f), Some(a)) => Some(WeatherData { forecast: Some(f), aqi: Some(a) }),
        (Some(f), None) => Some(WeatherData { forecast: Some(f), aqi: None }),
        (None, Some(a)) => Some(WeatherData { forecast: None, aqi: Some(a) }),
        (None, None) => None,
    };

    if weather_data.is_some()
        || last_news.is_some()
        || last_calendar.is_some()
        || last_transit.is_some()
    {
        let snap = DashboardSnapshot {
            saved_at: now.clone(),
            weather: weather_data,
            news: last_news,
            calendar: last_calendar,
            calendars: state.calendar_cache.calendars().await,
            transit: last_transit,
        };
        if let Err(err) = persistence::save(std::path::Path::new(&cfg.snapshot_path), &snap) {
            eprintln!("magic-mirror: failed to persist last-good snapshot: {err}");
        }
    }

    // Operator observability for the always-on dashboard: log the resolved
    // weather (condition label via the WMO mapping) and news on each poll.
    let wx = &body.weather;
    eprintln!(
        "magic-mirror: weather status={:?} temp={:.1}°C code={} ({}) pressure={}hPa aqi={}",
        wx.status,
        wx.temperature_c,
        wx.condition_code,
        crate::weather::wmo_condition(wx.condition_code),
        wx.pressure_hpa,
        wx.european_aqi,
    );
    eprintln!(
        "magic-mirror: news status={:?} items={}",
        body.news.status,
        body.news.items.len(),
    );
    eprintln!(
        "magic-mirror: calendar status={:?} events={}",
        body.calendar.status,
        body.calendar.events.len(),
    );
    eprintln!(
        "magic-mirror: transit status={:?} departures={}",
        body.transit.status,
        body.transit.departures.len(),
    );

    (
        [
            (header::CACHE_CONTROL, "no-store".to_string()),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff".to_string()),
        ],
        Json(body),
    )
}

/// `GET /api/v1/health` — liveness plus per-provider status.
pub async fn health() -> impl IntoResponse {
    let body = Json(Health {
        status: "ok".to_string(),
        providers: ProviderHealth {
            calendar: Status::Fresh,
            weather: Status::Fresh,
            transit: Status::Fresh,
            news: Status::Fresh,
        },
    });
    (
        [
            (header::CACHE_CONTROL, "no-store".to_string()),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff".to_string()),
        ],
        body,
    )
}

/// Names and stable event-ID prefixes for the configured calendars.
pub async fn calendars(State(state): State<AppState>) -> impl IntoResponse {
    axum::Json(state.calendar_cache.calendars().await)
}
