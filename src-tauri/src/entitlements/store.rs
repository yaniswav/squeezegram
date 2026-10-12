//! The stored license: `license.txt` next to `settings.json`, one token, written beside then
//! renamed (as `key.bin`). No file means the free plan.
//!
//! Evaluated at call time against the signed-in account, like the upload limit (`limit.rs`):
//! every change of the login state (sign-in, sign-out, another account) gives a new verdict,
//! with one `license_evaluated` journal line (plan, source, error code). A missing, unreadable
//! or corrupt file is never an error for the app: the plan is free.
//!
//! The token never reaches the journal, a `Debug` or an error text.

use std::fmt;
use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use tokio::sync::watch;

use super::license::{self, KeyRing, License, LicenseError};
use super::{EntitlementSource, EntitlementStatus, Entitlements, Feature, Plan};
use crate::limit;
use crate::transport::AuthState;

pub const FILE_NAME: &str = "license.txt";
/// A license is about 400 bytes; a larger file is not one and is not read whole.
const MAX_FILE_BYTES: u64 = 64 * 1024;

/// What `license.txt` holds, in memory.
enum Stored {
    Absent,
    Token(String),
    /// Not readable, too large or not UTF-8: evaluated as `LIC_MALFORMED`.
    Unreadable,
}

impl fmt::Debug for Stored {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Stored::Absent => "Absent",
            Stored::Token(_) => "Token(<redacted>)",
            Stored::Unreadable => "Unreadable",
        })
    }
}

/// Why `save` or `clear` did nothing.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum StoreError {
    #[error(transparent)]
    License(LicenseError),
    #[error("license file: {0}")]
    Io(io::ErrorKind),
}

impl StoreError {
    pub fn code(self) -> &'static str {
        match self {
            StoreError::License(error) => error.code(),
            StoreError::Io(_) => "IO_ERROR",
        }
    }
}

impl From<io::Error> for StoreError {
    fn from(error: io::Error) -> Self {
        StoreError::Io(error.kind())
    }
}

type Verdict = Result<Option<License>, LicenseError>;

struct Cached {
    generation: u64,
    user_id: Option<i64>,
    verdict: Verdict,
}

struct Inner {
    stored: Stored,
    /// Bumped by `save` and `clear`.
    generation: u64,
    cached: Option<Cached>,
}

pub struct LicenseStore {
    path: PathBuf,
    keys: KeyRing,
    /// `None`: no transport, never an account (a stored license is then `LIC_NO_ACCOUNT`).
    auth: Option<watch::Receiver<AuthState>>,
    inner: Mutex<Inner>,
}

impl fmt::Debug for LicenseStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LicenseStore")
            .field("keys", &self.keys)
            .field("stored", &self.lock().stored)
            .finish_non_exhaustive()
    }
}

fn read_stored(path: &Path) -> Stored {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Stored::Absent,
        Err(error) => {
            tracing::warn!(event = "license_unreadable", error_kind = %error.kind(), "license file not readable");
            return Stored::Unreadable;
        }
    };
    let mut bytes = Vec::new();
    if let Err(error) = file.take(MAX_FILE_BYTES + 1).read_to_end(&mut bytes) {
        tracing::warn!(event = "license_unreadable", error_kind = %error.kind(), "license file not readable");
        return Stored::Unreadable;
    }
    if bytes.len() as u64 > MAX_FILE_BYTES {
        return Stored::Unreadable;
    }
    match String::from_utf8(bytes) {
        Ok(text) if text.trim().is_empty() => Stored::Absent,
        Ok(text) => Stored::Token(text.trim().to_owned()),
        Err(_) => Stored::Unreadable,
    }
}

fn status_of(verdict: &Verdict) -> EntitlementStatus {
    match verdict {
        Ok(None) => EntitlementStatus {
            plan: Plan::Free,
            source: EntitlementSource::None,
        },
        Ok(Some(license)) => EntitlementStatus {
            plan: license.plan,
            source: EntitlementSource::License,
        },
        Err(_) => EntitlementStatus {
            plan: Plan::Free,
            source: EntitlementSource::Rejected,
        },
    }
}

impl LicenseStore {
    /// `license.txt` in the folder of `sibling` (`settings.json`).
    pub fn path_beside(sibling: &Path) -> PathBuf {
        sibling.with_file_name(FILE_NAME)
    }

    /// Reads the file once; never fails (absent or corrupt: the free plan).
    pub fn load(path: PathBuf, keys: KeyRing, auth: Option<watch::Receiver<AuthState>>) -> Self {
        let stored = read_stored(&path);
        Self {
            path,
            keys,
            auth,
            inner: Mutex::new(Inner {
                stored,
                generation: 0,
                cached: None,
            }),
        }
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn user_id(&self) -> Option<i64> {
        let auth = self.auth.as_ref()?;
        limit::account(&auth.borrow()).map(|me| me.user_id)
    }

    /// The verdict for the stored token and the account signed in now; recomputed (and
    /// logged) only when one of them changed.
    fn with_verdict<R>(&self, read: impl FnOnce(&Verdict) -> R) -> R {
        let user_id = self.user_id();
        let mut inner = self.lock();
        let generation = inner.generation;
        let fresh = inner
            .cached
            .as_ref()
            .is_some_and(|c| c.generation == generation && c.user_id == user_id);
        if !fresh {
            let verdict = match &inner.stored {
                Stored::Absent => Ok(None),
                Stored::Unreadable => Err(LicenseError::Malformed),
                Stored::Token(token) => license::verify_for(token, &self.keys, user_id).map(Some),
            };
            let status = status_of(&verdict);
            tracing::info!(
                event = "license_evaluated",
                plan = status.plan.as_str(),
                source = status.source.as_str(),
                error_code = verdict.as_ref().err().map(|e| e.code()),
                "license evaluated"
            );
            inner.cached = Some(Cached {
                generation,
                user_id,
                verdict,
            });
        }
        let cached = inner.cached.as_ref().map(|c| &c.verdict);
        read(cached.unwrap_or(&Ok(None)))
    }

    /// Verifies then stores `token` (surrounding whitespace dropped). Refused, and the file
    /// left as it was, when it does not verify or belongs to another signed-in account; kept
    /// when no account is signed in yet (checked at sign-in).
    pub fn save(&self, token: &str) -> Result<EntitlementStatus, StoreError> {
        let token = token.trim();
        let checked = license::verify(token, &self.keys).and_then(|license| {
            match license::check_account(&license, self.user_id()) {
                Err(LicenseError::NoAccount) => Ok(()),
                other => other,
            }
        });
        if let Err(error) = checked {
            tracing::warn!(
                event = "license_rejected",
                error_code = error.code(),
                "license key refused"
            );
            return Err(StoreError::License(error));
        }
        self.write(token).inspect_err(|error| {
            tracing::warn!(event = "license_write_failed", error_code = error.code(), error = %error, "license key not saved");
        })?;
        {
            let mut inner = self.lock();
            inner.stored = Stored::Token(token.to_owned());
            inner.generation += 1;
        }
        tracing::info!(event = "license_saved", "license key saved");
        Ok(self.status())
    }

    /// Written beside then renamed: a crash never leaves a half-written license.
    fn write(&self, token: &str) -> Result<(), StoreError> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        let temporary = self.path.with_extension("txt.tmp");
        fs::write(&temporary, format!("{token}\n"))?;
        fs::rename(&temporary, &self.path)?;
        Ok(())
    }

    /// Removes the license (no error when there is none): back to the free plan.
    pub fn clear(&self) -> Result<(), StoreError> {
        if let Err(error) = fs::remove_file(&self.path)
            && error.kind() != io::ErrorKind::NotFound
        {
            return Err(error.into());
        }
        let mut inner = self.lock();
        inner.stored = Stored::Absent;
        inner.generation += 1;
        tracing::info!(event = "license_cleared", "license key removed");
        Ok(())
    }

    /// The code that keeps the stored license from applying now, if any.
    pub fn problem(&self) -> Option<LicenseError> {
        self.with_verdict(|verdict| verdict.as_ref().err().copied())
    }
}

impl Entitlements for LicenseStore {
    fn allows(&self, feature: Feature) -> bool {
        self.with_verdict(|verdict| matches!(verdict, Ok(Some(license)) if license.allows(feature)))
    }

    fn status(&self) -> EntitlementStatus {
        self.with_verdict(status_of)
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::entitlements::license::test_vectors::{self, Vectors};
    use crate::logging::export;
    use crate::logging::testing::TestJournal;
    use crate::queue::test_util::TestDir;
    use crate::transport::mock::{MOCK_CODE, MOCK_ME_ID, MockTransport};
    use crate::transport::{AuthInput, TelegramTransport};

    const FREE_NONE: EntitlementStatus = EntitlementStatus {
        plan: Plan::Free,
        source: EntitlementSource::None,
    };
    const FREE_REJECTED: EntitlementStatus = EntitlementStatus {
        plan: Plan::Free,
        source: EntitlementSource::Rejected,
    };
    const LIFETIME: EntitlementStatus = EntitlementStatus {
        plan: Plan::Lifetime,
        source: EntitlementSource::License,
    };

    fn token(vectors: &Vectors, name: &str) -> String {
        vectors.case(name).token.clone()
    }

    fn store(dir: &TestDir, vectors: &Vectors, transport: &dyn TelegramTransport) -> LicenseStore {
        LicenseStore::load(
            dir.path().join(FILE_NAME),
            vectors.key_ring(),
            Some(transport.auth_state()),
        )
    }

    #[test]
    fn the_vectors_account_is_the_mock_account() {
        let vectors = test_vectors::load();
        assert_eq!(vectors.case("valid").user_id, Some(MOCK_ME_ID));
    }

    #[test]
    fn save_then_load_round_trips() {
        let dir = TestDir::new("license-roundtrip");
        let vectors = test_vectors::load();
        let account = MockTransport::authorized();
        let first = store(&dir, &vectors, &account);
        assert_eq!(first.status(), FREE_NONE);
        let valid = token(&vectors, "valid");
        assert_eq!(first.save(&format!("  {valid}\r\n")), Ok(LIFETIME));
        assert!(first.allows(Feature::AdvancedEncoding));
        assert!(first.allows(Feature::MultiDestination));
        assert!(!first.allows(Feature::Background));
        let path = dir.path().join(FILE_NAME);
        assert_eq!(fs::read_to_string(&path).unwrap(), format!("{valid}\n"));
        assert!(!path.with_extension("txt.tmp").exists());

        let second = store(&dir, &vectors, &account);
        assert_eq!(second.status(), LIFETIME);
        assert!(second.allows(Feature::AdvancedEncoding));

        second.clear().unwrap();
        second.clear().unwrap();
        assert!(!path.exists());
        assert_eq!(second.status(), FREE_NONE);
        assert!(Feature::ALL.iter().all(|f| !second.allows(*f)));
        assert_eq!(
            LicenseStore::path_beside(&dir.path().join(crate::settings::FILE_NAME)),
            path
        );
    }

    #[test]
    fn no_file_is_the_free_plan() {
        let dir = TestDir::new("license-absent");
        let vectors = test_vectors::load();
        let path = dir.path().join("missing").join(FILE_NAME);
        let store = LicenseStore::load(path.clone(), vectors.key_ring(), None);
        assert_eq!(store.status(), FREE_NONE);
        assert_eq!(store.problem(), None);
        assert!(Feature::ALL.iter().all(|f| !store.allows(*f)));
        assert!(!path.exists(), "reading never creates the file");
        // An empty file reads as no license.
        fs::write(dir.path().join(FILE_NAME), " \r\n").unwrap();
        let account = MockTransport::authorized();
        assert_eq!(
            super::tests::store(&dir, &vectors, &account).status(),
            FREE_NONE
        );
    }

    #[test]
    fn a_corrupt_file_is_the_free_plan_without_panic() {
        let dir = TestDir::new("license-corrupt");
        let vectors = test_vectors::load();
        let account = MockTransport::authorized();
        let path = dir.path().join(FILE_NAME);
        let mut huge = token(&vectors, "valid").into_bytes();
        huge.resize(MAX_FILE_BYTES as usize + 10, b'A');
        let contents: Vec<Vec<u8>> = vec![
            vec![0xFF, 0xFE, 0x00, 0x80, 0xC3],
            b"garbage".to_vec(),
            b"SQZ1...".to_vec(),
            huge,
            token(&vectors, "prefix_sqz2").into_bytes(),
            token(&vectors, "payload_byte_altered").into_bytes(),
            token(&vectors, "other_account").into_bytes(),
            format!("{0}\n{0}", token(&vectors, "valid")).into_bytes(),
        ];
        for content in contents {
            fs::write(&path, &content).unwrap();
            let store = store(&dir, &vectors, &account);
            assert_eq!(store.status(), FREE_REJECTED);
            assert!(store.problem().is_some());
            assert!(Feature::ALL.iter().all(|f| !store.allows(*f)));
        }
        // A folder where the file should be: unreadable, still free.
        fs::remove_file(&path).unwrap();
        fs::create_dir(&path).unwrap();
        let store = store(&dir, &vectors, &account);
        assert_eq!(store.status().plan, Plan::Free);
    }

    #[test]
    fn save_refuses_what_does_not_verify_and_keeps_the_file() {
        let dir = TestDir::new("license-refused");
        let vectors = test_vectors::load();
        let account = MockTransport::authorized();
        let store = store(&dir, &vectors, &account);
        let valid = token(&vectors, "valid");
        store.save(&valid).unwrap();
        for (name, code) in [
            ("truncated", "LIC_MALFORMED"),
            ("unknown_kid", "LIC_UNKNOWN_KID"),
            ("wrong_signature_other_key", "LIC_BAD_SIGNATURE"),
            ("other_account", "LIC_WRONG_ACCOUNT"),
        ] {
            let error = store.save(&token(&vectors, name)).unwrap_err();
            assert_eq!(error.code(), code, "{name}");
            assert_eq!(store.status(), LIFETIME, "{name}");
        }
        assert_eq!(
            fs::read_to_string(dir.path().join(FILE_NAME)).unwrap(),
            format!("{valid}\n")
        );
        // Signed out: a license is kept, then checked at sign-in.
        let signed_out = LicenseStore::load(
            dir.path().join("other").join(FILE_NAME),
            vectors.key_ring(),
            None,
        );
        let status = signed_out.save(&token(&vectors, "other_account")).unwrap();
        assert_eq!(status, FREE_REJECTED);
        assert_eq!(signed_out.problem(), Some(LicenseError::NoAccount));
    }

    #[tokio::test]
    async fn the_verdict_follows_the_signed_in_account() {
        let dir = TestDir::new("license-auth");
        let journal = TestJournal::new(&dir.path().join("logs"), "info");
        let vectors = test_vectors::load();
        fs::write(dir.path().join(FILE_NAME), token(&vectors, "valid")).unwrap();
        let later = MockTransport::new();
        let store = store(&dir, &vectors, &later);
        later.start().await.unwrap();
        assert_eq!(store.status(), FREE_REJECTED);
        assert_eq!(store.problem(), Some(LicenseError::NoAccount));

        later
            .auth_submit(AuthInput::Phone("+33600000000".into()))
            .await
            .unwrap();
        later
            .auth_submit(AuthInput::Code(MOCK_CODE.into()))
            .await
            .unwrap();
        assert_eq!(store.status(), LIFETIME);
        assert!(store.allows(Feature::MultiDestination));

        later.log_out().await.unwrap();
        assert_eq!(store.status(), FREE_REJECTED);
        assert!(!store.allows(Feature::MultiDestination));

        let codes: Vec<serde_json::Value> = journal
            .lines()
            .iter()
            .filter(|l| l["event"] == "license_evaluated")
            .map(|l| l["error_code"].clone())
            .collect();
        assert_eq!(
            codes,
            [
                json!("LIC_NO_ACCOUNT"),
                serde_json::Value::Null,
                json!("LIC_NO_ACCOUNT")
            ],
            "one line per change, none for repeated reads"
        );
    }

    #[test]
    fn the_token_reaches_no_journal_line_and_no_debug_export() {
        let dir = TestDir::new("license-journal");
        // The journal shares the license folder: the export must still take journal files only.
        let journal = TestJournal::new(dir.path(), "trace");
        let vectors = test_vectors::load();
        let account = MockTransport::authorized();
        let store = store(&dir, &vectors, &account);
        let mut debug = Vec::new();
        for case in &vectors.cases {
            let _ = store.save(&case.token);
            let reloaded = super::tests::store(&dir, &vectors, &account);
            debug.push(format!(
                "{store:?} {reloaded:?} {:?} {:?}",
                reloaded.status(),
                reloaded.problem()
            ));
            if let Err(error) = store.save(&case.token) {
                tracing::warn!(event = "test_error", error = %error, debug = ?error, "{error}");
            }
            tracing::info!(event = "test_store", store = ?store, "{store:?}");
        }
        store.clear().unwrap();
        store.save(&token(&vectors, "valid")).unwrap();
        assert!(dir.path().join(FILE_NAME).exists());

        let out = dir.path().join("export.zip");
        let report = json!({
            "entitlements": store.status(),
            "store": format!("{store:?}"),
            "debug": debug,
        });
        export::write_zip(&out, &report, dir.path()).unwrap();
        let mut zip = zip::ZipArchive::new(File::open(&out).unwrap()).unwrap();
        let names: Vec<String> = zip.file_names().map(str::to_owned).collect();
        assert!(
            names
                .iter()
                .all(|n| n == "report.json" || n.starts_with("logs/squeezegram-")),
            "{names:?}"
        );
        let mut exported = String::new();
        for index in 0..zip.len() {
            zip.by_index(index)
                .unwrap()
                .read_to_string(&mut exported)
                .unwrap();
        }
        let logged = journal.text();
        assert!(logged.contains("license_evaluated") && logged.contains("license_saved"));
        let license = license::verify(&token(&vectors, "valid"), &vectors.key_ring()).unwrap();
        for case in &vectors.cases {
            let parts: Vec<&str> = case.token.trim().split('.').collect();
            let mut secrets = vec![case.token.trim()];
            secrets.extend(parts.iter().skip(1).filter(|p| p.len() > 16));
            for secret in secrets {
                assert!(!logged.contains(secret), "{} in the journal", case.name);
                assert!(!exported.contains(secret), "{} in the export", case.name);
            }
        }
        for identity in [license.sub.as_str(), &license.jti, &MOCK_ME_ID.to_string()] {
            assert!(!logged.contains(identity) && !exported.contains(identity));
        }
    }
}
