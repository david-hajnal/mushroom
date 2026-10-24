//! Weather + air-quality integration with Open-Meteo (Slice 2).
//!
//! Open-Meteo requires no API key and its data is licensed CC BY 4.0
//! (attribution is surfaced in the frontend / docs). This module fetches the
//! current forecast and the current air-quality index for a config-driven
//! location and folds them into the `WeatherSection` shape defined in
//! `api.rs` (the frontend contract stays stable).
//!
//! The two providers are independent: a forecast fetch and an air-quality
//! fetch. Each can fail on its own, and the section `status` reflects the
//! worst outcome so the frontend can render a "stale / unavailable" affordance
//! rather than the endpoint ever returning 500.

use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

use crate::api::{Status, WeatherSection};
use crate::backoff::{retry_after_from_response, FetchError, RetryState};
use crate::config::WeatherConfig;

/// Map a WMO weather code (0–99) to a short human-readable condition label.
///
/// Unrecognised or out-of-range codes fall back to `"Unknown"` so a novel code
/// never panics and always yields a displayable string. This is the
/// `weather_code_map_unknown_fallback` behaviour called out in the program
/// design.
pub fn wmo_condition(code: i32) -> &'static str {
    match code {
        0 => "Clear sky",
        1 => "Mainly clear",
        2 => "Partly cloudy",
        3 => "Overcast",
        45 | 48 => "Fog",
        51 => "Light drizzle",
        53 => "Moderate drizzle",
        55 => "Dense drizzle",
        56 => "Light freezing drizzle",
        57 => "Dense freezing drizzle",
        61 => "Light rain",
        63 => "Moderate rain",
        65 => "Heavy rain",
        66 => "Light freezing rain",
        67 => "Heavy freezing rain",
        71 => "Light snow",
        73 => "Moderate snow",
        75 => "Heavy snow",
        77 => "Snow grains",
        80 => "Light rain showers",
        81 => "Moderate rain showers",
        82 => "Violent rain showers",
        85 | 86 => "Snow showers",
        95 => "Thunderstorm",
        96 | 99 => "Thunderstorm with hail",
        _ => "Unknown",
    }
}

/// A subset of the Open-Meteo forecast `current` block we consume.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct ForecastCurrent {
    pub temperature_2m: f64,
    pub weather_code: i32,
    pub pressure_msl: f64,
}

#[derive(Debug, Deserialize)]
struct ForecastResponse {
    current: ForecastCurrent,
}

/// A subset of the Open-Meteo air-quality `current` block we consume.
///
/// Open-Meteo exposes `us_aqi` as its headline index; we surface it as the
/// section's `european_aqi` field (the closest available single-number index).
#[derive(Debug, Deserialize)]
struct AirQualityCurrent {
    us_aqi: i32,
}

#[derive(Debug, Deserialize)]
struct AirQualityResponse {
    current: AirQualityCurrent,
}

/// Fetch the current forecast for the configured location.
///
/// Failures are surfaced as a [`FetchError`] so the caller can honor the
/// upstream's `Retry-After` hint (HTTP 429 / 503) when scheduling the next
/// retry (Slice 6).
pub async fn fetch_forecast(
    client: &reqwest::Client,
    cfg: &WeatherConfig,
) -> Result<ForecastCurrent, FetchError> {
    let url = format!(
        "{}?latitude={}&longitude={}&current=temperature_2m,weather_code,pressure_msl&timezone=auto",
        cfg.forecast_base_url, cfg.lat, cfg.lon
    );
    let resp = client
        .get(&url)
        .send()
        .await
        .map_err(FetchError::Network)?;
    if !resp.status().is_success() {
        return Err(FetchError::Http {
            status: resp.status(),
            retry_after: retry_after_from_response(&resp),
        });
    }
    let body: ForecastResponse = resp
        .json()
        .await
        .map_err(|e| FetchError::Parse(e.to_string()))?;
    Ok(body.current)
}

/// Fetch the current air-quality index for the configured location.
///
/// Failures are surfaced as a [`FetchError`] so the caller can honor the
/// upstream's `Retry-After` hint (HTTP 429 / 503) when scheduling the next
/// retry (Slice 6).
pub async fn fetch_air_quality(
    client: &reqwest::Client,
    cfg: &WeatherConfig,
) -> Result<i32, FetchError> {
    let url = format!(
        "{}?latitude={}&longitude={}&current=us_aqi&timezone=auto",
        cfg.air_quality_base_url, cfg.lat, cfg.lon
    );
    let resp = client
        .get(&url)
        .send()
        .await
        .map_err(FetchError::Network)?;
    if !resp.status().is_success() {
        return Err(FetchError::Http {
            status: resp.status(),
            retry_after: retry_after_from_response(&resp),
        });
    }
    let body: AirQualityResponse = resp
        .json()
        .await
        .map_err(|e| FetchError::Parse(e.to_string()))?;
    Ok(body.current.us_aqi)
}

/// Fold a (possibly partial) set of live readings into the API's
/// `WeatherSection`.
///
/// `forecast` and `aqi` are independent: if one fetch failed we still surface
/// the other, and the section `status` reflects the worst outcome. When both
/// failed we fall back to the supplied mock values with `Status::Unavailable`
/// so the endpoint never 500s and the frontend always has a well-shaped body.
pub fn build_weather_section(
    now: String,
    forecast: Option<ForecastCurrent>,
    aqi: Option<i32>,
    fallback: WeatherSection,
) -> WeatherSection {
    match (forecast, aqi) {
        (Some(fc), Some(aqi_val)) => WeatherSection {
            updated_at: now,
            status: Status::Fresh,
            temperature_c: fc.temperature_2m,
            condition_code: fc.weather_code,
            pressure_hpa: fc.pressure_msl.round() as i32,
            european_aqi: aqi_val,
        },
        (Some(fc), None) => WeatherSection {
            updated_at: now,
            status: Status::Stale,
            temperature_c: fc.temperature_2m,
            condition_code: fc.weather_code,
            pressure_hpa: fc.pressure_msl.round() as i32,
            european_aqi: fallback.european_aqi,
        },
        (None, Some(aqi_val)) => WeatherSection {
            updated_at: now,
            status: Status::Stale,
            temperature_c: fallback.temperature_c,
            condition_code: fallback.condition_code,
            pressure_hpa: fallback.pressure_hpa,
            european_aqi: aqi_val,
        },
        (None, None) => WeatherSection {
            updated_at: now,
            status: Status::Unavailable,
            ..fallback
        },
    }
}

/// A last-known-good reading with the instant it was fetched.
#[derive(Debug, Clone)]
struct Snapshot<T> {
    fetched_at: Instant,
    value: T,
}

/// The worse of two provider statuses (Fresh < Stale < Unavailable).
///
/// The section status reflects the worst outcome so the frontend degrades
/// gracefully when either provider is not fresh.
pub fn worst_status(a: Status, b: Status) -> Status {
    let rank = |s: Status| match s {
        Status::Fresh => 0,
        Status::Stale => 1,
        Status::Unavailable => 2,
    };
    if rank(a) >= rank(b) {
        a
    } else {
        b
    }
}

/// The outcome of resolving a provider reading against its refresh interval.
#[derive(Debug)]
pub enum Resolved<T> {
    /// Fresh: fetched now, or served from a snapshot within the interval.
    Fresh(T),
    /// Stale: a refetch was attempted and failed; serving the last-good value.
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

/// In-memory weather + air-quality snapshots, atomically replaced on each
/// successful refresh (architecture doc §Data). Weather and AQI are cached
/// independently so their refresh intervals are honored independently
/// (acceptance criterion: "Weather and AQI refresh intervals are independently
/// configurable").
#[derive(Debug, Default)]
pub struct WeatherCache {
    forecast: RwLock<Option<Snapshot<ForecastCurrent>>>,
    aqi: RwLock<Option<Snapshot<i32>>>,
    /// Per-provider retry state (Slice 6): after a fetch failure we skip
    /// refetches until the backoff cooldown elapses, serving the last-good
    /// snapshot as `Stale` in the meantime. Forecast and AQI are independent
    /// providers, so they carry independent retry state.
    forecast_retry: RwLock<RetryState>,
    aqi_retry: RwLock<RetryState>,
}

impl WeatherCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// Seed the forecast snapshot from a persisted last-good value (Slice 6
    /// restart recovery). The snapshot is stamped with the current instant so
    /// it is immediately eligible to be served (as `Stale` if a refetch is
    /// attempted and fails, or `Fresh` if the refresh interval has not yet
    /// elapsed).
    pub async fn seed_forecast(&self, value: ForecastCurrent) {
        *self.forecast.write().await =
            Some(Snapshot { fetched_at: Instant::now(), value });
    }

    /// Seed the air-quality snapshot from a persisted last-good value.
    pub async fn seed_aqi(&self, value: i32) {
        *self.aqi.write().await = Some(Snapshot { fetched_at: Instant::now(), value });
    }

    /// The last-good forecast value, if any (for persistence).
    pub async fn last_good_forecast(&self) -> Option<ForecastCurrent> {
        self.forecast.read().await.as_ref().map(|s| s.value.clone())
    }

    /// The last-good air-quality value, if any (for persistence).
    pub async fn last_good_aqi(&self) -> Option<i32> {
        self.aqi.read().await.as_ref().map(|s| s.value)
    }

    /// Resolve the current forecast, honoring the weather refresh interval.
    ///
    /// A snapshot younger than the interval is served as-is (`Fresh`).
    /// Otherwise a refetch is attempted: on success the snapshot is replaced
    /// and the value is `Fresh`; on failure the last-good snapshot (if any) is
    /// served as `Stale`, else `Unavailable`.
    ///
    /// While the provider is in its backoff cooldown (a recent failure) the
    /// refetch is skipped entirely and the last-good snapshot is served as
    /// `Stale` (Slice 6) — so a failing upstream is not re-hit on every poll.
    pub async fn resolve_forecast(
        &self,
        client: &reqwest::Client,
        cfg: &WeatherConfig,
    ) -> Resolved<ForecastCurrent> {
        let interval = Duration::from_secs(cfg.weather_refresh_secs);
        if let Some(snap) = self.forecast.read().await.as_ref() {
            if snap.fetched_at.elapsed() < interval {
                return Resolved::Fresh(snap.value.clone());
            }
        }
        // In backoff cooldown: skip the refetch and serve the last-good value.
        if self.forecast_retry.read().await.in_cooldown() {
            return match self.forecast.read().await.as_ref() {
                Some(snap) => Resolved::Stale(snap.value.clone()),
                None => Resolved::Unavailable,
            };
        }
        match fetch_forecast(client, cfg).await {
            Ok(value) => {
                self.forecast_retry.write().await.record_success();
                *self.forecast.write().await =
                    Some(Snapshot { fetched_at: Instant::now(), value: value.clone() });
                Resolved::Fresh(value)
            }
            Err(err) => {
                self.forecast_retry
                    .write()
                    .await
                    .record_failure(err.retry_after());
                match self.forecast.read().await.as_ref() {
                    Some(snap) => Resolved::Stale(snap.value.clone()),
                    None => Resolved::Unavailable,
                }
            }
        }
    }

    /// Resolve the current air-quality index, honoring the AQI refresh interval.
    pub async fn resolve_aqi(
        &self,
        client: &reqwest::Client,
        cfg: &WeatherConfig,
    ) -> Resolved<i32> {
        let interval = Duration::from_secs(cfg.aqi_refresh_secs);
        if let Some(snap) = self.aqi.read().await.as_ref() {
            if snap.fetched_at.elapsed() < interval {
                return Resolved::Fresh(snap.value);
            }
        }
        // In backoff cooldown: skip the refetch and serve the last-good value.
        if self.aqi_retry.read().await.in_cooldown() {
            return match self.aqi.read().await.as_ref() {
                Some(snap) => Resolved::Stale(snap.value),
                None => Resolved::Unavailable,
            };
        }
        match fetch_air_quality(client, cfg).await {
            Ok(value) => {
                self.aqi_retry.write().await.record_success();
                *self.aqi.write().await =
                    Some(Snapshot { fetched_at: Instant::now(), value });
                Resolved::Fresh(value)
            }
            Err(err) => {
                self.aqi_retry.write().await.record_failure(err.retry_after());
                match self.aqi.read().await.as_ref() {
                    Some(snap) => Resolved::Stale(snap.value),
                    None => Resolved::Unavailable,
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// WMO code → label mapping, including the unknown-code fallback.
    #[test]
    fn weather_code_map_known_codes() {
        assert_eq!(wmo_condition(0), "Clear sky");
        assert_eq!(wmo_condition(2), "Partly cloudy");
        assert_eq!(wmo_condition(45), "Fog");
        assert_eq!(wmo_condition(48), "Fog");
        assert_eq!(wmo_condition(61), "Light rain");
        assert_eq!(wmo_condition(65), "Heavy rain");
        assert_eq!(wmo_condition(71), "Light snow");
        assert_eq!(wmo_condition(77), "Snow grains");
        assert_eq!(wmo_condition(95), "Thunderstorm");
        assert_eq!(wmo_condition(99), "Thunderstorm with hail");
    }

    /// Unknown / out-of-range codes must fall back to "Unknown", never panic.
    #[test]
    fn weather_code_map_unknown_fallback() {
        assert_eq!(wmo_condition(100), "Unknown");
        assert_eq!(wmo_condition(999), "Unknown");
        assert_eq!(wmo_condition(-1), "Unknown");
        // Gaps in the WMO table (e.g. 4, 5, 6, 9, 10) are also "Unknown".
        assert_eq!(wmo_condition(4), "Unknown");
        assert_eq!(wmo_condition(10), "Unknown");
    }

    /// Both providers present → Fresh with live values.
    #[test]
    fn build_section_both_present_is_fresh() {
        let fc = ForecastCurrent {
            temperature_2m: 12.5,
            weather_code: 61,
            pressure_msl: 1013.4,
        };
        let fallback = mock_fallback();
        let section = build_weather_section("now".into(), Some(fc), Some(30), fallback);
        assert_eq!(section.status, Status::Fresh);
        assert_eq!(section.temperature_c, 12.5);
        assert_eq!(section.condition_code, 61);
        assert_eq!(section.pressure_hpa, 1013);
        assert_eq!(section.european_aqi, 30);
    }

    /// Forecast present, AQI failed → Stale, AQI falls back to mock value.
    #[test]
    fn build_section_aqi_missing_is_stale() {
        let fc = ForecastCurrent {
            temperature_2m: 12.5,
            weather_code: 61,
            pressure_msl: 1013.4,
        };
        let fallback = mock_fallback();
        let section = build_weather_section("now".into(), Some(fc), None, fallback);
        assert_eq!(section.status, Status::Stale);
        assert_eq!(section.temperature_c, 12.5);
        assert_eq!(section.european_aqi, 22); // mock fallback AQI
    }

    /// Both failed → Unavailable, all values from mock fallback.
    #[test]
    fn build_section_both_missing_is_unavailable() {
        let fallback = mock_fallback();
        let section = build_weather_section("now".into(), None, None, fallback);
        assert_eq!(section.status, Status::Unavailable);
        assert_eq!(section.temperature_c, 11.2);
        assert_eq!(section.condition_code, 45);
        assert_eq!(section.pressure_hpa, 1014);
        assert_eq!(section.european_aqi, 22);
    }

    fn mock_fallback() -> WeatherSection {
        WeatherSection {
            updated_at: String::new(),
            status: Status::Fresh,
            temperature_c: 11.2,
            condition_code: 45,
            pressure_hpa: 1014,
            european_aqi: 22,
        }
    }

    // --- Cache / refresh-interval behaviour (criterion: independently
    //     configurable refresh intervals) -----------------------------------

    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    /// Start a minimal mock Open-Meteo server on a random localhost port.
    ///
    /// Returns the base URL plus independent hit counters for the forecast and
    /// air-quality endpoints so tests can assert how many times each was hit.
    async fn start_mock_server() -> (String, Arc<AtomicUsize>, Arc<AtomicUsize>) {
        use axum::{routing::get, Json, Router};

        let forecast_hits = Arc::new(AtomicUsize::new(0));
        let aqi_hits = Arc::new(AtomicUsize::new(0));

        let fh = forecast_hits.clone();
        let ah = aqi_hits.clone();

        let app = Router::new()
            .route(
                "/forecast",
                get(move |_: ()| {
                    fh.fetch_add(1, Ordering::SeqCst);
                    async move {
                        Json(serde_json::json!({
                            "current": {
                                "temperature_2m": 20.5,
                                "weather_code": 1,
                                "pressure_msl": 1020.0
                            }
                        }))
                    }
                }),
            )
            .route(
                "/air-quality",
                get(move |_: ()| {
                    ah.fetch_add(1, Ordering::SeqCst);
                    async move {
                        Json(serde_json::json!({ "current": { "us_aqi": 42 } }))
                    }
                }),
            );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock server");
        let addr = listener.local_addr().expect("local addr");
        tokio::spawn(async move {
            axum::serve(listener, app).await.expect("mock server ran");
        });

        (format!("http://{addr}"), forecast_hits, aqi_hits)
    }

    fn cfg_pointing_at(base: &str, weather_secs: u64, aqi_secs: u64) -> WeatherConfig {
        WeatherConfig {
            lat: 47.4979,
            lon: 19.0402,
            weather_refresh_secs: weather_secs,
            aqi_refresh_secs: aqi_secs,
            forecast_base_url: format!("{base}/forecast"),
            air_quality_base_url: format!("{base}/air-quality"),
        }
    }

    /// A reading within the refresh interval is served from cache: the second
    /// resolve does not hit the upstream again.
    #[tokio::test]
    async fn cache_serves_within_interval() {
        let (base, forecast_hits, _aqi_hits) = start_mock_server().await;
        let cfg = cfg_pointing_at(&base, 3600, 3600); // 1 h interval
        let cache = WeatherCache::new();
        let client = reqwest::Client::new();

        let r1 = cache.resolve_forecast(&client, &cfg).await;
        let r2 = cache.resolve_forecast(&client, &cfg).await;

        assert!(matches!(r1, Resolved::Fresh(_)));
        assert!(matches!(r2, Resolved::Fresh(_)));
        assert_eq!(forecast_hits.load(Ordering::SeqCst), 1, "second call must be cached");
    }

    /// An interval of zero forces a refetch every time (no caching).
    #[tokio::test]
    async fn zero_interval_always_refetches() {
        let (base, _forecast_hits, aqi_hits) = start_mock_server().await;
        let cfg = cfg_pointing_at(&base, 3600, 0); // AQI interval = 0
        let cache = WeatherCache::new();
        let client = reqwest::Client::new();

        let _ = cache.resolve_aqi(&client, &cfg).await;
        let _ = cache.resolve_aqi(&client, &cfg).await;

        assert_eq!(aqi_hits.load(Ordering::SeqCst), 2, "zero interval must refetch");
    }

    /// The two intervals are independent: with a long weather interval and a
    /// zero AQI interval, the forecast is cached while the AQI refetches.
    #[tokio::test]
    async fn intervals_are_independent() {
        let (base, forecast_hits, aqi_hits) = start_mock_server().await;
        let cfg = cfg_pointing_at(&base, 3600, 0); // weather 1 h, AQI 0
        let cache = WeatherCache::new();
        let client = reqwest::Client::new();

        // Two rounds of resolving both providers.
        for _ in 0..2 {
            let _ = cache.resolve_forecast(&client, &cfg).await;
            let _ = cache.resolve_aqi(&client, &cfg).await;
        }

        assert_eq!(forecast_hits.load(Ordering::SeqCst), 1, "forecast cached (1 h interval)");
        assert_eq!(aqi_hits.load(Ordering::SeqCst), 2, "AQI refetched (0 interval)");
    }

    // --- Slice 6: restart recovery + backoff cooldown -----------------------

    /// Start a mock server whose endpoints always fail (500), with a shared hit
    /// counter so tests can assert how many times the upstream was hit.
    async fn start_failing_server() -> (String, Arc<AtomicUsize>) {
        use axum::{routing::get, Router};

        let hits = Arc::new(AtomicUsize::new(0));

        let app = Router::new()
            .route(
                "/forecast",
                get({
                    let h = hits.clone();
                    move |_: ()| {
                        h.fetch_add(1, Ordering::SeqCst);
                        async move { axum::http::StatusCode::INTERNAL_SERVER_ERROR }
                    }
                }),
            )
            .route(
                "/air-quality",
                get({
                    let h = hits.clone();
                    move |_: ()| {
                        h.fetch_add(1, Ordering::SeqCst);
                        async move { axum::http::StatusCode::INTERNAL_SERVER_ERROR }
                    }
                }),
            );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind failing server");
        let addr = listener.local_addr().expect("local addr");
        tokio::spawn(async move {
            axum::serve(listener, app).await.expect("failing server ran");
        });

        (format!("http://{addr}"), hits)
    }

    fn seeded_forecast() -> ForecastCurrent {
        ForecastCurrent {
            temperature_2m: 12.5,
            weather_code: 61,
            pressure_msl: 1013.4,
        }
    }

    /// Restart recovery: a seeded (persisted) last-good snapshot is served as
    /// `Stale` when the first refresh after restart fails (acceptance criterion:
    /// "on restart the section is restored from last-good before the first
    /// refresh").
    #[tokio::test]
    async fn restart_recovery_serves_last_good_as_stale() {
        let (base, _hits) = start_failing_server().await;
        let cfg = cfg_pointing_at(&base, 0, 0); // zero interval forces a refetch
        let cache = WeatherCache::new();
        let client = reqwest::Client::new();

        // Simulate restart recovery: seed the last-good snapshot from disk.
        cache.seed_forecast(seeded_forecast()).await;

        // The first refresh after restart fails; the seeded value is served as Stale.
        let resolved = cache.resolve_forecast(&client, &cfg).await;
        let (value, status) = resolved.into_parts();
        assert_eq!(status, Status::Stale, "seeded last-good must be served as Stale");
        let value = value.expect("seeded last-good must be retained");
        assert_eq!(value.temperature_2m, 12.5);
    }

    /// Backoff cooldown: after a failure, the next resolve within the cooldown
    /// window does not hit the upstream (acceptance criterion: "exponential
    /// backoff … skips refetches while in cooldown").
    #[tokio::test]
    async fn backoff_cooldown_skips_refetch() {
        let (base, hits) = start_failing_server().await;
        let cfg = cfg_pointing_at(&base, 0, 0); // zero interval forces a refetch
        let cache = WeatherCache::new();
        let client = reqwest::Client::new();

        // Seed last-good so the cooldown path serves Stale (not Unavailable).
        cache.seed_forecast(seeded_forecast()).await;

        // First resolve: not in cooldown, attempts the (failing) fetch.
        let r1 = cache.resolve_forecast(&client, &cfg).await;
        assert!(matches!(r1, Resolved::Stale(_)));
        assert_eq!(hits.load(Ordering::SeqCst), 1, "first resolve must attempt the fetch");

        // Second resolve: now in cooldown, must NOT hit the upstream.
        let r2 = cache.resolve_forecast(&client, &cfg).await;
        assert!(matches!(r2, Resolved::Stale(_)));
        assert_eq!(hits.load(Ordering::SeqCst), 1, "cooldown must skip the refetch");
    }
}
