//! Encryption key of the TDLib database: 32 random bytes protected by
//! DPAPI for the current Windows user (`CryptProtectData`, `CRYPTPROTECT_UI_FORBIDDEN`), the
//! blob in `key.bin` next to the database. The key never reaches a log or a `Debug`.

use std::fmt;
use std::fs;
use std::io;
use std::path::Path;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;

use crate::transport::TransportError;

pub const KEY_FILE: &str = "key.bin";
const KEY_LEN: usize = 32;

pub struct DatabaseKey([u8; KEY_LEN]);

impl DatabaseKey {
    /// Reads `dir\key.bin`, or creates it with a fresh key when it does not exist. A blob that
    /// DPAPI refuses (corrupt, other user, other machine) or that is not 32 bytes is
    /// `BadDatabaseKey`: the database cannot be opened any more, the user logs in again.
    pub fn load_or_create(dir: &Path) -> Result<Self, TransportError> {
        let path = dir.join(KEY_FILE);
        match fs::read(&path) {
            Ok(blob) => {
                let mut clear = unprotect(&blob).map_err(|_| TransportError::BadDatabaseKey)?;
                let key = <[u8; KEY_LEN]>::try_from(clear.as_slice())
                    .map_err(|_| TransportError::BadDatabaseKey);
                clear.fill(0);
                key.map(Self)
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let key = Self(random()?);
                let blob = protect(&key.0)?;
                fs::create_dir_all(dir).map_err(io_error)?;
                // Written beside then renamed: a crash never leaves a half-written key.
                let temporary = dir.join(format!("{KEY_FILE}.tmp"));
                fs::write(&temporary, &blob).map_err(io_error)?;
                fs::rename(&temporary, &path).map_err(io_error)?;
                Ok(key)
            }
            Err(error) => Err(io_error(error)),
        }
    }

    /// `setTdlibParameters.database_encryption_key` (TDLib JSON `bytes` = base64).
    pub fn base64(&self) -> String {
        STANDARD.encode(self.0)
    }

    #[cfg(test)]
    pub fn bytes(&self) -> &[u8; KEY_LEN] {
        &self.0
    }
}

impl Drop for DatabaseKey {
    fn drop(&mut self) {
        self.0.fill(0);
        std::hint::black_box(&self.0);
    }
}

impl fmt::Debug for DatabaseKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("DatabaseKey(<redacted>)")
    }
}

/// The path is enough to act; the content of the key folder never appears.
fn io_error(error: io::Error) -> TransportError {
    TransportError::Protocol(format!("database key file: {}", error.kind()))
}

#[cfg(windows)]
fn random() -> Result<[u8; KEY_LEN], TransportError> {
    use windows::Win32::Security::Cryptography::{
        BCRYPT_USE_SYSTEM_PREFERRED_RNG, BCryptGenRandom,
    };
    let mut key = [0u8; KEY_LEN];
    // SAFETY: `key` is a valid writable buffer; no algorithm handle with the system RNG flag.
    unsafe { BCryptGenRandom(None, &mut key, BCRYPT_USE_SYSTEM_PREFERRED_RNG) }
        .ok()
        .map_err(|_| TransportError::Protocol("database key: no random source".into()))?;
    Ok(key)
}

#[cfg(windows)]
fn blob_in(data: &[u8]) -> windows::Win32::Security::Cryptography::CRYPT_INTEGER_BLOB {
    windows::Win32::Security::Cryptography::CRYPT_INTEGER_BLOB {
        // A key file over 4 GiB is not ours anyway; it is read as an empty, refused blob.
        cbData: u32::try_from(data.len()).unwrap_or(0),
        pbData: data.as_ptr().cast_mut(),
    }
}

/// Copies DPAPI's output and frees it with `LocalFree`, wiping it first.
#[cfg(windows)]
fn take(blob: windows::Win32::Security::Cryptography::CRYPT_INTEGER_BLOB) -> Vec<u8> {
    use windows::Win32::Foundation::{HLOCAL, LocalFree};
    if blob.pbData.is_null() {
        return Vec::new();
    }
    // SAFETY: DPAPI allocated `cbData` bytes at `pbData` with `LocalAlloc`; they are copied,
    // zeroed, then released once.
    unsafe {
        let data = std::slice::from_raw_parts_mut(blob.pbData, blob.cbData as usize);
        let copy = data.to_vec();
        data.fill(0);
        LocalFree(Some(HLOCAL(blob.pbData.cast())));
        copy
    }
}

#[cfg(windows)]
pub(super) fn protect(clear: &[u8]) -> Result<Vec<u8>, TransportError> {
    use windows::Win32::Security::Cryptography::{
        CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptProtectData,
    };
    let input = blob_in(clear);
    let mut output = CRYPT_INTEGER_BLOB::default();
    // SAFETY: `input` points to `clear`, alive for the call; `output` is filled by DPAPI and
    // released by `take`.
    unsafe {
        CryptProtectData(
            &input,
            windows::core::PCWSTR::null(),
            None,
            None,
            None,
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut output,
        )
    }
    .map_err(|_| TransportError::Protocol("database key: DPAPI protect failed".into()))?;
    Ok(take(output))
}

#[cfg(windows)]
pub(super) fn unprotect(blob: &[u8]) -> Result<Vec<u8>, TransportError> {
    use windows::Win32::Security::Cryptography::{
        CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptUnprotectData,
    };
    let input = blob_in(blob);
    let mut output = CRYPT_INTEGER_BLOB::default();
    // SAFETY: as `protect`.
    unsafe {
        CryptUnprotectData(
            &input,
            None,
            None,
            None,
            None,
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut output,
        )
    }
    .map_err(|_| TransportError::BadDatabaseKey)?;
    Ok(take(output))
}

#[cfg(not(windows))]
fn random() -> Result<[u8; KEY_LEN], TransportError> {
    Err(TransportError::Protocol(
        "database key: DPAPI is Windows only".into(),
    ))
}

#[cfg(not(windows))]
pub(super) fn protect(_: &[u8]) -> Result<Vec<u8>, TransportError> {
    Err(TransportError::Protocol(
        "database key: DPAPI is Windows only".into(),
    ))
}

#[cfg(not(windows))]
pub(super) fn unprotect(_: &[u8]) -> Result<Vec<u8>, TransportError> {
    Err(TransportError::BadDatabaseKey)
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use crate::transport::tdlib::tests::TempDir;

    #[test]
    fn dpapi_round_trip_keeps_the_same_key() {
        let dir = TempDir::new("key-roundtrip");
        let first = DatabaseKey::load_or_create(dir.path()).unwrap();
        let blob = fs::read(dir.path().join(KEY_FILE)).unwrap();
        // The file is a DPAPI blob, not the key.
        assert!(!blob.windows(KEY_LEN).any(|w| w == first.bytes()));
        assert!(blob.len() > KEY_LEN);
        let second = DatabaseKey::load_or_create(dir.path()).unwrap();
        assert_eq!(first.bytes(), second.bytes());
        assert_eq!(STANDARD.decode(first.base64()).unwrap(), first.bytes());
        assert!(!dir.path().join(format!("{KEY_FILE}.tmp")).exists());
        // Two folders, two keys.
        let other_dir = TempDir::new("key-roundtrip-other");
        let other = DatabaseKey::load_or_create(other_dir.path()).unwrap();
        assert_ne!(first.bytes(), other.bytes());
    }

    #[test]
    fn a_corrupt_blob_is_bad_database_key() {
        let dir = TempDir::new("key-corrupt");
        let key = DatabaseKey::load_or_create(dir.path()).unwrap();
        drop(key);
        let path = dir.path().join(KEY_FILE);
        let mut blob = fs::read(&path).unwrap();
        let last = blob.len() - 1;
        blob[last] ^= 0xFF;
        fs::write(&path, &blob).unwrap();
        assert!(matches!(
            DatabaseKey::load_or_create(dir.path()),
            Err(TransportError::BadDatabaseKey)
        ));
        fs::write(&path, b"garbage").unwrap();
        assert!(matches!(
            DatabaseKey::load_or_create(dir.path()),
            Err(TransportError::BadDatabaseKey)
        ));
        // A valid DPAPI blob of the wrong length is refused too.
        fs::write(&path, protect(&[7u8; 16]).unwrap()).unwrap();
        assert!(matches!(
            DatabaseKey::load_or_create(dir.path()),
            Err(TransportError::BadDatabaseKey)
        ));
        // Never silently replaced: the corrupt file is still there.
        assert!(path.exists());
    }

    #[test]
    fn debug_never_shows_the_key() {
        let dir = TempDir::new("key-debug");
        let key = DatabaseKey::load_or_create(dir.path()).unwrap();
        let text = format!("{key:?} {key:#?}");
        assert_eq!(text, "DatabaseKey(<redacted>) DatabaseKey(<redacted>)");
        assert!(!text.contains(&key.base64()));
    }
}
