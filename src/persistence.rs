//! Last-good JSON persistence with atomic writes (Slice 6).
//!
//! The mirror is an always-on dashboard: a restart (crash, deploy, power blip)
//! should not blank the screen. We persist the last-known-good section data to
//! disk after each successful fetch, and on startup we load it and seed the
//! in-memory caches so the first dashboard render already has data — before the
//! first network refresh completes (acceptance criterion: "on restart the
//! section is restored from last-good before the first refresh").
//!
//! Writes are atomic: we write to a temporary file in the same directory,
//! `fsync` it, then `rename` it over the target. A crash mid-write therefore
//! leaves either the old complete file or the new complete file — never a
//! truncated one.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

/// A process-wide counter so concurrent writers to the same target each get a
/// distinct temp file (otherwise one writer's rename would steal the file out
/// from under another, failing with "No such file or directory").
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

use crate::api::{Departure, Event, Headline};
use crate::weather::ForecastCurrent;

/// The last-known-good section data, persisted to disk (Slice 6).
///
/// Each field is `Option` so a section that never succeeded (or whose data we
/// choose not to persist) is simply absent.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct DashboardSnapshot {
    /// When this snapshot was written (RFC 3339 UTC), for observability.
    pub saved_at: String,
    /// Weather: the last-good forecast and air-quality index.
    pub weather: Option<WeatherData>,
    /// News: the last-good aggregated headlines.
    pub news: Option<Vec<Headline>>,
    /// Calendar: the last-good expanded events.
    pub calendar: Option<Vec<Event>>,
    /// Calendar display names and stable event-ID prefixes (older snapshots omit this).
    #[serde(default)]
    pub calendars: Vec<(String, String)>,
    /// Transit: the last-good departures.
    pub transit: Option<Vec<Departure>>,
}

/// The last-good weather reading (forecast + air-quality).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WeatherData {
    pub forecast: Option<ForecastCurrent>,
    pub aqi: Option<i32>,
}

/// Load a persisted snapshot from disk.
///
/// A missing or malformed file is not an error: it yields `None` so the server
/// still starts (defaults / first refresh apply).
pub fn load(path: &Path) -> Option<DashboardSnapshot> {
    let text = fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

/// Save a snapshot to disk atomically (write temp → fsync → rename).
///
/// Parent directories are created if missing. The `serde_json` error is mapped
/// to an `io::Error` so the caller can treat all persistence failures uniformly.
pub fn save(path: &Path, snap: &DashboardSnapshot) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)?;
        }
    }
    let tmp = temp_path(path);
    let text = serde_json::to_string_pretty(snap)
        .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
    {
        let mut file = fs::File::create(&tmp)?;
        file.write_all(text.as_bytes())?;
        // fsync the file so the bytes are durable before the rename.
        file.sync_all()?;
    }
    fs::rename(&tmp, path)?;
    Ok(())
}

/// A temporary file name in the same directory as `path` (same filesystem, so
/// the rename is atomic).
///
/// The name is unique per writer (process id + a process-wide counter) so
/// concurrent saves to the same target do not collide on a shared temp file.
fn temp_path(path: &Path) -> PathBuf {
    let parent = path.parent().unwrap_or(Path::new("."));
    let file_name = path
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_else(|| "snapshot.json".into());
    let n = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let tmp = format!("{}-{}-{}.tmp", std::process::id(), n, file_name.to_string_lossy());
    parent.join(tmp)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A process-wide counter so every `temp_dir()` call gets a distinct
    /// directory. All tests run in one process (same pid) and macOS
    /// `SystemTime` resolution is ~1ms, so a timestamp alone is not unique
    /// under parallel test execution: two tests landing on the same tick would
    /// share a directory, and one's `remove_dir_all` would delete the other's
    /// mid-run (surfacing as a spurious `NotFound` in `save`).
    static TEMP_DIR_COUNTER: AtomicU64 = AtomicU64::new(0);

    /// A unique temp directory per test (no cross-test interference).
    fn temp_dir() -> PathBuf {
        let n = std::process::id();
        let c = TEMP_DIR_COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("mm-persist-{n}-{c}"));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    fn sample_snapshot() -> DashboardSnapshot {
        DashboardSnapshot {
            saved_at: "2026-10-18T12:00:00Z".to_string(),
            weather: Some(WeatherData {
                forecast: Some(ForecastCurrent {
                    temperature_2m: 12.5,
                    weather_code: 61,
                    pressure_msl: 1013.4,
                }),
                aqi: Some(30),
            }),
            news: Some(vec![Headline {
                id: "n1".into(),
                source: "S".into(),
                title: "T".into(),
                published_at: "2026-10-18T12:00:00Z".into(),
            }]),
            calendars: vec![("https://calendar.test/work/".into(), "Work".into())],
            calendar: Some(vec![Event {
                id: "c1".into(),
                title: "Standup".into(),
                start: "2026-10-18T09:00:00Z".into(),
                end: "2026-10-18T09:30:00Z".into(),
                all_day: false,
                location: Some("Studio".into()),
                description: None,
            }]),
            transit: Some(vec![Departure {
                stop_id: "BKK:9210001:1".into(),
                route: "4".into(),
                destination: "Kálvin tér".into(),
                scheduled_time: "2026-10-18T12:02:00Z".into(),
                predicted_time: Some("2026-10-18T12:02:00Z".into()),
                realtime: true,
                delay_seconds: 0,
                platform: None,
            }]),
        }
    }

    /// Save then load round-trips the full snapshot (acceptance criterion:
    /// "the last-good JSON is persisted to disk").
    #[test]
    fn save_then_load_roundtrips() {
        let dir = temp_dir();
        let path = dir.join("snapshot.json");
        let snap = sample_snapshot();
        save(&path, &snap).expect("save");
        let loaded = load(&path).expect("load");
        assert_eq!(loaded.saved_at, snap.saved_at);
        assert_eq!(loaded.weather, snap.weather);
        assert_eq!(loaded.news, snap.news);
        assert_eq!(loaded.calendar, snap.calendar);
        assert_eq!(loaded.calendars, snap.calendars);
        assert_eq!(loaded.transit, snap.transit);
        let _ = fs::remove_dir_all(&dir);
    }

    /// Loading a missing file yields `None` (server still starts).
    #[test]
    fn load_missing_file_is_none() {
        let dir = temp_dir();
        let path = dir.join("does-not-exist.json");
        assert!(load(&path).is_none());
        let _ = fs::remove_dir_all(&dir);
    }

    /// Loading a malformed file yields `None` (not a crash).
    #[test]
    fn load_malformed_file_is_none() {
        let dir = temp_dir();
        let path = dir.join("bad.json");
        fs::write(&path, "this is not json {").expect("write");
        assert!(load(&path).is_none());
        let _ = fs::remove_dir_all(&dir);
    }

    /// Saving creates missing parent directories.
    #[test]
    fn save_creates_parent_dirs() {
        let dir = temp_dir();
        let path = dir.join("nested/deep/snapshot.json");
        save(&path, &sample_snapshot()).expect("save");
        assert!(path.exists());
        let _ = fs::remove_dir_all(&dir);
    }

    /// Saving is idempotent: a second save overwrites cleanly and the file is
    /// still valid (no leftover temp file, no corruption).
    #[test]
    fn save_overwrites_cleanly() {
        let dir = temp_dir();
        let path = dir.join("snapshot.json");
        save(&path, &sample_snapshot()).expect("save 1");
        let mut second = sample_snapshot();
        second.saved_at = "2026-10-19T12:00:00Z".to_string();
        save(&path, &second).expect("save 2");
        let loaded = load(&path).expect("load");
        assert_eq!(loaded.saved_at, "2026-10-19T12:00:00Z");
        // No leftover temp file.
        let tmp = temp_path(&path);
        assert!(!tmp.exists(), "temp file should be renamed away");
        let _ = fs::remove_dir_all(&dir);
    }

    /// Concurrent saves from multiple threads never leave a corrupt file: every
    /// reader sees either the first or the second snapshot, never garbage.
    #[test]
    fn concurrent_saves_are_atomic() {
        let dir = temp_dir();
        let path = dir.join("snapshot.json");
        let iterations = 20;
        let handles: Vec<_> = (0..4)
            .map(|t| {
                let path = path.clone();
                std::thread::spawn(move || {
                    for i in 0..iterations {
                        let mut snap = sample_snapshot();
                        snap.saved_at = format!("thread-{t}-iter-{i}");
                        save(&path, &snap).expect("save");
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().expect("thread finished");
        }
        // The final file must be valid JSON and a well-formed snapshot.
        let loaded = load(&path).expect("final file must be valid");
        assert!(!loaded.saved_at.is_empty());
        let _ = fs::remove_dir_all(&dir);
    }

    /// A snapshot with all sections absent (never fetched) round-trips too.
    #[test]
    fn empty_snapshot_roundtrips() {
        let dir = temp_dir();
        let path = dir.join("snapshot.json");
        let snap = DashboardSnapshot {
            saved_at: "2026-10-18T12:00:00Z".to_string(),
            ..Default::default()
        };
        save(&path, &snap).expect("save");
        let loaded = load(&path).expect("load");
        assert!(loaded.weather.is_none());
        assert!(loaded.news.is_none());
        assert!(loaded.calendar.is_none());
        assert!(loaded.transit.is_none());
        let _ = fs::remove_dir_all(&dir);
    }
}
