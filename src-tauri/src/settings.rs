//! Versioned settings, `settings.v1`, stored as JSON next to the database.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::media::plan::{EncodePreset, ResolutionCap, TARGET_MB_RANGE, VideoCodec};
use crate::queue::orchestrator::PauseRange;
use crate::transport::proxy::{Proxy, ProxyKind};

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
    /// Paid feature `background`. Absent from older files: the defaults apply.
    pub background: Background,
    /// Proxy for Telegram. Absent from older files: no proxy. Changed only through
    /// `proxy_set`, which also stores the secret and applies it to the transport.
    pub network: Network,
    /// What leaves with a file sent without re-encoding. Absent from older files: the defaults.
    pub privacy: Privacy,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Privacy {
    /// "Remove location and camera data": a JPEG, PNG or WebP sent as it is goes out as a copy
    /// without its Exif, XMP and the like, and an MP4 sent without re-encoding whose container
    /// names a place or a camera is remuxed without its metadata (`media::metadata`, `plan`).
    pub strip_metadata: bool,
}

impl Default for Privacy {
    fn default() -> Self {
        Self {
            strip_metadata: true,
        }
    }
}

/// `proxy: null`: direct connection. The secret of the proxy is not here (`transport::proxy`).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Network {
    pub proxy: Option<ProxySettings>,
}

/// What `settings.json` keeps of a proxy: everything but its secret.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProxySettings {
    pub kind: ProxyKind,
    pub host: String,
    /// `u32` so that 0 or 70000 reach `validate` and are refused there with the others.
    pub port: u32,
    /// Empty for MTProto and for a SOCKS5/HTTP proxy without authentication.
    #[serde(default)]
    pub username: String,
}

/// What closing the window does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CloseAction {
    /// The app exits. The only behavior without the license.
    #[default]
    Quit,
    /// The window hides and the app keeps running from the notification area.
    Tray,
}

/// Background mode (`background` module). Saved whatever the plan; `Tray` only takes effect
/// while the license grants `background`.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Background {
    pub close_action: CloseAction,
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
    /// `h264` or `hevc`. HEVC needs a GPU encoder that passed its test encode; without one the
    /// plan falls back to H.264 (`media::stage`). Replaces `encoding.video_codec`, which was
    /// never read.
    pub video_codec: VideoCodec,
    /// Speed/quality of the encoder (`faster`, `balanced`, `quality`), mapped per encoder.
    pub preset: EncodePreset,
    /// Custom output size in MiB, within `TARGET_MB_RANGE`. `null`: fill the upload limit. At
    /// or above the effective limit it is the same as `null`.
    pub target_mb: Option<u64>,
}

impl AdvancedEncoding {
    /// What every encode uses without the feature.
    pub const AUTOMATIC: Self = Self {
        max_resolution: ResolutionCap::P1080,
        audio_kbps: 128,
        video_codec: VideoCodec::H264,
        preset: EncodePreset::Balanced,
        target_mb: None,
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
    /// Not read either: the codec is `advanced_encoding.video_codec` with the feature, H.264
    /// without. A typed enum there, a free string here that every file already carries.
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
    /// `device_model` of `setTdlibParameters`, shown in Telegram > Devices. `null` or blank:
    /// "squeezegram <machine>". Read once, when the transport is built at startup.
    pub device_name: Option<String>,
    /// The user read the warning about sending to groups and channels (F33) and accepted it
    /// once. Only the interface reads it: the core never blocks a send on it.
    pub group_risk_acknowledged: bool,
}

/// Longest `telegram.device_name`, in characters. TDLib documents no limit for `device_model`
/// (only "non-empty"); Telegram's device list shows a short line, so a long name would only be
/// cut there. Same bound in the page (`DEVICE_NAME_MAX`).
pub const DEVICE_NAME_MAX_CHARS: usize = 64;

/// A device name the page may save: at most `DEVICE_NAME_MAX_CHARS` once trimmed, and nothing
/// that would show differently than typed in Telegram's device list: no control character
/// (Cc, a line break), line or paragraph separator (Zl, Zp), or format character (Cf: a
/// right-to-left override such as U+202E could disguise the name).
fn is_device_name(name: &str) -> bool {
    let name = name.trim();
    name.chars().count() <= DEVICE_NAME_MAX_CHARS && !name.chars().any(is_hidden_mark)
}

/// Cc, Zl, Zp and Cf (Unicode 15.1), except what ordinary text needs: the zero-width joiner and
/// non-joiner (emoji sequences, Persian, Indic scripts) and the emoji tag characters (flags of
/// England, Scotland, Wales). The page applies the same rule (`HIDDEN_MARK`).
fn is_hidden_mark(c: char) -> bool {
    matches!(
        c,
        '\u{2028}'
            | '\u{2029}'
            | '\u{00AD}'
            | '\u{0600}'..='\u{0605}'
            | '\u{061C}'
            | '\u{06DD}'
            | '\u{070F}'
            | '\u{0890}'..='\u{0891}'
            | '\u{08E2}'
            | '\u{180E}'
            | '\u{200B}'
            | '\u{200E}'..='\u{200F}'
            | '\u{202A}'..='\u{202E}'
            | '\u{2060}'..='\u{2064}'
            | '\u{2066}'..='\u{206F}'
            | '\u{FEFF}'
            | '\u{FFF9}'..='\u{FFFB}'
            | '\u{110BD}'
            | '\u{110CD}'
            | '\u{13430}'..='\u{1343F}'
            | '\u{1BCA0}'..='\u{1BCA3}'
            | '\u{1D173}'..='\u{1D17A}'
            | '\u{E0001}'
    ) || c.is_control()
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
            background: Background::default(),
            network: Network::default(),
            privacy: Privacy::default(),
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
        if let Some(mb) = self.advanced_encoding.target_mb
            && !TARGET_MB_RANGE.contains(&mb)
        {
            return Err(SettingsError::Invalid("advanced_encoding.target_mb".into()));
        }
        if let Some(name) = &self.telegram.device_name
            && !is_device_name(name)
        {
            return Err(SettingsError::Invalid("telegram.device_name".into()));
        }
        if let Some(proxy) = &self.network.proxy {
            // The same checks as `proxy_set`, with a placeholder secret: the real one is not
            // in the settings.
            let secret = match proxy.kind {
                ProxyKind::Mtproto => "00000000000000000000000000000000",
                ProxyKind::Socks5 | ProxyKind::Http => "",
            };
            Proxy::new(proxy.kind, &proxy.host, proxy.port, &proxy.username, secret).map_err(
                |field| SettingsError::Invalid(format!("network.proxy.{}", field.as_str())),
            )?;
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

/// Why a folder cannot hold the temporary files (`temp.dir`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TempDirProblem {
    /// Relative, or without a drive.
    NotAbsolute,
    /// A network share (`\server\share`, a mapped network drive) or a device path: encodes
    /// there would be slow, and a share can vanish or be somebody else's.
    NotLocal,
    /// Does not exist or is not a folder.
    Missing,
    /// A file cannot be created in it.
    NotWritable,
}

impl TempDirProblem {
    pub fn as_str(self) -> &'static str {
        match self {
            TempDirProblem::NotAbsolute => "not_absolute",
            TempDirProblem::NotLocal => "not_local",
            TempDirProblem::Missing => "missing",
            TempDirProblem::NotWritable => "not_writable",
        }
    }
}

/// A folder is fit to be `temp.dir` when it is an absolute path on a local drive, exists, and
/// a file can be created in it. The check is made by the core on the folder the user picked
/// in the native dialog; the page never sends a path for this setting.
pub fn check_temp_dir(dir: &Path) -> Result<(), TempDirProblem> {
    check_local(dir)?;
    match std::fs::metadata(dir) {
        Ok(meta) if meta.is_dir() => {}
        _ => return Err(TempDirProblem::Missing),
    }
    let probe = dir.join(format!(".squeezegram-write-check-{}", std::process::id()));
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&probe)
    {
        Ok(file) => {
            drop(file);
            let _ = std::fs::remove_file(&probe);
            Ok(())
        }
        Err(_) => Err(TempDirProblem::NotWritable),
    }
}

#[cfg(windows)]
fn check_local(dir: &Path) -> Result<(), TempDirProblem> {
    use std::path::{Component, Prefix};

    use windows::Win32::Storage::FileSystem::GetDriveTypeW;
    use windows::core::HSTRING;

    /// `DRIVE_REMOTE` of `GetDriveTypeW`.
    const DRIVE_REMOTE: u32 = 4;

    let mut parts = dir.components();
    let (Some(Component::Prefix(prefix)), Some(Component::RootDir)) = (parts.next(), parts.next())
    else {
        return Err(TempDirProblem::NotAbsolute);
    };
    let letter = match prefix.kind() {
        Prefix::Disk(letter) | Prefix::VerbatimDisk(letter) => letter,
        // `\server\share`, `\?\UNC\...`, `\.\device`, `\?\Volume{...}`.
        _ => return Err(TempDirProblem::NotLocal),
    };
    let root = HSTRING::from(format!("{}:\\", char::from(letter).to_ascii_uppercase()));
    // SAFETY: `root` is a NUL-terminated wide string that outlives the call.
    if unsafe { GetDriveTypeW(&root) } == DRIVE_REMOTE {
        return Err(TempDirProblem::NotLocal);
    }
    Ok(())
}

#[cfg(not(windows))]
fn check_local(dir: &Path) -> Result<(), TempDirProblem> {
    if dir.is_absolute() {
        Ok(())
    } else {
        Err(TempDirProblem::NotAbsolute)
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
              "telegram": { "api_override": null, "device_name": null, "group_risk_acknowledged": false },
              "temp": { "dir": null, "min_free_gb": 5 },
              "ffmpeg": { "custom_dir": null },
              "ui": { "theme": "system", "language": "en" },
              "watch": { "stable_seconds": 10 },
              "logging": { "level": null },
              "updates": { "check_automatically": true, "last_check_at": null },
              "advanced_encoding": {
                "max_resolution": "1080p", "audio_kbps": 128, "video_codec": "h264",
                "preset": "balanced", "target_mb": null
              },
              "background": { "close_action": "quit" },
              "network": { "proxy": null },
              "privacy": { "strip_metadata": true }
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
    fn codec_preset_and_target_load_from_older_files_and_are_validated() {
        let dir = TestDir::new("settings-advanced-codec");
        let path = dir.path().join(FILE_NAME);
        // What the version with only a cap and an audio bitrate wrote.
        let mut old = serde_json::to_value(Settings::default()).unwrap();
        old["advanced_encoding"] = json!({ "max_resolution": "1440p", "audio_kbps": 160 });
        std::fs::write(&path, old.to_string()).unwrap();
        let store = SettingsStore::load(path.clone()).unwrap();
        let loaded = store.get().advanced_encoding;
        assert_eq!(
            (loaded.max_resolution, loaded.audio_kbps),
            (ResolutionCap::P1440, 160),
            "the old choices are kept"
        );
        assert_eq!(
            (loaded.video_codec, loaded.preset, loaded.target_mb),
            (VideoCodec::H264, EncodePreset::Balanced, None),
            "the new ones take today's behavior"
        );
        // `encoding.video_codec` of the very first files is still accepted and never read.
        let mut first = serde_json::to_value(Settings::default()).unwrap();
        first["encoding"]["video_codec"] = json!("hevc");
        std::fs::write(&path, first.to_string()).unwrap();
        let store = SettingsStore::load(path).unwrap();
        assert_eq!(
            store.get().advanced_encoding.video_codec,
            VideoCodec::H264,
            "the legacy string does not choose the codec"
        );

        let saved = store
            .set(&json!({ "advanced_encoding": {
                "video_codec": "hevc", "preset": "quality", "target_mb": 40
            } }))
            .unwrap();
        assert_eq!(
            (
                saved.advanced_encoding.video_codec,
                saved.advanced_encoding.preset,
                saved.advanced_encoding.target_mb
            ),
            (VideoCodec::Hevc, EncodePreset::Quality, Some(40))
        );
        // `null` goes back to filling the limit.
        let reset = store
            .set(&json!({ "advanced_encoding": { "target_mb": null } }))
            .unwrap();
        assert_eq!(reset.advanced_encoding.target_mb, None);
        assert_eq!(reset.advanced_encoding.video_codec, VideoCodec::Hevc);

        for bad in [
            json!({ "advanced_encoding": { "video_codec": "av1" } }),
            json!({ "advanced_encoding": { "video_codec": "H264" } }),
            json!({ "advanced_encoding": { "preset": "slow" } }),
            json!({ "advanced_encoding": { "target_mb": 0 } }),
            json!({ "advanced_encoding": { "target_mb": 4001 } }),
            json!({ "advanced_encoding": { "target_mb": -5 } }),
            json!({ "advanced_encoding": { "target_mb": 1.5 } }),
        ] {
            assert_eq!(
                store.set(&bad).unwrap_err().code(),
                "SETTINGS_INVALID",
                "{bad}"
            );
        }
        assert_eq!(store.get(), reset, "a refused patch changes nothing");
    }

    #[test]
    fn a_file_without_background_quits_on_close_and_the_choice_is_validated() {
        let dir = TestDir::new("settings-background");
        let path = dir.path().join(FILE_NAME);
        let mut old = serde_json::to_value(Settings::default()).unwrap();
        old.as_object_mut().unwrap().remove("background");
        std::fs::write(&path, old.to_string()).unwrap();
        let store = SettingsStore::load(path.clone()).unwrap();
        assert_eq!(store.get().background.close_action, CloseAction::Quit);

        let saved = store
            .set(&json!({ "background": { "close_action": "tray" } }))
            .unwrap();
        assert_eq!(saved.background.close_action, CloseAction::Tray);
        assert_eq!(SettingsStore::load(path).unwrap().get(), saved);
        for bad in [
            json!({ "background": { "close_action": "minimize" } }),
            json!({ "background": { "close_action": true } }),
            json!({ "background": { "autostart": true } }),
        ] {
            assert_eq!(
                store.set(&bad).unwrap_err().code(),
                "SETTINGS_INVALID",
                "{bad}"
            );
        }
        assert_eq!(store.get(), saved);
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
    fn the_device_name_is_bounded_and_single_line() {
        let dir = TestDir::new("settings-device");
        let store = SettingsStore::load(dir.path().join(FILE_NAME)).unwrap();
        // 64 characters, counted as characters (an emoji is one), surrounding spaces ignored.
        let longest = format!("  {}🎬  ", "é".repeat(DEVICE_NAME_MAX_CHARS - 1));
        let saved = store
            .set(&json!({ "telegram": { "device_name": longest } }))
            .unwrap();
        assert_eq!(
            saved.telegram.device_name.as_deref(),
            Some(longest.as_str())
        );
        // Joiners and emoji tags are ordinary text: a family emoji, a Persian name, a flag.
        for fine in [
            "👨\u{200D}👩\u{200D}👧 PC",
            "می\u{200C}خواهم",
            "🏴\u{E0067}\u{E0062}\u{E0065}\u{E006E}\u{E0067}\u{E007F} laptop",
        ] {
            assert!(
                store
                    .set(&json!({ "telegram": { "device_name": fine } }))
                    .is_ok(),
                "{fine:?}"
            );
        }
        let reset = store
            .set(&json!({ "telegram": { "device_name": null } }))
            .unwrap();
        assert_eq!(reset.telegram.device_name, None);
        for bad in [
            "x".repeat(DEVICE_NAME_MAX_CHARS + 1),
            "Office\nPC".to_owned(),
            "tab\there".to_owned(),
            // Shows as "PC gnalb" reversed: a disguised name.
            "PC \u{202E}blang".to_owned(),
            "line\u{2028}break".to_owned(),
            "para\u{2029}graph".to_owned(),
            "zero\u{200B}width".to_owned(),
            "\u{FEFF}bom".to_owned(),
        ] {
            let error = store
                .set(&json!({ "telegram": { "device_name": bad } }))
                .unwrap_err();
            assert_eq!(error.code(), "SETTINGS_INVALID", "{bad:?}");
        }
        assert_eq!(store.get(), reset, "a refused patch changes nothing");
    }

    #[test]
    fn metadata_is_removed_unless_turned_off_older_files_included() {
        let dir = TestDir::new("settings-privacy");
        let path = dir.path().join(FILE_NAME);
        let mut old = serde_json::to_value(Settings::default()).unwrap();
        old.as_object_mut().unwrap().remove("privacy");
        std::fs::write(&path, old.to_string()).unwrap();
        let store = SettingsStore::load(path).unwrap();
        assert!(
            store.get().privacy.strip_metadata,
            "on for files written before"
        );
        let off = store
            .set(&json!({ "privacy": { "strip_metadata": false } }))
            .unwrap();
        assert!(!off.privacy.strip_metadata);
        assert_eq!(
            store
                .set(&json!({ "privacy": { "strip_metadata": "yes" } }))
                .unwrap_err()
                .code(),
            "SETTINGS_INVALID"
        );
    }

    #[test]
    fn a_proxy_is_saved_without_secret_and_validated() {
        let dir = TestDir::new("settings-proxy");
        let path = dir.path().join(FILE_NAME);
        let mut old = serde_json::to_value(Settings::default()).unwrap();
        old.as_object_mut().unwrap().remove("network");
        std::fs::write(&path, old.to_string()).unwrap();
        let store = SettingsStore::load(path.clone()).unwrap();
        assert_eq!(store.get().network.proxy, None, "older files: no proxy");

        let saved = store
            .set(&json!({ "network": { "proxy": {
                "kind": "mtproto", "host": "mt.example", "port": 443
            } } }))
            .unwrap();
        let proxy = saved.network.proxy.unwrap();
        assert_eq!(
            (proxy.kind, proxy.port, proxy.username.as_str()),
            (ProxyKind::Mtproto, 443, "")
        );
        for bad in [
            json!({ "network": { "proxy": { "kind": "socks5", "host": "", "port": 1080 } } }),
            json!({ "network": { "proxy": { "kind": "socks5", "host": "h.org", "port": 0 } } }),
            json!({ "network": { "proxy": { "kind": "socks5", "host": "h.org", "port": 65536 } } }),
            json!({ "network": { "proxy": { "kind": "mtproto", "host": "h.org", "port": 1, "username": "u" } } }),
            json!({ "network": { "proxy": { "kind": "vpn", "host": "h.org", "port": 1 } } }),
            json!({ "network": { "proxy": { "kind": "socks5", "host": "h.org", "port": 1, "password": "p" } } }),
        ] {
            assert_eq!(
                store.set(&bad).unwrap_err().code(),
                "SETTINGS_INVALID",
                "{bad}"
            );
        }
        let direct = store.set(&json!({ "network": { "proxy": null } })).unwrap();
        assert_eq!(direct.network.proxy, None);
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
    fn the_group_risk_acknowledgement_is_persisted_and_old_files_read_it_as_not_given() {
        let dir = TestDir::new("settings-group-risk");
        let path = dir.path().join(FILE_NAME);
        // A file from before the warning existed: the telegram section has no such field.
        let mut old = serde_json::to_value(Settings::default()).unwrap();
        old["telegram"]
            .as_object_mut()
            .unwrap()
            .remove("group_risk_acknowledged");
        std::fs::write(&path, old.to_string()).unwrap();
        let store = SettingsStore::load(path.clone()).unwrap();
        assert!(!store.get().telegram.group_risk_acknowledged);

        store
            .set(&json!({ "telegram": { "group_risk_acknowledged": true } }))
            .unwrap();
        let reloaded = SettingsStore::load(path).unwrap();
        assert!(reloaded.get().telegram.group_risk_acknowledged);
        assert_eq!(reloaded.get().telegram.device_name, None, "others kept");
        assert_eq!(
            store
                .set(&json!({ "telegram": { "group_risk_acknowledged": "yes" } }))
                .unwrap_err()
                .code(),
            "SETTINGS_INVALID"
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
