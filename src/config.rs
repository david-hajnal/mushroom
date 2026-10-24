//! Configuration loading for the Magic Mirror server.
//!
//! Slice 1 keeps this deliberately small: a bind address, the display
//! timezone/locale (used to stamp the dashboard JSON), and the static
//! asset directory. Later slices extend this with provider settings.
//!
//! Precedence (highest wins):
//!   1. Environment variables (`MIRROR_BIND`, `MIRROR_STATIC_DIR`)
//!   2. Optional TOML file (`config.toml` in the working directory, or the
//!      path named by `MIRROR_CONFIG`)
//!   3. Built-in defaults
//!
//! Secrets are never read from the config file; they come from the
//! environment in later slices.

use serde::Deserialize;

/// An error loading or validating configuration.
///
/// Carries a single human-readable, *actionable* message: it names the
/// offending field (or file) and says what to change. The server prints this
/// to stderr and exits non-zero (fail fast) so a misconfiguration is caught at
/// boot rather than silently running on wrong defaults (Slice 7).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigError {
    /// A human-readable, actionable description of the problem.
    pub message: String,
}

impl ConfigError {
    /// Build an error from a message.
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ConfigError {}

/// Top-level configuration.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Config {
    pub server: ServerConfig,
    pub display: DisplayConfig,
    /// Weather / air-quality provider settings (Open-Meteo).
    pub weather: WeatherConfig,
    /// News provider settings: the RSS/Atom feeds to aggregate (Slice 3).
    pub news: NewsConfig,
    /// Calendar provider settings: the CalDAV server to read from (Slice 5).
    pub calendar: CalendarConfig,
    /// Transit provider settings: the BKK FUTÁR stops to read from (Slice 4).
    pub transit: TransitConfig,
    /// Directory (relative to the working directory or absolute) that holds
    /// the static frontend: `index.html` plus `assets/`.
    pub static_dir: String,
    /// Path (relative to the working directory or absolute) of the persisted
    /// last-good snapshot (Slice 6 restart recovery). Written after each
    /// successful fetch and loaded on startup to seed the caches.
    pub snapshot_path: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ServerConfig {
    /// Socket address to bind, e.g. `127.0.0.1:3000`.
    pub bind: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct DisplayConfig {
    /// IANA timezone used to stamp the dashboard payload, e.g. `Europe/Budapest`.
    pub timezone: String,
    /// BCP-47 locale for the frontend, e.g. `hu-HU`.
    pub locale: String,
}

/// Weather / air-quality provider settings.
///
/// The location is config-driven (acceptance criterion for Slice 2) and the
/// two refresh intervals are independently configurable (weather vs. AQI).
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct WeatherConfig {
    /// Latitude of the mirror's location (WGS-84 decimal degrees).
    pub lat: f64,
    /// Longitude of the mirror's location (WGS-84 decimal degrees).
    pub lon: f64,
    /// How often the weather (forecast) data is considered fresh, in seconds.
    /// Default 900 s = 15 min (architecture doc cadence).
    pub weather_refresh_secs: u64,
    /// How often the air-quality (AQI) data is considered fresh, in seconds.
    /// Default 3600 s = 60 min (architecture doc cadence).
    pub aqi_refresh_secs: u64,
    /// Base URL of the Open-Meteo forecast API. Overridable for tests /
    /// self-hosted mirrors of the API.
    pub forecast_base_url: String,
    /// Base URL of the Open-Meteo air-quality API.
    pub air_quality_base_url: String,
}

/// A single RSS/Atom feed to aggregate (Slice 3).
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct FeedConfig {
    /// URL of the RSS/Atom feed.
    pub url: String,
    /// Human-readable source label shown in the frontend. If empty, the host
    /// of the feed URL is used as the source.
    pub name: String,
}

impl Default for FeedConfig {
    fn default() -> Self {
        Self {
            url: String::new(),
            name: String::new(),
        }
    }
}

/// News provider settings (Slice 3).
///
/// The list of feeds is config-driven (acceptance criterion: "the news
/// section is populated from configured RSS/Atom feeds") and the refresh
/// interval is independently configurable (acceptance criterion: "the news
/// refresh interval is independently configurable").
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct NewsConfig {
    /// RSS/Atom feeds to aggregate. Empty = no feeds configured (the news
    /// section reports `unavailable`).
    pub feeds: Vec<FeedConfig>,
    /// How often the news data is considered fresh, in seconds.
    /// Default 900 s = 15 min (architecture doc cadence).
    pub news_refresh_secs: u64,
}

impl Default for NewsConfig {
    fn default() -> Self {
        Self {
            feeds: Vec::new(),
            news_refresh_secs: 900,
        }
    }
}

/// Calendar provider settings (Slice 5).
///
/// The CalDAV server is config-driven (acceptance criterion: "CalDAV
/// discovery … resolves the calendar home for the configured server") and the
/// refresh interval is independently configurable (acceptance criterion: "the
/// calendar refresh interval is independently configurable").
///
/// The app-specific password is a *secret*: it is read only from the
/// `MIRROR_CALDAV_APP_PASSWORD` environment variable (acceptance criterion:
/// "the CalDAV app password is read only from the MIRROR_CALDAV_APP_PASSWORD
/// environment variable"). It is `#[serde(skip)]` so a config file can never
/// supply it, and it is never serialized into API JSON or logs.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct CalendarConfig {
    /// Whether the calendar section is enabled. When disabled (the default),
    /// the dashboard keeps its mock calendar section.
    pub enabled: bool,
    /// CalDAV server base URL (discovery entry point), e.g.
    /// `https://caldav.icloud.com/`. Discovery follows server-provided
    /// principal / calendar-home URLs rather than constructing brittle paths.
    pub url: String,
    /// CalDAV account identifier (username), e.g. an Apple ID.
    pub username: String,
    /// Names of the calendars to display, resolved through discovery. Empty
    /// = every calendar under the discovered calendar home.
    pub calendar_names: Vec<String>,
    /// How often the calendar data is considered fresh, in seconds.
    /// Default 300 s = 5 min (spec §8 calendar cadence).
    pub refresh_secs: u64,
    /// Rolling query and display window in 24-hour days from now.
    /// Default 7 (168 hours); overridden by MIRROR_CALENDAR_LOOKAHEAD_DAYS.
    pub lookahead_days: u32,
    /// App-specific password. Populated only from the
    /// `MIRROR_CALDAV_APP_PASSWORD` environment variable by `Config::load`;
    /// never deserialized from the config file.
    #[serde(skip)]
    pub app_password: Option<String>,
}

impl Default for CalendarConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            url: "https://caldav.icloud.com/".to_string(),
            username: String::new(),
            calendar_names: Vec::new(),
            refresh_secs: 300,
            lookahead_days: 7,
            app_password: None,
        }
    }
}

/// Transit provider settings (Slice 4).
///
/// The transit section is populated from the BKK FUTÁR
/// `arrivals-and-departures-for-stop` endpoint for the configured stops
/// (acceptance criterion: "the transit section is populated from the BKK FUTÁR
/// API for configured stops") and the refresh interval is independently
/// configurable (acceptance criterion: "the transit refresh interval is
/// independently configurable").
///
/// The BKK API key is a *secret*: it is read only from the `MIRROR_BKK_API_KEY`
/// environment variable (acceptance criterion: "the BKK API key is read only
/// from the MIRROR_BKK_API_KEY environment variable"). It is `#[serde(skip)]`
/// so a config file can never supply it, and it is never serialized into API
/// JSON or logs.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct TransitConfig {
    /// Whether the transit section is enabled. When disabled (the default),
    /// the dashboard keeps its mock transit section.
    pub enabled: bool,
    /// BKK FUTÁR stop IDs to show departures for (e.g.
    /// `BKK:9210001:1`). Empty = no stops configured (the section reports
    /// `unavailable` when enabled).
    pub stops: Vec<String>,
    /// Route short-names to keep (e.g. `["4", "6", "8"]`). Empty = every route
    /// at the configured stops (acceptance criterion: "route … filtering").
    pub routes: Vec<String>,
    /// Departure horizon in minutes: only departures within this many minutes
    /// of now are shown (acceptance criterion: "… walking-time filtering").
    /// Default 30.
    pub walking_minutes: u32,
    /// How often the transit data is considered fresh, in seconds.
    /// Default 30 s (spec §8 transit cadence).
    pub refresh_secs: u64,
    /// Base URL of the BKK FUTÁR OTP `where` API. Overridable for tests /
    /// self-hosted mirrors of the API.
    pub base_url: String,
    /// BKK API key. Populated only from the `MIRROR_BKK_API_KEY` environment
    /// variable by `Config::load`; never deserialized from the config file.
    #[serde(skip)]
    pub api_key: Option<String>,
}

impl Default for TransitConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            stops: Vec::new(),
            routes: Vec::new(),
            walking_minutes: 30,
            refresh_secs: 30,
            base_url: "https://futar.bkk.hu/api/query/v1/ws/otp/api/where/".to_string(),
            api_key: None,
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            server: ServerConfig::default(),
            display: DisplayConfig::default(),
            weather: WeatherConfig::default(),
            news: NewsConfig::default(),
            calendar: CalendarConfig::default(),
            transit: TransitConfig::default(),
            static_dir: "static".to_string(),
            snapshot_path: "data/snapshot.json".to_string(),
        }
    }
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            bind: "127.0.0.1:3000".to_string(),
        }
    }
}

impl Default for DisplayConfig {
    fn default() -> Self {
        Self {
            timezone: "Europe/Budapest".to_string(),
            locale: "hu-HU".to_string(),
        }
    }
}

impl Default for WeatherConfig {
    fn default() -> Self {
        Self {
            // Budapest (matches the default display timezone).
            lat: 47.4979,
            lon: 19.0402,
            weather_refresh_secs: 900,
            aqi_refresh_secs: 3600,
            forecast_base_url: "https://api.open-meteo.com/v1/forecast".to_string(),
            air_quality_base_url: "https://air-quality-api.open-meteo.com/v1/air-quality".to_string(),
        }
    }
}

impl Config {
    /// Load configuration from the environment and an optional TOML file.
    ///
    /// A *missing* config file is not an error: the built-in defaults apply
    /// (the mirror is fully usable out of the box). A *present but malformed*
    /// config file, or a config whose values are invalid, is an error: it is
    /// returned as [`ConfigError`] so the caller can fail fast with an
    /// actionable message (Slice 7 acceptance criterion: "invalid or missing
    /// configuration fails fast with an actionable error").
    pub fn load() -> Result<Config, ConfigError> {
        let path = std::env::var("MIRROR_CONFIG")
            .unwrap_or_else(|_| "config.toml".to_string());
        let mut cfg = Self::read_config_file(&path)?;

        if let Ok(bind) = std::env::var("MIRROR_BIND") {
            cfg.server.bind = bind;
        }
        if let Ok(dir) = std::env::var("MIRROR_STATIC_DIR") {
            cfg.static_dir = dir;
        }
        if let Ok(path) = std::env::var("MIRROR_SNAPSHOT_PATH") {
            cfg.snapshot_path = path;
        }

        // Weather / air-quality overrides (highest precedence).
        if let Ok(v) = std::env::var("MIRROR_WEATHER_LAT") {
            if let Ok(lat) = v.parse::<f64>() {
                cfg.weather.lat = lat;
            }
        }
        if let Ok(v) = std::env::var("MIRROR_WEATHER_LON") {
            if let Ok(lon) = v.parse::<f64>() {
                cfg.weather.lon = lon;
            }
        }
        if let Ok(v) = std::env::var("MIRROR_WEATHER_REFRESH_SECS") {
            if let Ok(secs) = v.parse::<u64>() {
                cfg.weather.weather_refresh_secs = secs;
            }
        }
        if let Ok(v) = std::env::var("MIRROR_AQI_REFRESH_SECS") {
            if let Ok(secs) = v.parse::<u64>() {
                cfg.weather.aqi_refresh_secs = secs;
            }
        }

        // News override (highest precedence).
        if let Ok(v) = std::env::var("MIRROR_NEWS_REFRESH_SECS") {
            if let Ok(secs) = v.parse::<u64>() {
                cfg.news.news_refresh_secs = secs;
            }
        }

        // Calendar overrides (highest precedence).
        if let Ok(v) = std::env::var("MIRROR_CALENDAR_REFRESH_SECS") {
            if let Ok(secs) = v.parse::<u64>() {
                cfg.calendar.refresh_secs = secs;
            }
        }

        if let Ok(v) = std::env::var("MIRROR_CALENDAR_LOOKAHEAD_DAYS") {
            cfg.calendar.lookahead_days = v.parse::<u32>().map_err(|_| {
                ConfigError::new("MIRROR_CALENDAR_LOOKAHEAD_DAYS must be a positive integer (days)")
            })?;
        }

        // The CalDAV app-specific password is a secret: it is read ONLY from
        // the environment, never from the config file (acceptance criterion:
        // "the CalDAV app password is read only from the
        // MIRROR_CALDAV_APP_PASSWORD environment variable").
        if let Ok(v) = std::env::var("MIRROR_CALDAV_APP_PASSWORD") {
            cfg.calendar.app_password = Some(v);
        }

        // Transit overrides (highest precedence).
        if let Ok(v) = std::env::var("MIRROR_TRANSIT_REFRESH_SECS") {
            if let Ok(secs) = v.parse::<u64>() {
                cfg.transit.refresh_secs = secs;
            }
        }

        // The BKK API key is a secret: it is read ONLY from the environment,
        // never from the config file (acceptance criterion: "the BKK API key is
        // read only from the MIRROR_BKK_API_KEY environment variable").
        if let Ok(v) = std::env::var("MIRROR_BKK_API_KEY") {
            cfg.transit.api_key = Some(v);
        }

        cfg.validate()?;
        Ok(cfg)
    }

    /// Read and parse a config file at `path`.
    ///
    /// A *missing* file is not an error: the built-in defaults are returned. A
    /// *present but malformed* file (invalid TOML, or a value that does not
    /// match the schema) is an error with an actionable message. This is split
    /// out from [`Config::load`] so the malformed-file path can be tested
    /// directly against a temp file without touching process-global env vars.
    fn read_config_file(path: &str) -> Result<Config, ConfigError> {
        match std::fs::read_to_string(path) {
            Ok(text) => match toml::from_str::<Config>(&text) {
                Ok(parsed) => Ok(parsed),
                Err(err) => Err(ConfigError::new(format!(
                    "config file {path:?} is not valid TOML / does not match the schema: {err}. \
                     Fix the file (see config/mirror.example.toml) or remove it to use defaults."
                ))),
            },
            Err(_) => {
                // No config file present — that is fine, defaults apply.
                Ok(Config::default())
            }
        }
    }

    /// Validate the configuration, returning an actionable [`ConfigError`] for
    /// the first problem found.
    ///
    /// This is the "fail fast" gate (Slice 7): every value that would otherwise
    /// cause a confusing runtime failure (an unparseable bind address, an
    /// out-of-range coordinate, a zero refresh interval, an empty feed URL) is
    /// checked here, with a message that names the offending field and says what
    /// to change. A missing config file is *not* an error (defaults apply); this
    /// only rejects values that are present but wrong.
    pub fn validate(&self) -> Result<(), ConfigError> {
        // Bind address must parse as a socket address (host:port).
        if let Err(err) = self.server.bind.parse::<std::net::SocketAddr>() {
            return Err(ConfigError::new(format!(
                "server.bind {:?} is not a valid host:port address ({err}); \
                 use e.g. 127.0.0.1:3000",
                self.server.bind
            )));
        }

        // Display: timezone and locale must be non-empty.
        if self.display.timezone.trim().is_empty() {
            return Err(ConfigError::new(
                "display.timezone must be a non-empty IANA timezone (e.g. Europe/Budapest)",
            ));
        }
        if self.display.locale.trim().is_empty() {
            return Err(ConfigError::new(
                "display.locale must be a non-empty BCP-47 locale (e.g. hu-HU)",
            ));
        }

        // Weather: coordinates must be finite and in range.
        if !self.weather.lat.is_finite() || self.weather.lat < -90.0 || self.weather.lat > 90.0 {
            return Err(ConfigError::new(format!(
                "weather.lat {} is out of range [-90, 90] (WGS-84 decimal degrees)",
                self.weather.lat
            )));
        }
        if !self.weather.lon.is_finite() || self.weather.lon < -180.0 || self.weather.lon > 180.0 {
            return Err(ConfigError::new(format!(
                "weather.lon {} is out of range [-180, 180] (WGS-84 decimal degrees)",
                self.weather.lon
            )));
        }

        // Refresh intervals must be positive (a zero interval would busy-loop).
        if self.weather.weather_refresh_secs == 0 {
            return Err(ConfigError::new(
                "weather.weather_refresh_secs must be > 0 (seconds)",
            ));
        }
        if self.weather.aqi_refresh_secs == 0 {
            return Err(ConfigError::new("weather.aqi_refresh_secs must be > 0 (seconds)"));
        }
        if self.news.news_refresh_secs == 0 {
            return Err(ConfigError::new("news.news_refresh_secs must be > 0 (seconds)"));
        }
        if self.calendar.refresh_secs == 0 {
            return Err(ConfigError::new("calendar.refresh_secs must be > 0 (seconds)"));
        }
        if self.calendar.lookahead_days == 0 {
            return Err(ConfigError::new(
                "calendar.lookahead_days must be > 0 (days)",
            ));
        }

        // Transit: the refresh interval must be positive (a zero interval would
        // busy-loop), and an enabled transit section needs at least one stop.
        if self.transit.refresh_secs == 0 {
            return Err(ConfigError::new(
                "transit.refresh_secs must be > 0 (seconds)",
            ));
        }
        if self.transit.enabled && self.transit.stops.is_empty() {
            return Err(ConfigError::new(
                "transit.stops must list at least one BKK FUTÁR stop when transit.enabled = true",
            ));
        }

        // Paths must be non-empty.
        if self.static_dir.trim().is_empty() {
            return Err(ConfigError::new(
                "static_dir must be a non-empty path to the static frontend",
            ));
        }
        if self.snapshot_path.trim().is_empty() {
            return Err(ConfigError::new(
                "snapshot_path must be a non-empty path for the last-good snapshot",
            ));
        }

        // News feeds: if any are configured, each must carry a valid http(s) URL.
        for (i, feed) in self.news.feeds.iter().enumerate() {
            if feed.url.trim().is_empty() {
                return Err(ConfigError::new(format!(
                    "news.feeds[{i}].url must be a non-empty http(s) feed URL",
                )));
            }
            if !(feed.url.starts_with("http://") || feed.url.starts_with("https://")) {
                return Err(ConfigError::new(format!(
                    "news.feeds[{i}].url {:?} must start with http:// or https://",
                    feed.url
                )));
            }
        }

        // Calendar: when enabled, the server URL and username are required.
        if self.calendar.enabled {
            if self.calendar.url.trim().is_empty() {
                return Err(ConfigError::new(
                    "calendar.url must be a non-empty CalDAV server URL when calendar.enabled = true",
                ));
            }
            if !(self.calendar.url.starts_with("http://")
                || self.calendar.url.starts_with("https://"))
            {
                return Err(ConfigError::new(format!(
                    "calendar.url {:?} must start with http:// or https://",
                    self.calendar.url
                )));
            }
            if self.calendar.username.trim().is_empty() {
                return Err(ConfigError::new(
                    "calendar.username must be a non-empty CalDAV account when calendar.enabled = true",
                ));
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    //! Slice 7: configuration fails fast with an actionable error.
    //!
    //! These tests exercise the validation gate directly (no network, no env
    //! vars, no port) so they are deterministic and safe to run in parallel.

    use super::*;

    /// A unique temp file path per test (no cross-test interference).
    fn temp_path(name: &str) -> std::path::PathBuf {
        let n = std::process::id();
        let t = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        std::env::temp_dir().join(format!("mm-config-{n}-{t}-{name}"))
    }

    /// The built-in defaults must always be valid (the mirror is usable out of
    /// the box with no config file).
    #[test]
    fn default_config_is_valid() {
        let cfg = Config::default();
        cfg.validate().expect("defaults must validate");
    }

    /// A present but malformed config file fails fast with an actionable error
    /// (acceptance criterion: "invalid … configuration fails fast").
    #[test]
    fn malformed_toml_fails_fast() {
        let path = temp_path("malformed.toml");
        std::fs::write(&path, "this is [ not valid toml ===").expect("write bad config");
        let err = Config::read_config_file(path.to_str().unwrap())
            .expect_err("malformed config must be rejected");
        // The message must be actionable: name the file and point to a fix.
        let msg = err.to_string();
        assert!(msg.contains("not valid TOML"), "message should explain the problem: {msg}");
        assert!(msg.contains("mirror.example.toml"), "message should point to a fix: {msg}");
        let _ = std::fs::remove_file(&path);
    }

    /// A missing config file is not an error: defaults apply (the mirror is
    /// fully usable out of the box).
    #[test]
    fn missing_file_uses_defaults() {
        let path = temp_path("does-not-exist.toml");
        let cfg = Config::read_config_file(path.to_str().unwrap())
            .expect("a missing config file must not be an error");
        assert_eq!(cfg.server.bind, "127.0.0.1:3000");
        cfg.validate().expect("defaults must validate");
    }

    #[test]
    fn calendar_lookahead_is_configurable_and_must_be_positive() {
        for days in [1, 2, 7] {
            let cfg: Config = toml::from_str(&format!("[calendar]\nlookahead_days = {days}\n"))
                .expect("valid days");
            assert_eq!(cfg.calendar.lookahead_days, days);
            cfg.validate().expect("positive days validate");
        }
        let mut cfg = Config::default();
        cfg.calendar.lookahead_days = 0;
        assert!(cfg.validate().unwrap_err().to_string().contains("calendar.lookahead_days"));
    }

    /// A valid config file parses and validates.
    #[test]
    fn valid_file_parses_and_validates() {
        let path = temp_path("valid.toml");
        std::fs::write(
            &path,
            "static_dir = \"static\"\n[weather]\nlat = 47.5\nlon = 19.0\n",
        )
        .expect("write config");
        let cfg = Config::read_config_file(path.to_str().unwrap()).expect("parse");
        assert!((cfg.weather.lat - 47.5).abs() < f64::EPSILON);
        cfg.validate().expect("valid config must validate");
        let _ = std::fs::remove_file(&path);
    }

    /// An unparseable bind address fails fast, naming the field.
    #[test]
    fn invalid_bind_fails() {
        let mut cfg = Config::default();
        cfg.server.bind = "not-an-address".to_string();
        let err = cfg.validate().expect_err("invalid bind must be rejected");
        assert!(err.to_string().contains("server.bind"), "message should name the field: {}", err);
    }

    /// Out-of-range coordinates fail fast, naming the field.
    #[test]
    fn out_of_range_coordinates_fail() {
        let mut cfg = Config::default();
        cfg.weather.lat = 91.0;
        let err = cfg.validate().expect_err("lat > 90 must be rejected");
        assert!(err.to_string().contains("weather.lat"), "message should name the field: {err}");

        let mut cfg = Config::default();
        cfg.weather.lon = -181.0;
        let err = cfg.validate().expect_err("lon < -180 must be rejected");
        assert!(err.to_string().contains("weather.lon"), "message should name the field: {err}");
    }

    /// A zero refresh interval would busy-loop, so it fails fast.
    #[test]
    fn zero_refresh_interval_fails() {
        let mut cfg = Config::default();
        cfg.weather.weather_refresh_secs = 0;
        let err = cfg.validate().expect_err("zero refresh must be rejected");
        assert!(
            err.to_string().contains("weather_refresh_secs"),
            "message should name the field: {err}"
        );
    }

    /// A configured feed with an empty or non-http URL fails fast.
    #[test]
    fn bad_feed_url_fails() {
        let mut cfg = Config::default();
        cfg.news.feeds.push(FeedConfig {
            url: String::new(),
            name: String::new(),
        });
        let err = cfg.validate().expect_err("empty feed url must be rejected");
        assert!(err.to_string().contains("news.feeds[0].url"), "message should name the field: {err}");

        let mut cfg = Config::default();
        cfg.news.feeds.push(FeedConfig {
            url: "ftp://example.com/feed".to_string(),
            name: String::new(),
        });
        let err = cfg.validate().expect_err("non-http feed url must be rejected");
        assert!(
            err.to_string().contains("http:// or https://"),
            "message should explain the requirement: {err}"
        );
    }

    /// An enabled calendar with no username fails fast (it could never
    /// authenticate), naming the field.
    #[test]
    fn calendar_enabled_without_username_fails() {
        let mut cfg = Config::default();
        cfg.calendar.enabled = true;
        cfg.calendar.username = String::new();
        let err = cfg.validate().expect_err("enabled calendar without username must be rejected");
        assert!(
            err.to_string().contains("calendar.username"),
            "message should name the field: {err}"
        );
    }

    /// A disabled calendar (the default) is valid even with an empty username.
    #[test]
    fn calendar_disabled_is_valid() {
        let cfg = Config::default();
        assert!(!cfg.calendar.enabled);
        cfg.validate().expect("disabled calendar must validate");
    }

    /// The BKK API key is read only from the `MIRROR_BKK_API_KEY` environment
    /// variable (acceptance criterion). A config file cannot supply it
    /// (`#[serde(skip)]`).
    #[test]
    fn transit_api_key_is_env_only() {
        let toml = r#"
[transit]
enabled = true
stops = ["BKK:9210001:1"]
api_key = "from-file-must-be-ignored"
refresh_secs = 30
"#;
        let cfg: Config = toml::from_str(toml).expect("parses");
        assert!(
            cfg.transit.api_key.is_none(),
            "config file must not supply the BKK API key: {:?}",
            cfg.transit.api_key
        );
        assert_eq!(cfg.transit.refresh_secs, 30);
        assert_eq!(cfg.transit.stops, vec!["BKK:9210001:1"]);
    }

    /// The transit refresh interval is independently configurable (separate
    /// from the weather / news / calendar intervals).
    #[test]
    fn transit_refresh_interval_independently_configurable() {
        let toml = r#"
[transit]
refresh_secs = 30
[weather]
weather_refresh_secs = 900
[news]
news_refresh_secs = 900
[calendar]
refresh_secs = 300
"#;
        let cfg: Config = toml::from_str(toml).expect("parses");
        assert_eq!(cfg.transit.refresh_secs, 30);
        assert_eq!(cfg.weather.weather_refresh_secs, 900);
        assert_eq!(cfg.news.news_refresh_secs, 900);
        assert_eq!(cfg.calendar.refresh_secs, 300);
    }

    /// An enabled transit section with no stops fails fast (it could never
    /// fetch), naming the field.
    #[test]
    fn transit_enabled_without_stops_fails() {
        let mut cfg = Config::default();
        cfg.transit.enabled = true;
        cfg.transit.stops = Vec::new();
        let err = cfg
            .validate()
            .expect_err("enabled transit without stops must be rejected");
        assert!(
            err.to_string().contains("transit.stops"),
            "message should name the field: {err}"
        );
    }

    /// A disabled transit section (the default) is valid even with no stops.
    #[test]
    fn transit_disabled_is_valid() {
        let cfg = Config::default();
        assert!(!cfg.transit.enabled);
        cfg.validate().expect("disabled transit must validate");
    }

    /// `ConfigError` implements `Display` and `Error` (so it can be printed and
    /// used as a boxed error).
    #[test]
    fn config_error_is_display_and_error() {
        let e = ConfigError::new("boom");
        assert_eq!(e.to_string(), "boom");
        let boxed: Box<dyn std::error::Error> = Box::new(e);
        assert_eq!(boxed.to_string(), "boom");
    }
}
