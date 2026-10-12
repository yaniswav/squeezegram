//! Where `tdjson.dll` is and whether it is the pinned build.
//!
//! Search order: `SQUEEZEGRAM_TDJSON` (that path only, when set), then
//! `%LOCALAPPDATA%\squeezegram\tdlib\bin\tdjson.dll` (`scripts\install-tdjson.ps1`), then next
//! to the executable (installer). The SHA-256 is compared with the pinned TDLib 1.8.68 build;
//! the caller decides what a mismatch means (warning in debug, refusal in release).

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
        }
    }

    /// Candidates in search order: only the variable's path when it is set.
    pub fn candidates(&self) -> Vec<(PathBuf, Origin)> {
        if let Some(path) = &self.env {
            return vec![(path.clone(), Origin::Env)];
        }
        let mut list = Vec::new();
        if let Some(base) = &self.local_app_data {
            list.push((installed_path(base), Origin::LocalAppData));
        }
        if let Some(dir) = &self.exe_dir {
            list.push((dir.join(DLL_NAME), Origin::NextToExe));
        }
        list
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
    fn search_order_env_then_local_app_data_then_exe() {
        let dir = TempDir::new("tdjson-search");
        let local = dir.path().join("local");
        let exe = dir.path().join("exe");
        let custom = dir.path().join("custom.dll");
        let mut search = SearchPaths {
            env: None,
            local_app_data: Some(local.clone()),
            exe_dir: Some(exe.clone()),
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
