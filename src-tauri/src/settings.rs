//! Versioned settings, `settings.v1`, stored as JSON next to the database.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::media::plan::ResolutionCap;
use crate::queue::orchestrator::PauseRange;

pub const FILE_NAME: &str = "settings.json";

#[derive(Debug, thiserror::Error)]
pub enum SettingsError {
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid settings: {0}")]
    Invalid(String),
}

impl SettingsError {
    pub fn code(&self) -> &'static str {
        match self {
            SettingsError::Io(_) => "IO_ERROR",
            SettingsError::Invalid(_) => "SETTINGS_INVALID",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Settings {
    pub version: u32,
    pub limit: Limit,
    pub encoding: Encoding,
    pub send: SendSettings,
    pub telegram: Telegram,
    pub temp: Temp,
    pub ffmpeg: Ffmpeg,
    pub ui: Ui,
    pub watch: Watch,
    /// Journal settings.
    pub logging: Logging,
    /// Update checks. Absent from files written before they existed: the defaults apply.
    pub updates: Updates,
    /// Paid feature `advanced_encoding`. Absent from older files: the defaults apply.
    pub advanced_encoding: AdvancedEncoding,
}

/// Audio bitrates offered for an encode, in kbit/s.
pub const AUDIO_KBPS_CHOICES: [u32; 4] = [96, 128, 160, 192];

/// Advanced encoding choices. Saved whatever the plan; the media stage reads them only while
/// the license grants `advanced_encoding`, and uses `AUTOMATIC` otherwise.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AdvancedEncoding {
    /// Cap of an encode (`1080p`, `1440p`, `2160p`, `original`); never an upscale.
    pub max_resolution: ResolutionCap,
    /// AAC bitrate of an encode or an audio conversion, one of `AUDIO_KBPS_CHOICES`.
    pub audio_kbps: u32,
}

impl AdvancedEncoding {
    /// What every encode uses without the feature.
    pub const AUTOMATIC: Self = Self {
        max_resolution: ResolutionCap::P1080,
        audio_kbps: 128,
    };
}

impl Default for AdvancedEncoding {
    fn default() -> Self {
        Self::AUTOMATIC
    }
}

/// Update checks (`update` module). The check asks GitHub for the latest release manifest and
/// sends nothing else than the plugin's default request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Updates {
    /// Check at startup, at most once per 24 hours. "Check now" ignores it.
    pub check_automatically: bool,
    /// ISO 8601 UTC time of the last check (automatic or not, successful or not).
    pub last_check_at: Option<String>,
}

/// Journal level: `null` = `info` in a release, `debug` in a debug build. `RUST_LOG` wins.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Logging {
    pub level: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Limit {
    pub premium: Option<bool>,
    pub custom_mb: Option<u64>,
    pub margin_percent: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Encoding {
    pub mode: String,
    pub prefer: String,
    /// Not read: the audio bitrate is `advanced_encoding.audio_kbps` with the feature, 128
    /// without. Kept so that the files that carry it still load.
    pub audio_kbps: u32,
    pub video_codec: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SendSettings {
    pub pause_min_s: u64,
    pub pause_max_s: u64,
    pub watchdog_s: u64,
    /// Prefills the caption of the add panel (variables: `caption::VARIABLES`). Empty: no caption.
    pub caption_template: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApiOverride {
    pub api_id: i64,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Telegram {
    pub api_override: Option<ApiOverride>,
    pub device_name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Temp {
    pub dir: Option<String>,
    pub min_free_gb: u64,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Ffmpeg {
    pub custom_dir: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Ui {
    pub theme: String,
    pub language: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Watch {
    pub stable_seconds: u64,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            version: 1,
            limit: Limit::default(),
            encoding: Encoding::default(),
            send: SendSettings::default(),
            telegram: Telegram::default(),
            temp: Temp::default(),
            ffmpeg: Ffmpeg::default(),
            ui: Ui::default(),
            watch: Watch::default(),
            logging: Logging::default(),
            updates: Updates::default(),
            advanced_encoding: AdvancedEncoding::default(),
        }
    }
}

impl Default for Updates {
    fn default() -> Self {
        Self {
            check_automatically: true,
            last_check_at: None,
        }
    }
}

impl Default for Limit {
    fn default() -> Self {
        Self {
            premium: None,
            custom_mb: None,
            margin_percent: 3.0,
        }
    }
}

impl Default for Encoding {
    fn default() -> Self {
        Self {
            mode: "auto".into(),
            prefer: "gpu".into(),
            audio_kbps: 128,
            video_codec: "h264".into(),
        }
    }
}

impl Default for SendSettings {
    fn default() -> Self {
        Self {
            pause_min_s: 5,
            pause_max_s: 12,
            watchdog_s: 120,
            caption_template: String::new(),
        }
    }
}

impl Default for Temp {
    fn default() -> Self {
        Self {
            dir: None,
            min_free_gb: 5,
        }
    }
}

impl Default for Ui {
    fn default() -> Self {
        Self {
            theme: "system".into(),
            language: "en".into(),
        }
    }
}

impl Default for Watch {
    fn default() -> Self {
        Self { stable_seconds: 10 }
    }
}

impl Settings {
    fn validate(&self) -> Result<(), SettingsError> {
        if self.version != 1 {
            return Err(SettingsError::Invalid(format!("version {}", self.version)));
        }
        if self.send.pause_min_s > self.send.pause_max_s {
            return Err(SettingsError::Invalid(
                "send.pause_min_s > pause_max_s".into(),
            ));
        }
        if !(0.0..50.0).contains(&self.limit.margin_percent) {
            return Err(SettingsError::Invalid("limit.margin_percent".into()));
        }
        if let Some(level) = &self.logging.level
            && !crate::logging::is_level(level)
        {
            return Err(SettingsError::Invalid("logging.level".into()));
        }
        if let Some(at) = &self.updates.last_check_at
            && crate::queue::parse_iso(at).is_none()
        {
            return Err(SettingsError::Invalid("updates.last_check_at".into()));
        }
        if !AUDIO_KBPS_CHOICES.contains(&self.advanced_encoding.audio_kbps) {
            return Err(SettingsError::Invalid(
                "advanced_encoding.audio_kbps".into(),
            ));
        }
        Ok(())
    }

    pub fn pause_range(&self) -> PauseRange {
        PauseRange {
            min: Duration::from_secs(self.send.pause_min_s),
            max: Duration::from_secs(self.send.pause_max_s),
        }
    }
}

/// JSON merge patch (RFC 7396): objects merge, `null` resets a field to its default.
fn merge(target: &mut Value, patch: &Value) {
    match (target, patch) {
        (Value::Object(target), Value::Object(patch)) => {
            for (key, value) in patch {
                if value.is_null() {
                    target.remove(key);
                } else {
                    merge(target.entry(key.clone()).or_insert(Value::Null), value);
                }
            }
        }
        (target, patch) => *target = patch.clone(),
    }
}

/// Dotted paths of the leaves of a merge patch (`send.pause_min_s`), for the journal.
pub fn patch_keys(patch: &Value) -> Vec<String> {
    fn walk(value: &Value, prefix: &str, out: &mut Vec<String>) {
        match value {
            Value::Object(map) if !map.is_empty() => {
                for (key, child) in map {
                    let path = if prefix.is_empty() {
                        key.clone()
                    } else {
                        format!("{prefix}.{key}")
                    };
                    walk(child, &path, out);
                }
            }
            _ if !prefix.is_empty() => out.push(prefix.to_owned()),
            _ => {}
        }
    }
    let mut keys = Vec::new();
    walk(patch, "", &mut keys);
    keys
}

pub struct SettingsStore {
    path: PathBuf,
    current: Mutex<Settings>,
}

impl SettingsStore {
    /// Loads the file, or the defaults if it does not exist yet.
    pub fn load(path: PathBuf) -> Result<Self, SettingsError> {
        let settings = match std::fs::read_to_string(&path) {
            Ok(text) => {
                let settings: Settings = serde_json::from_str(&text)
                    .map_err(|error| SettingsError::Invalid(error.to_string()))?;
                settings.validate()?;
                settings
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Settings::default(),
            Err(error) => return Err(error.into()),
        };
        Ok(Self {
            path,
            current: Mutex::new(settings),
        })
    }

    pub fn path_next_to(database: &Path) -> PathBuf {
        database.with_file_name(FILE_NAME)
    }

    pub fn get(&self) -> Settings {
        self.current
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// Applies a merge patch, validates, writes the file atomically, returns the new settings.
    /// The journal gets the dotted keys of the patch, never the values.
    pub fn set(&self, patch: &Value) -> Result<Settings, SettingsError> {
        let keys = serde_json::json!({ "keys": patch_keys(patch) }).to_string();
        let result = self.apply(patch);
        match &result {
            Ok(_) => {
                tracing::info!(
                    event = "settings_changed",
                    json = keys.as_str(),
                    "settings saved"
                )
            }
            Err(error) => tracing::warn!(
                event = "settings_rejected",
                error_code = error.code(),
                error = %error,
                json = keys.as_str(),
                "settings patch refused"
            ),
        }
        result
    }

    fn apply(&self, patch: &Value) -> Result<Settings, SettingsError> {
        let mut current = self
            .current
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut value = serde_json::to_value(&*current)
            .map_err(|error| SettingsError::Invalid(error.to_string()))?;
        merge(&mut value, patch);
        let updated: Settings = serde_json::from_value(value)
            .map_err(|error| SettingsError::Invalid(error.to_string()))?;
        updated.validate()?;
        let text = serde_json::to_string_pretty(&updated)
            .map_err(|error| SettingsError::Invalid(error.to_string()))?;
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let temp = self.path.with_extension("json.tmp");
        std::fs::write(&temp, text)?;
        std::fs::rename(&temp, &self.path)?;
        *current = updated.clone();
        Ok(updated)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::queue::test_util::TestDir;
    use serde_json::json;

    #[test]
    fn defaults_match_the_spec() {
        let value = serde_json::to_value(Settings::default()).unwrap();
        assert_eq!(
            value,
            json!({
              "version": 1,
              "limit": { "premium": null, "custom_mb": null, "margin_percent": 3.0 },
              "encoding": { "mode": "auto", "prefer": "gpu", "audio_kbps": 128, "video_codec": "h264" },
              "send": { "pause_min_s": 5, "pause_max_s": 12, "watchdog_s": 120, "caption_template": "" },
              "telegram": { "api_override": null, "device_name": null },
              "temp": { "dir": null, "min_free_gb": 5 },
              "ffmpeg": { "custom_dir": null },
              "ui": { "theme": "system", "language": "en" },
              "watch": { "stable_seconds": 10 },
              "logging": { "level": null },
              "updates": { "check_automatically": true, "last_check_at": null },
              "advanced_encoding": { "max_resolution": "1080p", "audio_kbps": 128 }
            })
        );
    }

    #[test]
    fn a_file_without_advanced_encoding_loads_and_choices_are_validated() {
        let dir = TestDir::new("settings-advanced");
        let path = dir.path().join(FILE_NAME);
        let mut old = serde_json::to_value(Settings::default()).unwrap();
        old.as_object_mut().unwrap().remove("advanced_encoding");
        std::fs::write(&path, old.to_string()).unwrap();
        let store = SettingsStore::load(path).unwrap();
        assert_eq!(store.get().advanced_encoding, AdvancedEncoding::AUTOMATIC);

        for (resolution, cap) in [
            ("1440p", ResolutionCap::P1440),
            ("2160p", ResolutionCap::P2160),
            ("original", ResolutionCap::Original),
            ("1080p", ResolutionCap::P1080),
        ] {
            let saved = store
                .set(&json!({ "advanced_encoding": { "max_resolution": resolution, "audio_kbps": 192 } }))
                .unwrap();
            assert_eq!(saved.advanced_encoding.max_resolution, cap);
        }
        for bad in [
            json!({ "advanced_encoding": { "audio_kbps": 100 } }),
            json!({ "advanced_encoding": { "audio_kbps": 0 } }),
            json!({ "advanced_encoding": { "max_resolution": "4k" } }),
            json!({ "advanced_encoding": { "max_resolution": "p1080" } }),
            json!({ "advanced_encoding": { "bitrate": 1 } }),
        ] {
            assert_eq!(
                store.set(&bad).unwrap_err().code(),
                "SETTINGS_INVALID",
                "{bad}"
            );
        }
        assert_eq!(store.get().advanced_encoding.audio_kbps, 192);
    }

    #[test]
    fn a_file_written_before_updates_existed_loads_with_the_defaults() {
        let dir = TestDir::new("settings-migration");
        let path = dir.path().join(FILE_NAME);
        // What this version wrote before the `updates` section: every other section present.
        let mut old = serde_json::to_value(Settings::default()).unwrap();
        old.as_object_mut().unwrap().remove("updates");
        old["ui"]["theme"] = json!("dark");
        std::fs::write(&path, old.to_string()).unwrap();

        let store = SettingsStore::load(path.clone()).unwrap();
        let loaded = store.get();
        assert_eq!(loaded.updates, Updates::default());
        assert!(loaded.updates.check_automatically);
        assert_eq!(loaded.ui.theme, "dark", "the rest of the file is kept");

        // The first save writes the new section next to the old ones.
        store
            .set(&json!({ "updates": { "check_automatically": false } }))
            .unwrap();
        let rewritten: Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(rewritten["updates"]["check_automatically"], json!(false));
        assert_eq!(rewritten["ui"]["theme"], json!("dark"));
    }

    #[test]
    fn the_last_check_time_must_be_a_timestamp() {
        let dir = TestDir::new("settings-updates");
        let store = SettingsStore::load(dir.path().join(FILE_NAME)).unwrap();
        let stamp = "2026-10-12T08:30:00.000Z";
        let saved = store
            .set(&json!({ "updates": { "last_check_at": stamp } }))
            .unwrap();
        assert_eq!(saved.updates.last_check_at.as_deref(), Some(stamp));
        assert!(saved.updates.check_automatically, "untouched field kept");
        let error = store
            .set(&json!({ "updates": { "last_check_at": "yesterday" } }))
            .unwrap_err();
        assert_eq!(error.code(), "SETTINGS_INVALID");
        assert_eq!(store.get(), saved, "a refused patch changes nothing");
    }

    #[test]
    fn logging_level_is_validated_and_patch_keys_carry_no_value() {
        let dir = TestDir::new("settings-logging");
        let store = SettingsStore::load(dir.path().join(FILE_NAME)).unwrap();
        assert_eq!(
            store
                .set(&json!({ "logging": { "level": "debug" } }))
                .unwrap()
                .logging
                .level
                .as_deref(),
            Some("debug")
        );
        let error = store
            .set(&json!({ "logging": { "level": "verbose" } }))
            .unwrap_err();
        assert_eq!(error.code(), "SETTINGS_INVALID");
        assert_eq!(
            patch_keys(&json!({
                "send": { "pause_min_s": 3, "pause_max_s": 9 },
                "telegram": { "api_override": { "api_id": 4242 }, "device_name": null },
                "logging": {}
            })),
            [
                "logging",
                "send.pause_max_s",
                "send.pause_min_s",
                "telegram.api_override.api_id",
                "telegram.device_name"
            ]
        );
    }

    #[test]
    fn the_caption_template_is_empty_by_default_and_persisted() {
        let dir = TestDir::new("settings-caption");
        let path = dir.path().join(FILE_NAME);
        let store = SettingsStore::load(path.clone()).unwrap();
        assert_eq!(store.get().send.caption_template, "");
        store
            .set(&json!({ "send": { "caption_template": "{name} - {resolution}" } }))
            .unwrap();
        let reloaded = SettingsStore::load(path).unwrap();
        assert_eq!(
            reloaded.get().send.caption_template,
            "{name} - {resolution}"
        );
        assert_eq!(reloaded.get().send.watchdog_s, 120, "untouched fields kept");
    }

    #[test]
    fn patch_is_merged_validated_and_persisted() {
        let dir = TestDir::new("settings");
        let path = dir.path().join(FILE_NAME);
        let store = SettingsStore::load(path.clone()).unwrap();
        assert_eq!(store.get(), Settings::default());

        let updated = store
            .set(&json!({ "send": { "pause_min_s": 1 }, "telegram": { "api_override": { "api_id": 42 } } }))
            .unwrap();
        assert_eq!(updated.send.pause_min_s, 1);
        assert_eq!(updated.send.pause_max_s, 12, "untouched fields kept");
        assert_eq!(
            updated.telegram.api_override,
            Some(ApiOverride { api_id: 42 })
        );

        let reloaded = SettingsStore::load(path.clone()).unwrap();
        assert_eq!(reloaded.get(), updated);

        // null resets to the default.
        let reset = store
            .set(&json!({ "telegram": { "api_override": null } }))
            .unwrap();
        assert_eq!(reset.telegram.api_override, None);

        for bad in [
            json!({ "send": { "pause_min_s": 100 } }),
            json!({ "version": 2 }),
            json!({ "unknown": true }),
            json!({ "send": { "pause_min_s": "five" } }),
        ] {
            let error = store.set(&bad).unwrap_err();
            assert_eq!(error.code(), "SETTINGS_INVALID", "{bad}");
        }
        assert_eq!(store.get(), reset, "a refused patch changes nothing");
    }
}
