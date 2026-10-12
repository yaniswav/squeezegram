//! Where `tdjson.dll` is and whether it is the pinned build.
//!
//! Search order: `SQUEEZEGRAM_TDJSON` (that path only, when set), then
//! - release build: next to the executable (the DLL the installer or the portable zip ships),
//!   then `%LOCALAPPDATA%\squeezegram\tdlib\bin\tdjson.dll` as a fallback;
//! - debug build: `%LOCALAPPDATA%\squeezegram\tdlib\bin\tdjson.dll`
//!   (`scripts\install-tdjson.ps1`; `target\debug` has no DLL), then next to the executable.
//!
//! A release looks beside itself first because the bundled DLL is the one its pin was built
//! for: a stale copy left in `%LOCALAPPDATA%` by a development setup or an older version would
//! otherwise win, fail the pin and leave the release without Telegram.
//!
//! The SHA-256 is compared with the pinned TDLib 1.8.68 build; the caller decides what a
//! mismatch means (warning in debug, refusal in release).

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;

/// Variable naming a `tdjson.dll` to use instead of the installed one.
pub const TDJSON_ENV: &str = "SQUEEZEGRAM_TDJSON";
pub const DLL_NAME: &str = "tdjson.dll";
/// TDLib release the pinned DLL was built from.
pub const PINNED_VERSION: &str = "1.8.68";
/// SHA-256 of our own `tdjson.dll` build (TDLib 1.8.68, commit c15d3f5, OpenSSL 3.6.5, zlib 1.3.2).
pub const PINNED_SHA256: &str = "048af1d1c97ef9b860dd5de132c4d2d901d444691ec86fc15012e5896dbde6b8";

/// Where a candidate path comes from (journal field `origin`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Origin {
    Env,
    LocalAppData,
    NextToExe,
}

impl Origin {
    pub fn as_str(self) -> &'static str {
        match self {
            Origin::Env => "env",
            Origin::LocalAppData => "local_app_data",
            Origin::NextToExe => "next_to_exe",
        }
    }
}

/// What the search depends on, injectable for tests.
#[derive(Clone, Debug, Default)]
pub struct SearchPaths {
    /// `SQUEEZEGRAM_TDJSON`, when set and not blank.
    pub env: Option<PathBuf>,
    /// `%LOCALAPPDATA%`.
    pub local_app_data: Option<PathBuf>,
    /// Folder of the executable.
    pub exe_dir: Option<PathBuf>,
    /// Release order: the DLL next to the executable before the `%LOCALAPPDATA%` one.
    pub bundled_first: bool,
}

impl SearchPaths {
    pub fn from_process() -> Self {
        Self {
            env: std::env::var_os(TDJSON_ENV)
                .filter(|value| !value.to_string_lossy().trim().is_empty())
                .map(PathBuf::from),
            local_app_data: std::env::var_os("LOCALAPPDATA").map(PathBuf::from),
            exe_dir: std::env::current_exe()
                .ok()
                .and_then(|exe| exe.parent().map(Path::to_path_buf)),
            bundled_first: !cfg!(debug_assertions),
        }
    }

    /// Candidates in search order: only the variable's path when it is set.
    pub fn candidates(&self) -> Vec<(PathBuf, Origin)> {
        if let Some(path) = &self.env {
            return vec![(path.clone(), Origin::Env)];
        }
        let installed = self
            .local_app_data
            .as_ref()
            .map(|base| (installed_path(base), Origin::LocalAppData));
        let bundled = self
            .exe_dir
            .as_ref()
            .map(|dir| (dir.join(DLL_NAME), Origin::NextToExe));
        let (first, second) = if self.bundled_first {
            (bundled, installed)
        } else {
            (installed, bundled)
        };
        first.into_iter().chain(second).collect()
    }

    /// The first candidate that is a file, or every path searched.
    pub fn locate(&self) -> Result<(PathBuf, Origin), Vec<PathBuf>> {
        let candidates = self.candidates();
        candidates
            .iter()
            .find(|(path, _)| path.is_file())
            .cloned()
            .ok_or_else(|| candidates.into_iter().map(|(path, _)| path).collect())
    }
}

/// `%LOCALAPPDATA%\squeezegram\tdlib\bin\tdjson.dll`, where the install script puts it.
pub fn installed_path(local_app_data: &Path) -> PathBuf {
    local_app_data
        .join("squeezegram")
        .join("tdlib")
        .join("bin")
        .join(DLL_NAME)
}

/// A located DLL and its check against the pinned build.
#[derive(Clone, Debug)]
pub struct Checked {
    pub path: PathBuf,
    pub origin: Origin,
    /// `None` when the file could not be read.
    pub sha256: Option<String>,
    pub pinned: bool,
}

pub fn matches_pinned(sha256: &str) -> bool {
    sha256.eq_ignore_ascii_case(PINNED_SHA256)
}

/// Hashes the DLL (about 0.1 s for 29 MB; cached while size and date do not change).
pub fn check(path: PathBuf, origin: Origin) -> Checked {
    let sha256 = sha256_cached(&path);
    Checked {
        pinned: sha256.as_deref().is_some_and(matches_pinned),
        path,
        origin,
        sha256,
    }
}

type CacheEntry = (PathBuf, u64, Option<SystemTime>, String);

/// `app_start`, the transport choice and the debug export hash the same file.
static HASH_CACHE: Mutex<Option<CacheEntry>> = Mutex::new(None);

pub fn sha256_cached(path: &Path) -> Option<String> {
    let metadata = std::fs::metadata(path).ok()?;
    let (size, modified) = (metadata.len(), metadata.modified().ok());
    let mut cache = HASH_CACHE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some((cached, cached_size, cached_modified, hash)) = cache.as_ref()
        && cached == path
        && *cached_size == size
        && *cached_modified == modified
    {
        return Some(hash.clone());
    }
    let hash = crate::sidecars::file_sha256(path).ok()?;
    *cache = Some((path.to_path_buf(), size, modified, hash.clone()));
    Some(hash)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::tdlib::tests::TempDir;

    #[test]
    fn debug_order_env_then_local_app_data_then_exe() {
        let dir = TempDir::new("tdjson-search");
        let local = dir.path().join("local");
        let exe = dir.path().join("exe");
        let custom = dir.path().join("custom.dll");
        let mut search = SearchPaths {
            env: None,
            local_app_data: Some(local.clone()),
            exe_dir: Some(exe.clone()),
            bundled_first: false,
        };
        let missing = search.locate().unwrap_err();
        assert_eq!(missing, [installed_path(&local), exe.join(DLL_NAME)]);

        std::fs::create_dir_all(&exe).unwrap();
        std::fs::write(exe.join(DLL_NAME), b"x").unwrap();
        assert_eq!(
            search.locate().unwrap(),
            (exe.join(DLL_NAME), Origin::NextToExe)
        );

        let installed = installed_path(&local);
        std::fs::create_dir_all(installed.parent().unwrap()).unwrap();
        std::fs::write(&installed, b"y").unwrap();
        assert_eq!(search.locate().unwrap(), (installed, Origin::LocalAppData));

        // The variable wins, and only its path is searched.
        search.env = Some(custom.clone());
        assert_eq!(search.locate().unwrap_err(), std::slice::from_ref(&custom));
        std::fs::write(&custom, b"z").unwrap();
        assert_eq!(search.locate().unwrap(), (custom, Origin::Env));
    }

    /// A release keeps the DLL it ships: a stale `%LOCALAPPDATA%` copy only serves when none
    /// is beside the executable.
    #[test]
    fn release_order_env_then_exe_then_local_app_data() {
        let dir = TempDir::new("tdjson-search-release");
        let local = dir.path().join("local");
        let exe = dir.path().join("exe");
        let custom = dir.path().join("custom.dll");
        let mut search = SearchPaths {
            env: None,
            local_app_data: Some(local.clone()),
            exe_dir: Some(exe.clone()),
            bundled_first: true,
        };
        assert_eq!(
            search.locate().unwrap_err(),
            [exe.join(DLL_NAME), installed_path(&local)]
        );

        // Only the development copy: used as the fallback.
        let installed = installed_path(&local);
        std::fs::create_dir_all(installed.parent().unwrap()).unwrap();
        std::fs::write(&installed, b"stale").unwrap();
        assert_eq!(
            search.locate().unwrap(),
            (installed.clone(), Origin::LocalAppData)
        );

        // Both: the bundled one wins.
        std::fs::create_dir_all(&exe).unwrap();
        std::fs::write(exe.join(DLL_NAME), b"bundled").unwrap();
        assert_eq!(
            search.locate().unwrap(),
            (exe.join(DLL_NAME), Origin::NextToExe)
        );

        // Without a known executable folder, the fallback alone.
        search.exe_dir = None;
        assert_eq!(search.locate().unwrap(), (installed, Origin::LocalAppData));

        // The variable still wins over both.
        search.exe_dir = Some(exe);
        std::fs::write(&custom, b"z").unwrap();
        search.env = Some(custom.clone());
        assert_eq!(search.locate().unwrap(), (custom, Origin::Env));
    }

    #[test]
    fn the_process_order_follows_the_build() {
        assert_eq!(
            SearchPaths::from_process().bundled_first,
            !cfg!(debug_assertions)
        );
    }

    #[test]
    fn hash_is_compared_with_the_pin() {
        let dir = TempDir::new("tdjson-hash");
        let fake = dir.path().join(DLL_NAME);
        std::fs::write(&fake, b"not the pinned build").unwrap();
        let checked = check(fake.clone(), Origin::Env);
        let sha = checked.sha256.clone().unwrap();
        assert_eq!(sha.len(), 64);
        assert!(!checked.pinned);
        // The cache follows the file's content changes (size differs).
        std::fs::write(&fake, b"another content, longer").unwrap();
        assert_ne!(check(fake.clone(), Origin::Env).sha256.unwrap(), sha);
        assert!(matches_pinned(&PINNED_SHA256.to_ascii_uppercase()));
        assert!(!matches_pinned(&sha));
        let gone = check(dir.path().join("gone.dll"), Origin::Env);
        assert_eq!((gone.sha256, gone.pinned), (None, false));
    }
}
