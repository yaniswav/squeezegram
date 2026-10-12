//! Versioned settings, `settings.v1`, stored as JSON next to the database.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;

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
    pub audio_kbps: u32,
    pub video_codec: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SendSettings {
    pub pause_min_s: u64,
    pub pause_max_s: u64,
    pub watchdog_s: u64,
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
              "send": { "pause_min_s": 5, "pause_max_s": 12, "watchdog_s": 120 },
              "telegram": { "api_override": null, "device_name": null },
              "temp": { "dir": null, "min_free_gb": 5 },
              "ffmpeg": { "custom_dir": null },
              "ui": { "theme": "system", "language": "en" },
              "watch": { "stable_seconds": 10 },
              "logging": { "level": null }
            })
        );
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
