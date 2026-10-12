//! Telegram API identity (`api_id` + `api_hash`).
//!
//! - The product's identity is injected at build time (`build.rs`, read with `option_env!`).
//! - A personal identity overrides it: `telegram.api_override.api_id` in the settings,
//!   and the `api_hash` protected by DPAPI for the current Windows user in `api_hash.bin` of
//!   the TDLib folder, like the database key (`key.rs`); never in `settings.json`.
//!
//! Neither value ever reaches a log or a `Debug`: the journal only gets presence and lengths.

use std::fmt;
use std::fs;
use std::io;
use std::path::Path;

use super::key::{protect, unprotect};
use crate::transport::TransportError;

pub const API_HASH_FILE: &str = "api_hash.bin";
/// Telegram's `api_hash` is 32 hexadecimal characters.
const API_HASH_LEN: usize = 32;

/// Where the identity in use comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IdentitySource {
    /// Injected at build time.
    Embedded,
    /// `telegram.api_override` + DPAPI `api_hash.bin`.
    Override,
}

impl IdentitySource {
    pub fn as_str(self) -> &'static str {
        match self {
            IdentitySource::Embedded => "embedded",
            IdentitySource::Override => "override",
        }
    }
}

/// A validated identity, ready for `setTdlibParameters`.
#[derive(Clone, PartialEq, Eq)]
pub struct ApiIdentity {
    pub api_id: i32,
    pub api_hash: String,
}

impl fmt::Debug for ApiIdentity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ApiIdentity(<redacted>)")
    }
}

/// Why an identity is unusable; the text names the field, never its value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IdentityProblem {
    MissingApiId,
    InvalidApiId,
    MissingApiHash,
    InvalidApiHash,
    /// Override set in the settings, but no readable `api_hash.bin`.
    OverrideHashUnavailable,
}

impl IdentityProblem {
    pub fn as_str(self) -> &'static str {
        match self {
            IdentityProblem::MissingApiId => "api_id_missing",
            IdentityProblem::InvalidApiId => "api_id_invalid",
            IdentityProblem::MissingApiHash => "api_hash_missing",
            IdentityProblem::InvalidApiHash => "api_hash_invalid",
            IdentityProblem::OverrideHashUnavailable => "override_api_hash_unavailable",
        }
    }

    pub fn into_error(self) -> TransportError {
        TransportError::NotConfigured(self.as_str())
    }
}

/// A positive decimal `api_id` that fits TDLib's `int32`; empty or non-numeric is invalid.
pub fn parse_api_id(text: Option<&str>) -> Result<i32, IdentityProblem> {
    let text = text.map(str::trim).unwrap_or_default();
    if text.is_empty() {
        return Err(IdentityProblem::MissingApiId);
    }
    if !text.bytes().all(|b| b.is_ascii_digit()) {
        return Err(IdentityProblem::InvalidApiId);
    }
    match text.parse::<i32>() {
        Ok(id) if id > 0 => Ok(id),
        _ => Err(IdentityProblem::InvalidApiId),
    }
}

pub fn check_api_hash(text: Option<&str>) -> Result<String, IdentityProblem> {
    let text = text.map(str::trim).unwrap_or_default();
    if text.is_empty() {
        return Err(IdentityProblem::MissingApiHash);
    }
    if text.len() != API_HASH_LEN || !text.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(IdentityProblem::InvalidApiHash);
    }
    Ok(text.to_owned())
}

/// Both fields validated; the first problem found is returned.
pub fn parse_identity(
    api_id: Option<&str>,
    api_hash: Option<&str>,
) -> Result<ApiIdentity, IdentityProblem> {
    Ok(ApiIdentity {
        api_id: parse_api_id(api_id)?,
        api_hash: check_api_hash(api_hash)?,
    })
}

fn io_error(error: io::Error) -> TransportError {
    TransportError::Protocol(format!("api_hash file: {}", error.kind()))
}

/// Stores the personal `api_hash` in `dir\api_hash.bin`, protected by DPAPI (written beside
/// then renamed). An invalid hash is refused and nothing is written.
pub fn store_override_hash(dir: &Path, api_hash: &str) -> Result<(), TransportError> {
    let hash = check_api_hash(Some(api_hash)).map_err(IdentityProblem::into_error)?;
    let blob = protect(hash.as_bytes())?;
    fs::create_dir_all(dir).map_err(io_error)?;
    let path = dir.join(API_HASH_FILE);
    let temporary = dir.join(format!("{API_HASH_FILE}.tmp"));
    fs::write(&temporary, &blob).map_err(io_error)?;
    fs::rename(&temporary, &path).map_err(io_error)
}

/// The personal `api_hash`, `None` when none is stored. A blob DPAPI refuses (other user,
/// other machine, corrupt) or that does not hold a valid hash is
/// `NotConfigured(override_api_hash_unavailable)`, never a silent fallback.
pub fn load_override_hash(dir: &Path) -> Result<Option<String>, TransportError> {
    let blob = match fs::read(dir.join(API_HASH_FILE)) {
        Ok(blob) => blob,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(io_error(error)),
    };
    let unavailable = || IdentityProblem::OverrideHashUnavailable.into_error();
    let mut clear = unprotect(&blob).map_err(|_| unavailable())?;
    let hash = std::str::from_utf8(&clear)
        .ok()
        .and_then(|text| check_api_hash(Some(text)).ok());
    clear.fill(0);
    hash.map(Some).ok_or_else(unavailable)
}

/// Removes the personal `api_hash` (no error when there is none).
pub fn clear_override_hash(dir: &Path) -> Result<(), TransportError> {
    match fs::remove_file(dir.join(API_HASH_FILE)) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => Err(io_error(error)),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::tdlib::tests::TempDir;

    // Fake identity: never the product's real one.
    const FAKE_HASH: &str = "0f1e2d3c4b5a69788796a5b4c3d2e1f0";

    #[test]
    fn api_id_must_be_a_positive_number() {
        assert_eq!(parse_api_id(Some(" 1234567 ")), Ok(1_234_567));
        assert_eq!(parse_api_id(None), Err(IdentityProblem::MissingApiId));
        assert_eq!(parse_api_id(Some("  ")), Err(IdentityProblem::MissingApiId));
        for bad in ["12a4", "-5", "+5", "0", "1.5", "99999999999", "１２"] {
            assert_eq!(
                parse_api_id(Some(bad)),
                Err(IdentityProblem::InvalidApiId),
                "{bad}"
            );
        }
    }

    #[test]
    fn api_hash_must_be_32_hex() {
        assert_eq!(check_api_hash(Some(FAKE_HASH)).as_deref(), Ok(FAKE_HASH));
        assert_eq!(
            check_api_hash(Some("")),
            Err(IdentityProblem::MissingApiHash)
        );
        for bad in [
            &FAKE_HASH[..31],
            "g".repeat(32).as_str(),
            format!("{FAKE_HASH}0").as_str(),
        ] {
            assert_eq!(
                check_api_hash(Some(bad)),
                Err(IdentityProblem::InvalidApiHash)
            );
        }
        let identity = parse_identity(Some("42"), Some(FAKE_HASH)).unwrap();
        assert_eq!(identity.api_id, 42);
        let debug = format!("{identity:?} {identity:#?}");
        assert!(
            !debug.contains(FAKE_HASH) && !debug.contains("42"),
            "{debug}"
        );
        assert_eq!(
            parse_identity(Some("x"), None),
            Err(IdentityProblem::InvalidApiId),
            "the id is checked first"
        );
        let error = IdentityProblem::MissingApiHash.into_error();
        assert_eq!(error.code_and_params().0, "TG_NOT_CONFIGURED");
    }

    #[cfg(windows)]
    #[test]
    fn override_hash_is_dpapi_protected_and_round_trips() {
        let dir = TempDir::new("api-hash");
        assert_eq!(load_override_hash(dir.path()).unwrap(), None);
        store_override_hash(dir.path(), FAKE_HASH).unwrap();
        let blob = fs::read(dir.path().join(API_HASH_FILE)).unwrap();
        assert!(
            !blob
                .windows(FAKE_HASH.len())
                .any(|w| w == FAKE_HASH.as_bytes()),
            "the file is a DPAPI blob, not the hash"
        );
        assert!(!dir.path().join(format!("{API_HASH_FILE}.tmp")).exists());
        assert_eq!(
            load_override_hash(dir.path()).unwrap().as_deref(),
            Some(FAKE_HASH)
        );

        // An invalid hash is refused and the stored one is kept.
        let error = store_override_hash(dir.path(), "not-a-hash").unwrap_err();
        assert_eq!(error.code_and_params().0, "TG_NOT_CONFIGURED");
        assert!(!error.to_string().contains("not-a-hash"));
        assert_eq!(
            load_override_hash(dir.path()).unwrap().as_deref(),
            Some(FAKE_HASH)
        );

        clear_override_hash(dir.path()).unwrap();
        clear_override_hash(dir.path()).unwrap();
        assert_eq!(load_override_hash(dir.path()).unwrap(), None);
    }

    #[cfg(windows)]
    #[test]
    fn a_corrupt_override_blob_is_not_configured() {
        let dir = TempDir::new("api-hash-corrupt");
        store_override_hash(dir.path(), FAKE_HASH).unwrap();
        let path = dir.path().join(API_HASH_FILE);
        let mut blob = fs::read(&path).unwrap();
        let last = blob.len() - 1;
        blob[last] ^= 0xFF;
        fs::write(&path, &blob).unwrap();
        let error = load_override_hash(dir.path()).unwrap_err();
        assert!(
            matches!(
                error,
                TransportError::NotConfigured("override_api_hash_unavailable")
            ),
            "{error}"
        );
        // A valid DPAPI blob that is not a hash is refused too.
        fs::write(&path, protect(b"short").unwrap()).unwrap();
        assert!(load_override_hash(dir.path()).is_err());
    }
}
