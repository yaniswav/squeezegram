//! ffmpeg/ffprobe sidecars: resolution (never through `PATH`), pinned download and install.

pub mod discover;
mod download;
pub mod recommend;
mod trust;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use serde::Serialize;
use serde_json::json;

use crate::queue::events::EventSink;
use crate::settings::{SettingsError, SettingsStore};

pub(crate) use download::file_sha256;
pub use download::{Build, install_build};

const EXE: &str = if cfg!(windows) { ".exe" } else { "" };

/// The pinned LGPL build (dated release, never `latest`).
pub fn pinned_build() -> Build {
    Build {
        tag: "autobuild-2026-10-05-13-07".into(),
        url: "https://github.com/BtbN/FFmpeg-Builds/releases/download/autobuild-2026-10-05-13-07/ffmpeg-n9.0.2-22-g46d8f462ee-win64-lgpl-9.0.zip".into(),
        size: 171_475_153,
        sha256: "1c73d2256f0198805daf0f765feab8d3f21d3ab85e8dda0c2bb44ce1e3a126c7".into(),
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SidecarError {
    #[error("ffmpeg or ffprobe not found")]
    Missing,
    #[error("the downloaded archive does not match the pinned SHA-256")]
    Checksum,
    #[error("download failed: {0}")]
    Download(String),
    #[error("could not extract the archive: {0}")]
    Extract(String),
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),
    #[error("an installation is already running")]
    Busy,
    #[error("the folder does not hold a working ffmpeg and ffprobe")]
    Invalid,
    #[error("the folder was not proposed by the last search")]
    NotProposed,
    #[error(
        "the ffmpeg folder can be modified by other users or its permissions cannot be checked"
    )]
    UntrustedFolder,
    #[error("settings: {0}")]
    Settings(#[from] SettingsError),
}

impl SidecarError {
    pub fn code(&self) -> &'static str {
        match self {
            SidecarError::Missing => "FFMPEG_MISSING",
            SidecarError::Checksum => "SIDECAR_CHECKSUM",
            SidecarError::Download(_) => "SIDECAR_DOWNLOAD",
            SidecarError::Extract(_) => "SIDECAR_EXTRACT",
            SidecarError::Io(_) => "IO_ERROR",
            SidecarError::Busy => "SIDECAR_BUSY",
            SidecarError::Invalid => "SIDECAR_INVALID",
            SidecarError::NotProposed => "SIDECAR_CHOICE_STALE",
            SidecarError::UntrustedFolder => "SIDECAR_UNTRUSTED_FOLDER",
            SidecarError::Settings(error) => error.code(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    Custom,
    Installed,
}

impl Source {
    pub fn as_str(&self) -> &'static str {
        match self {
            Source::Custom => "custom",
            Source::Installed => "installed",
        }
    }
}

/// Where the executables are.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved {
    pub ffmpeg: PathBuf,
    pub ffprobe: PathBuf,
    pub source: Source,
}

#[derive(Debug, Clone, Serialize)]
pub struct SidecarStatus {
    pub installed: bool,
    pub source: Option<Source>,
    pub ffmpeg_path: Option<String>,
    pub ffprobe_path: Option<String>,
    pub pinned_tag: String,
    pub pinned_size: u64,
    pub installing: bool,
}

/// `dir` itself or `dir\bin` holding both executables.
fn find_in(dir: &Path) -> Option<(PathBuf, PathBuf)> {
    [dir.to_path_buf(), dir.join("bin")]
        .into_iter()
        .map(|d| {
            (
                d.join(format!("ffmpeg{EXE}")),
                d.join(format!("ffprobe{EXE}")),
            )
        })
        .find(|(ffmpeg, ffprobe)| ffmpeg.is_file() && ffprobe.is_file())
}

pub struct SidecarService {
    /// `%LOCALAPPDATA%\squeezegram\sidecars`.
    base: PathBuf,
    settings: Arc<SettingsStore>,
    build: Build,
    installing: AtomicBool,
    /// The folders of the candidates the last search proposed, by the id the page was given.
    /// `use_proposed` takes nothing else from the page: a path it sent could be any folder
    /// holding an `ffmpeg.exe` to run, a network share included.
    proposed: Mutex<Vec<(u64, PathBuf)>>,
    last_id: AtomicU64,
}

impl SidecarService {
    pub fn new(base: PathBuf, settings: Arc<SettingsStore>) -> Self {
        Self::with_build(base, settings, pinned_build())
    }

    pub fn with_build(base: PathBuf, settings: Arc<SettingsStore>, build: Build) -> Self {
        Self {
            base,
            settings,
            build,
            installing: AtomicBool::new(false),
            proposed: Mutex::new(Vec::new()),
            last_id: AtomicU64::new(0),
        }
    }

    /// Numbers the candidates of a search and keeps their folders, replacing those of the
    /// previous search (its ids are no longer accepted).
    pub fn remember(&self, candidates: &mut [discover::Candidate]) {
        let mut kept = Vec::with_capacity(candidates.len());
        for candidate in candidates {
            candidate.id = self.last_id.fetch_add(1, Ordering::SeqCst) + 1;
            kept.push((candidate.id, PathBuf::from(&candidate.dir)));
        }
        *self
            .proposed
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = kept;
    }

    /// The pinned build's executables, when it is installed.
    pub fn installed_pair(&self) -> Option<(PathBuf, PathBuf)> {
        find_in(&self.install_dir())
    }

    pub fn default_base() -> Option<PathBuf> {
        let local = std::env::var_os("LOCALAPPDATA")?;
        Some(PathBuf::from(local).join("squeezegram").join("sidecars"))
    }

    fn install_dir(&self) -> PathBuf {
        self.base.join("ffmpeg")
    }

    /// The user's directory if set (an invalid one is an error, not a silent fallback),
    /// else the installed build.
    pub fn resolve(&self) -> Result<Resolved, SidecarError> {
        let custom = self.settings.get().ffmpeg.custom_dir;
        if let Some(dir) = custom.filter(|dir| !dir.trim().is_empty()) {
            let (ffmpeg, ffprobe) = find_in(Path::new(&dir)).ok_or(SidecarError::Missing)?;
            let (ffmpeg, ffprobe) = trust::check_pair(&ffmpeg, &ffprobe)?;
            return Ok(Resolved {
                ffmpeg,
                ffprobe,
                source: Source::Custom,
            });
        }
        let (ffmpeg, ffprobe) = find_in(&self.install_dir()).ok_or(SidecarError::Missing)?;
        Ok(Resolved {
            ffmpeg,
            ffprobe,
            source: Source::Installed,
        })
    }

    pub fn status(&self) -> SidecarStatus {
        let resolved = self.resolve().ok();
        SidecarStatus {
            installed: resolved.is_some(),
            source: resolved.as_ref().map(|r| r.source.clone()),
            ffmpeg_path: resolved
                .as_ref()
                .map(|r| r.ffmpeg.to_string_lossy().into_owned()),
            ffprobe_path: resolved
                .as_ref()
                .map(|r| r.ffprobe.to_string_lossy().into_owned()),
            pinned_tag: self.build.tag.clone(),
            pinned_size: self.build.size,
            installing: self.installing.load(Ordering::Relaxed),
        }
    }

    /// Records the folder of a candidate of the last search, after running `-version` on both
    /// tools. `SIDECAR_CHOICE_STALE` for an id that search did not give (an older search, or
    /// one the page made up). The `PATH` is not involved.
    pub async fn use_proposed(&self, id: u64) -> Result<SidecarStatus, SidecarError> {
        let dir = self
            .proposed
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .iter()
            .find(|(proposed, _)| *proposed == id)
            .map(|(_, dir)| dir.clone());
        let Some(dir) = dir else {
            tracing::warn!(
                event = "sidecar_use",
                error_code = SidecarError::NotProposed.code(),
                "no such candidate in the last search"
            );
            return Err(SidecarError::NotProposed);
        };
        self.record_dir(&dir).await
    }

    /// Records a folder the user picked in the native dialog (opened by the core, not by the
    /// page), with the same checks.
    pub async fn use_picked(&self, dir: &Path) -> Result<SidecarStatus, SidecarError> {
        self.record_dir(dir).await
    }

    async fn record_dir(&self, dir: &Path) -> Result<SidecarStatus, SidecarError> {
        let dir_text = dir.to_string_lossy().into_owned();
        let dir_text = dir_text.trim();
        if dir_text.is_empty() || find_in(Path::new(dir_text)).is_none() {
            tracing::warn!(
                event = "sidecar_use",
                dir = dir_text,
                error_code = SidecarError::Missing.code(),
                "no ffmpeg/ffprobe pair in this folder"
            );
            return Err(SidecarError::Missing);
        }
        let (ffmpeg, ffprobe) = find_in(Path::new(dir_text)).ok_or(SidecarError::Missing)?;
        let (ffmpeg, ffprobe) = trust::check_pair(&ffmpeg, &ffprobe).inspect_err(|error| {
            tracing::warn!(
                event = "sidecar_use",
                error_code = error.code(),
                "custom ffmpeg folder refused before execution"
            );
        })?;
        let Some(probed) = discover::probe_pair(&ffmpeg, &ffprobe).await else {
            tracing::warn!(
                event = "sidecar_use",
                dir = dir_text,
                error_code = SidecarError::Invalid.code(),
                "the pair in this folder does not run"
            );
            return Err(SidecarError::Invalid);
        };
        self.settings
            .set(&json!({ "ffmpeg": { "custom_dir": dir_text } }))?;
        tracing::info!(
            event = "sidecar_use",
            dir = dir_text,
            version = probed.version.as_str(),
            license = ?probed.license,
            "custom ffmpeg folder recorded"
        );
        Ok(self.status())
    }

    /// Downloads and installs the pinned build (blocking; call from a blocking thread).
    pub fn install_blocking(&self, sink: &dyn EventSink) -> Result<SidecarStatus, SidecarError> {
        if self.installing.swap(true, Ordering::SeqCst) {
            return Err(SidecarError::Busy);
        }
        let started = std::time::Instant::now();
        tracing::info!(
            event = "sidecar_install_start",
            tag = self.build.tag.as_str(),
            url = self.build.url.as_str(),
            size = self.build.size,
            "ffmpeg installation started"
        );
        let result = install_build(&self.build, &self.base, "ffmpeg", sink);
        self.installing.store(false, Ordering::SeqCst);
        let dur_ms = started.elapsed().as_millis() as u64;
        match &result {
            Ok(()) => tracing::info!(
                event = "sidecar_installed",
                dur_ms,
                tag = self.build.tag.as_str(),
                "ffmpeg installed"
            ),
            Err(error) => {
                tracing::error!(event = "sidecar_install_failed", dur_ms, error_code = error.code(), error = %error, "ffmpeg installation failed")
            }
        }
        result?;
        Ok(self.status())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::queue::test_util::TestDir;

    fn service(dir: &TestDir) -> SidecarService {
        let settings = Arc::new(SettingsStore::load(dir.path().join("settings.json")).unwrap());
        SidecarService::new(dir.path().join("sidecars"), settings)
    }

    fn candidate(dir: &Path) -> discover::Candidate {
        discover::Candidate {
            id: 0,
            dir: dir.to_string_lossy().into_owned(),
            ffmpeg_path: String::new(),
            ffprobe_path: String::new(),
            origin: discover::Origin::Path,
            verified: false,
            version: "test".into(),
            license: discover::License::Unknown,
            encoders: Vec::new(),
            working_encoders: Vec::new(),
            score: 0,
            recommended: false,
            reasons: Vec::new(),
        }
    }

    fn fake_pair(dir: &Path) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join(format!("ffmpeg{EXE}")), b"not an executable").unwrap();
        std::fs::write(dir.join(format!("ffprobe{EXE}")), b"not an executable").unwrap();
    }

    #[tokio::test]
    async fn a_picked_folder_is_refused_when_missing_or_not_working_and_records_nothing() {
        let dir = TestDir::new("sidecars-use");
        let service = service(&dir);
        assert!(matches!(
            service.use_picked(Path::new("  ")).await,
            Err(SidecarError::Missing)
        ));
        assert!(matches!(
            service.use_picked(dir.path()).await,
            Err(SidecarError::Missing)
        ));
        let fake = dir.path().join("fake");
        fake_pair(&fake);
        let error = service.use_picked(&fake).await.unwrap_err();
        assert_eq!(error.code(), "SIDECAR_INVALID");
        assert_eq!(service.settings.get().ffmpeg.custom_dir, None);
        assert!(!service.status().installed);
    }

    #[tokio::test]
    async fn only_a_candidate_of_the_last_search_can_be_used() {
        let dir = TestDir::new("sidecars-proposed");
        let service = service(&dir);
        let fake = dir.path().join("fake");
        fake_pair(&fake);

        // Nothing proposed yet: no id is accepted, whatever the folder holds.
        let error = service.use_proposed(1).await.unwrap_err();
        assert_eq!(error.code(), "SIDECAR_CHOICE_STALE");

        let mut first = vec![candidate(&fake)];
        service.remember(&mut first);
        let id = first[0].id;
        assert_ne!(id, 0);
        // A proposed folder is still checked: this one does not run.
        let error = service.use_proposed(id).await.unwrap_err();
        assert_eq!(error.code(), "SIDECAR_INVALID");
        // An id nobody was given.
        let error = service.use_proposed(id + 100).await.unwrap_err();
        assert_eq!(error.code(), "SIDECAR_CHOICE_STALE");

        // A new search replaces the list: the earlier id is no longer valid, and the new one
        // differs even for the same folder.
        let mut second = vec![candidate(&fake)];
        service.remember(&mut second);
        assert_ne!(second[0].id, id);
        let error = service.use_proposed(id).await.unwrap_err();
        assert_eq!(error.code(), "SIDECAR_CHOICE_STALE");
        assert_eq!(service.settings.get().ffmpeg.custom_dir, None);
    }

    /// Against the real ffmpeg of the session scratchpad; run with `--ignored`.
    #[tokio::test]
    #[ignore = "needs a real ffmpeg at a machine-specific path"]
    async fn use_picked_records_a_real_ffmpeg_and_the_status_resolves_it() {
        let dir = TestDir::new("sidecars-use-real");
        let service = service(&dir);
        let bin_owned = std::env::var("SQUEEZEGRAM_TEST_FFMPEG_DIR")
            .expect("SQUEEZEGRAM_TEST_FFMPEG_DIR must point to the bin folder of an ffmpeg build");
        let bin = bin_owned.as_str();
        let status = service.use_picked(Path::new(bin)).await.unwrap();
        assert!(status.installed);
        assert_eq!(status.source, Some(Source::Custom));
        assert_eq!(
            service.settings.get().ffmpeg.custom_dir.as_deref(),
            Some(bin)
        );
    }
}
