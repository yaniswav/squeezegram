//! The stored license: `license.txt` next to `settings.json`, one token, written beside then
//! renamed (as `key.bin`). No file means the free plan.
//!
//! Evaluated at call time against the signed-in account, like the upload limit (`limit.rs`):
//! every change of the login state (sign-in, sign-out, another account) gives a new verdict.
//! No account signed in is a wait, not an error: the license applies once its account signs
//! in. A missing, unreadable or corrupt file is never an error for the app: the plan is free.
//!
//! A key covers the app majors up to its `maj`. The order of the checks is: signature, revocation,
//! coverage of this build's major, then account: a key of an older major is `LIC_OLDER_MAJOR`
//! whether or not an account is signed in, and whichever account it was issued to.
//!
//! Journal: `license_status` (once per new verdict), `license_set` and `license_cleared`, with
//! the plan, the `LIC_*` code, and the `kid` and `maj` (`covers_major`) of a verified license.
//! The token, its `jti` and its `sub` never reach the journal, a `Debug` or an error text.

use std::fmt;
use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use tokio::sync::watch;

use super::license::{self, KeyRing, License, LicenseError};
use super::revoked::REVOKED_JTIS;
use super::{EntitlementSource, EntitlementStatus, Entitlements, Feature, Plan, keys};
use crate::limit;
use crate::transport::AuthState;

pub const FILE_NAME: &str = "license.txt";
/// Major version of this build, read from the package version at compile time: a malformed
/// value stops the build instead of panicking at run time. The updater compares against the
/// same value, so the two never disagree about which major is running.
pub const BUILD_MAJOR: u32 = parse_major(env!("CARGO_PKG_VERSION_MAJOR"));

const fn parse_major(text: &str) -> u32 {
    let digits = text.as_bytes();
    assert!(!digits.is_empty(), "empty major version");
    let mut major = 0u32;
    let mut at = 0;
    while at < digits.len() {
        assert!(digits[at].is_ascii_digit(), "major version is not a number");
        major = major * 10 + (digits[at] - b'0') as u32;
        at += 1;
    }
    major
}
/// A license is about 400 bytes; a larger file is not one and is not read whole.
const MAX_FILE_BYTES: u64 = 64 * 1024;

/// What `license.txt` holds, in memory.
enum Stored {
    Absent,
    Token(String),
    /// Not readable (with the I/O error kind), too large or not UTF-8: `LIC_MALFORMED`.
    Unreadable(Option<io::ErrorKind>),
}

impl fmt::Debug for Stored {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Stored::Absent => f.write_str("Absent"),
            Stored::Token(_) => f.write_str("Token(<redacted>)"),
            Stored::Unreadable(kind) => write!(f, "Unreadable({kind:?})"),
        }
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

/// The stored token judged against the account signed in at that moment.
enum Verdict {
    NoLicense,
    /// Verified and issued to the signed-in account.
    Applies(License),
    /// Verified, but not for the account signed in now: `NoAccount` or `WrongAccount`.
    NotThisAccount(License, LicenseError),
    /// Verified, but it covers only majors below this build's: wins over the account checks.
    OlderMajor(License),
    /// Not a license this build accepts.
    Invalid(LicenseError),
}

impl Verdict {
    fn of(
        stored: &Stored,
        keys: &KeyRing,
        user_id: Option<i64>,
        build_major: u32,
        revoked_jtis: &[&str],
    ) -> Self {
        let token = match stored {
            Stored::Absent => return Verdict::NoLicense,
            Stored::Unreadable(_) => return Verdict::Invalid(LicenseError::Malformed),
            Stored::Token(token) => token,
        };
        match license::verify(token, keys) {
            Err(error) => Verdict::Invalid(error),
            Ok(license) => {
                if revoked_jtis.contains(&license.jti.as_str()) {
                    return Verdict::Invalid(LicenseError::Revoked);
                }
                if license::check_coverage(&license, build_major).is_err() {
                    return Verdict::OlderMajor(license);
                }
                match license::check_account(&license, user_id) {
                    Ok(()) => Verdict::Applies(license),
                    Err(error) => Verdict::NotThisAccount(license, error),
                }
            }
        }
    }

    fn error(&self) -> Option<LicenseError> {
        match self {
            Verdict::NoLicense | Verdict::Applies(_) => None,
            Verdict::NotThisAccount(_, error) | Verdict::Invalid(error) => Some(*error),
            Verdict::OlderMajor(_) => Some(LicenseError::OlderMajor),
        }
    }

    /// The non-revoked license whose signature verified, whatever else keeps it from applying.
    fn verified(&self) -> Option<&License> {
        match self {
            Verdict::Applies(license)
            | Verdict::NotThisAccount(license, _)
            | Verdict::OlderMajor(license) => Some(license),
            Verdict::NoLicense | Verdict::Invalid(_) => None,
        }
    }

    /// Only for a license whose signature verified: an unverified `kid` is attacker text.
    fn kid(&self) -> Option<&str> {
        self.verified().map(|license| license.kid.as_str())
    }

    fn allows(&self, feature: Feature) -> bool {
        matches!(self, Verdict::Applies(license) if license.allows(feature))
    }

    fn status(&self) -> EntitlementStatus {
        let free = |source, account_matches| EntitlementStatus {
            plan: Plan::Free,
            source,
            features: Vec::new(),
            account_matches,
            covers_major: self.verified().map(|license| license.maj),
            reason: self.error().map(LicenseError::code),
        };
        match self {
            Verdict::NoLicense => free(EntitlementSource::None, None),
            Verdict::Applies(license) => EntitlementStatus {
                plan: license.plan,
                source: EntitlementSource::License,
                // What `allows` grants (every feature for a lifetime licence), minus the
                // declared-but-unwired ones: not shown as unlocked (`allows` is unchanged).
                features: license
                    .granted()
                    .filter(|feature| feature.shipped())
                    .collect(),
                account_matches: Some(true),
                covers_major: Some(license.maj),
                reason: None,
            },
            Verdict::NotThisAccount(_, LicenseError::NoAccount) => {
                free(EntitlementSource::Pending, None)
            }
            Verdict::NotThisAccount(..) => free(EntitlementSource::Rejected, Some(false)),
            // The account is not compared once the major is refused: nothing to say about it.
            Verdict::OlderMajor(_) | Verdict::Invalid(_) => free(EntitlementSource::Rejected, None),
        }
    }
}

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

/// The `Entitlements` of the app: the stored license, its keys and the login state.
pub struct LicenseEntitlements {
    path: PathBuf,
    keys: KeyRing,
    revoked_jtis: &'static [&'static str],
    /// `None`: no transport, never an account (a stored license then waits for one).
    auth: Option<watch::Receiver<AuthState>>,
    /// Major version of the build: `BUILD_MAJOR`, except in tests.
    build_major: u32,
    inner: Mutex<Inner>,
}

impl fmt::Debug for LicenseEntitlements {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LicenseEntitlements")
            .field("keys", &self.keys)
            .field("stored", &self.lock().stored)
            .finish_non_exhaustive()
    }
}

fn read_stored(path: &Path) -> Stored {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Stored::Absent,
        Err(error) => return Stored::Unreadable(Some(error.kind())),
    };
    let mut bytes = Vec::new();
    if let Err(error) = file.take(MAX_FILE_BYTES + 1).read_to_end(&mut bytes) {
        return Stored::Unreadable(Some(error.kind()));
    }
    if bytes.len() as u64 > MAX_FILE_BYTES {
        return Stored::Unreadable(None);
    }
    match String::from_utf8(bytes) {
        Ok(text) if text.trim().is_empty() => Stored::Absent,
        Ok(text) => Stored::Token(text.trim().to_owned()),
        Err(_) => Stored::Unreadable(None),
    }
}

impl LicenseEntitlements {
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
            revoked_jtis: REVOKED_JTIS,
            auth,
            build_major: BUILD_MAJOR,
            inner: Mutex::new(Inner {
                stored,
                generation: 0,
                cached: None,
            }),
        }
    }

    /// `load` with the production keys (`keys::PRODUCTION_KEYS`).
    pub fn production(path: PathBuf, auth: Option<watch::Receiver<AuthState>>) -> Self {
        Self::load(path, keys::production(), auth)
    }

    /// A store that judges coverage as the build of major `build_major`.
    #[cfg(test)]
    fn with_build_major(mut self, build_major: u32) -> Self {
        self.build_major = build_major;
        self
    }

    #[cfg(test)]
    pub(crate) fn with_revoked_jtis(mut self, revoked_jtis: &'static [&'static str]) -> Self {
        self.revoked_jtis = revoked_jtis;
        self
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
    /// logged as `license_status`) only when one of them changed.
    fn with_verdict<R>(&self, read: impl FnOnce(&Verdict) -> R) -> R {
        let user_id = self.user_id();
        let mut guard = self.lock();
        let inner = &mut *guard;
        let fresh = inner
            .cached
            .as_ref()
            .is_some_and(|c| c.generation == inner.generation && c.user_id == user_id);
        if !fresh {
            let verdict = Verdict::of(
                &inner.stored,
                &self.keys,
                user_id,
                self.build_major,
                self.revoked_jtis,
            );
            let status = verdict.status();
            let fields = serde_json::json!({ "features": status.features }).to_string();
            let io_error = match inner.stored {
                Stored::Unreadable(Some(kind)) => Some(kind.to_string()),
                _ => None,
            };
            tracing::info!(
                event = "license_status",
                plan = status.plan.as_str(),
                source = status.source.as_str(),
                json = fields.as_str(),
                error_code = status.reason,
                kid = verdict.kid(),
                covers_major = status.covers_major,
                io_error = io_error.as_deref(),
                "license evaluated"
            );
            inner.cached = Some(Cached {
                generation: inner.generation,
                user_id,
                verdict,
            });
        }
        match &inner.cached {
            Some(cached) => read(&cached.verdict),
            None => read(&Verdict::NoLicense),
        }
    }

    /// Verifies then stores `token` (surrounding whitespace dropped). Refused, and the file
    /// left as it was, when it does not verify, is revoked, covers only older majors than this
    /// build or belongs to another signed-in account; kept when no account is signed in yet
    /// (checked at sign-in).
    pub fn save(&self, token: &str) -> Result<EntitlementStatus, StoreError> {
        let token = token.trim();
        let verified = license::verify(token, &self.keys);
        let kid = verified.as_ref().ok().map(|license| license.kid.clone());
        let checked = verified.and_then(|license| {
            if self.revoked_jtis.contains(&license.jti.as_str()) {
                return Err(LicenseError::Revoked);
            }
            license::check_coverage(&license, self.build_major)?;
            match license::check_account(&license, self.user_id()) {
                Err(LicenseError::NoAccount) => Ok(()),
                other => other,
            }
        });
        if let Err(error) = checked {
            tracing::warn!(
                event = "license_set",
                ok = false,
                error_code = error.code(),
                kid = kid.as_deref(),
                "license key refused"
            );
            return Err(StoreError::License(error));
        }
        if let Err(error) = self.write(token) {
            tracing::warn!(
                event = "license_set",
                ok = false,
                error_code = error.code(),
                error = %error,
                kid = kid.as_deref(),
                "license key not saved"
            );
            return Err(error);
        }
        {
            let mut inner = self.lock();
            inner.stored = Stored::Token(token.to_owned());
            inner.generation += 1;
        }
        tracing::info!(
            event = "license_set",
            ok = true,
            kid = kid.as_deref(),
            "license key saved"
        );
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
            let error = StoreError::from(error);
            tracing::warn!(
                event = "license_cleared",
                ok = false,
                error_code = error.code(),
                error = %error,
                "license key not removed"
            );
            return Err(error);
        }
        let mut inner = self.lock();
        inner.stored = Stored::Absent;
        inner.generation += 1;
        tracing::info!(event = "license_cleared", ok = true, "license key removed");
        Ok(())
    }

    /// The code that keeps the stored license from applying now, if any.
    pub fn problem(&self) -> Option<LicenseError> {
        self.with_verdict(Verdict::error)
    }
}

impl Entitlements for LicenseEntitlements {
    fn allows(&self, feature: Feature) -> bool {
        self.with_verdict(|verdict| verdict.allows(feature))
    }

    fn status(&self) -> EntitlementStatus {
        self.with_verdict(Verdict::status)
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

    fn status(
        source: EntitlementSource,
        account_matches: Option<bool>,
        covers_major: Option<u32>,
        reason: Option<&'static str>,
    ) -> EntitlementStatus {
        EntitlementStatus {
            plan: Plan::Free,
            source,
            features: Vec::new(),
            account_matches,
            covers_major,
            reason,
        }
    }

    fn free_none() -> EntitlementStatus {
        status(EntitlementSource::None, None, None, None)
    }

    fn pending() -> EntitlementStatus {
        status(
            EntitlementSource::Pending,
            None,
            Some(1),
            Some("LIC_NO_ACCOUNT"),
        )
    }

    /// A key that verifies (`maj` 1 unless said) but covers only majors below the build's.
    fn older_major(covers_major: u32) -> EntitlementStatus {
        status(
            EntitlementSource::Rejected,
            None,
            Some(covers_major),
            Some("LIC_OLDER_MAJOR"),
        )
    }

    /// The `valid` vector on its own account: it lists two features, a lifetime licence
    /// grants every shipped one.
    fn lifetime() -> EntitlementStatus {
        EntitlementStatus {
            plan: Plan::Lifetime,
            source: EntitlementSource::License,
            features: Feature::ALL.into_iter().filter(|f| f.shipped()).collect(),
            account_matches: Some(true),
            covers_major: Some(1),
            reason: None,
        }
    }

    fn token(vectors: &Vectors, name: &str) -> String {
        vectors.case(name).token.clone()
    }

    fn store(
        dir: &TestDir,
        vectors: &Vectors,
        transport: &dyn TelegramTransport,
    ) -> LicenseEntitlements {
        LicenseEntitlements::load(
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
        assert_eq!(first.status(), free_none());
        let valid = token(&vectors, "valid");
        assert_eq!(first.save(&format!("  {valid}\r\n")), Ok(lifetime()));
        assert!(Feature::ALL.iter().all(|f| first.allows(*f)));
        let path = dir.path().join(FILE_NAME);
        assert_eq!(fs::read_to_string(&path).unwrap(), format!("{valid}\n"));
        assert!(!path.with_extension("txt.tmp").exists());

        let second = store(&dir, &vectors, &account);
        assert_eq!(second.status(), lifetime());
        assert!(second.allows(Feature::AdvancedEncoding));

        second.clear().unwrap();
        second.clear().unwrap();
        assert!(!path.exists());
        assert_eq!(second.status(), free_none());
        assert!(Feature::ALL.iter().all(|f| !second.allows(*f)));
        assert_eq!(
            LicenseEntitlements::path_beside(&dir.path().join(crate::settings::FILE_NAME)),
            path
        );
    }

    #[test]
    fn save_refuses_a_revoked_jti_and_preserves_a_repurchase() {
        let dir = TestDir::new("license-save-revoked");
        let vectors = test_vectors::load();
        let account = MockTransport::authorized();
        let refunded = vectors.lifetime_token(MOCK_ME_ID, &[], "refunded-key");
        let replacement = vectors.lifetime_token(MOCK_ME_ID, &[], "replacement-key");
        let path = dir.path().join(FILE_NAME);
        let store = store(&dir, &vectors, &account).with_revoked_jtis(&["refunded-key"]);
        let error = store.save(&refunded).unwrap_err();
        assert_eq!(error, StoreError::License(LicenseError::Revoked));
        assert_eq!(error.code(), "LIC_REVOKED");
        assert!(!path.exists());
        assert!(!path.with_extension("txt.tmp").exists());
        assert_eq!(store.status(), free_none());

        let old = license::verify(&refunded, &vectors.key_ring()).unwrap();
        let new = license::verify(&replacement, &vectors.key_ring()).unwrap();
        assert_eq!(old.sub, new.sub);
        assert_ne!(old.jti, new.jti);
        assert_eq!(store.save(&replacement), Ok(lifetime()));
        assert_eq!(
            store.save(&refunded),
            Err(StoreError::License(LicenseError::Revoked))
        );
        assert_eq!(store.status(), lifetime());
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            format!("{replacement}\n")
        );
        let reloaded =
            super::tests::store(&dir, &vectors, &account).with_revoked_jtis(&["refunded-key"]);
        assert_eq!(reloaded.status(), lifetime());
    }

    #[test]
    fn a_stored_revoked_key_is_free_before_coverage_or_account_checks() {
        let dir = TestDir::new("license-load-revoked");
        let vectors = test_vectors::load();
        let account = MockTransport::authorized();
        let refunded = vectors.lifetime_token(MOCK_ME_ID, &[], "refunded-key");
        let path = dir.path().join(FILE_NAME);
        fs::write(&path, &refunded).unwrap();
        let AuthState::Ready(mut me) = account.auth_state().borrow().clone() else {
            panic!("mock account must be ready");
        };
        me.user_id += 1;
        let (_, wrong_account) = watch::channel(AuthState::Ready(me));
        for build_major in [1, 2] {
            for auth in [
                None,
                Some(account.auth_state()),
                Some(wrong_account.clone()),
            ] {
                let store = LicenseEntitlements::load(path.clone(), vectors.key_ring(), auth)
                    .with_build_major(build_major)
                    .with_revoked_jtis(&["refunded-key"]);
                assert_eq!(
                    store.status(),
                    status(EntitlementSource::Rejected, None, None, Some("LIC_REVOKED"))
                );
                assert_eq!(store.problem(), Some(LicenseError::Revoked));
                assert!(Feature::ALL.iter().all(|f| !store.allows(*f)));
                assert_eq!(
                    store.save(&refunded),
                    Err(StoreError::License(LicenseError::Revoked))
                );
                assert_eq!(fs::read_to_string(&path).unwrap(), refunded);
            }
        }
    }

    #[test]
    fn a_revoked_refusal_journals_no_key_or_jti() {
        let dir = TestDir::new("license-revoked-journal");
        let journal = TestJournal::new(&dir.path().join("logs"), "info");
        let vectors = test_vectors::load();
        let account = MockTransport::authorized();
        let refunded = vectors.lifetime_token(MOCK_ME_ID, &[], "refunded-key");
        let store = store(&dir, &vectors, &account).with_revoked_jtis(&["refunded-key"]);
        store.save(&refunded).unwrap_err();
        fs::write(dir.path().join(FILE_NAME), &refunded).unwrap();
        let reloaded =
            super::tests::store(&dir, &vectors, &account).with_revoked_jtis(&["refunded-key"]);
        assert_eq!(reloaded.problem(), Some(LicenseError::Revoked));
        let lines = journal.lines();
        for event in ["license_set", "license_status"] {
            let line = lines.iter().find(|line| line["event"] == event).unwrap();
            assert_eq!(line["error_code"], json!("LIC_REVOKED"));
        }
        let evaluated = lines
            .iter()
            .find(|line| line["event"] == "license_status")
            .unwrap();
        assert!(evaluated["fields"]["covers_major"].is_null());
        let license = license::verify(&refunded, &vectors.key_ring()).unwrap();
        let logged = journal.text();
        for secret in [
            refunded.as_str(),
            refunded.split('.').nth(1).unwrap(),
            &license.sub,
            &license.jti,
        ] {
            assert!(!logged.contains(secret));
        }
    }

    #[test]
    fn a_licence_issued_with_a_short_list_unlocks_every_feature() {
        // Licences sold before watched folders and background mode list only
        // `advanced_encoding` (or two features): stored as they are, no reissue.
        let dir = TestDir::new("license-short-list");
        let vectors = test_vectors::load();
        let account = MockTransport::authorized();
        let short = vectors.lifetime_token(MOCK_ME_ID, &["advanced_encoding"], "short-list");
        fs::write(dir.path().join(FILE_NAME), &short).unwrap();
        let store = store(&dir, &vectors, &account);
        for feature in Feature::ALL {
            assert!(store.allows(feature), "{feature:?}");
        }
        let status = store.status();
        assert_eq!(status, lifetime());
        assert_eq!(
            serde_json::to_value(&status).unwrap()["features"],
            json!([
                "advanced_encoding",
                "multi_destination",
                "watched_folder",
                "background"
            ])
        );
        // Account binding is unchanged: not signed in, nothing is granted.
        let signed_out =
            LicenseEntitlements::load(dir.path().join(FILE_NAME), vectors.key_ring(), None);
        assert_eq!(signed_out.status(), pending());
        assert!(Feature::ALL.iter().all(|f| !signed_out.allows(*f)));
    }

    #[test]
    fn no_file_is_the_free_plan() {
        let dir = TestDir::new("license-absent");
        let vectors = test_vectors::load();
        let path = dir.path().join("missing").join(FILE_NAME);
        let store = LicenseEntitlements::load(path.clone(), vectors.key_ring(), None);
        assert_eq!(store.status(), free_none());
        assert_eq!(store.problem(), None);
        assert!(Feature::ALL.iter().all(|f| !store.allows(*f)));
        assert!(!path.exists(), "reading never creates the file");
        // An empty file reads as no license.
        fs::write(dir.path().join(FILE_NAME), " \r\n").unwrap();
        let account = MockTransport::authorized();
        assert_eq!(
            super::tests::store(&dir, &vectors, &account).status(),
            free_none()
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
        let contents: Vec<(Vec<u8>, &str)> = vec![
            (vec![0xFF, 0xFE, 0x00, 0x80, 0xC3], "LIC_MALFORMED"),
            (b"garbage".to_vec(), "LIC_MALFORMED"),
            (b"SQZ1...".to_vec(), "LIC_MALFORMED"),
            (huge, "LIC_MALFORMED"),
            (token(&vectors, "prefix_sqz2").into_bytes(), "LIC_MALFORMED"),
            (
                token(&vectors, "payload_byte_altered").into_bytes(),
                "LIC_BAD_SIGNATURE",
            ),
            (
                format!("{0}\n{0}", token(&vectors, "valid")).into_bytes(),
                "LIC_MALFORMED",
            ),
        ];
        for (content, code) in contents {
            fs::write(&path, &content).unwrap();
            let store = store(&dir, &vectors, &account);
            assert_eq!(
                store.status(),
                status(EntitlementSource::Rejected, None, None, Some(code))
            );
            assert!(Feature::ALL.iter().all(|f| !store.allows(*f)));
        }
        // A folder where the file should be: unreadable, still free.
        fs::remove_file(&path).unwrap();
        fs::create_dir(&path).unwrap();
        let store = store(&dir, &vectors, &account);
        assert_eq!(store.status().plan, Plan::Free);
        assert_eq!(store.problem(), Some(LicenseError::Malformed));
    }

    #[test]
    fn another_account_is_rejected_and_no_account_is_pending() {
        let dir = TestDir::new("license-accounts");
        let vectors = test_vectors::load();
        fs::write(dir.path().join(FILE_NAME), token(&vectors, "other_account")).unwrap();
        let account = MockTransport::authorized();
        let other = store(&dir, &vectors, &account);
        assert_eq!(
            other.status(),
            status(
                EntitlementSource::Rejected,
                Some(false),
                Some(1),
                Some("LIC_WRONG_ACCOUNT")
            )
        );
        assert!(Feature::ALL.iter().all(|f| !other.allows(*f)));

        fs::write(dir.path().join(FILE_NAME), token(&vectors, "valid")).unwrap();
        let signed_out =
            LicenseEntitlements::load(dir.path().join(FILE_NAME), vectors.key_ring(), None);
        assert_eq!(signed_out.status(), pending());
        assert!(!signed_out.allows(Feature::AdvancedEncoding));
        assert_eq!(signed_out.problem(), Some(LicenseError::NoAccount));
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
            assert_eq!(store.status(), lifetime(), "{name}");
        }
        assert_eq!(
            fs::read_to_string(dir.path().join(FILE_NAME)).unwrap(),
            format!("{valid}\n")
        );
        // Signed out: a license is kept, then checked at sign-in.
        let signed_out = LicenseEntitlements::load(
            dir.path().join("other").join(FILE_NAME),
            vectors.key_ring(),
            None,
        );
        let status = signed_out.save(&token(&vectors, "other_account")).unwrap();
        assert_eq!(status, pending());
        assert_eq!(signed_out.problem(), Some(LicenseError::NoAccount));
    }

    /// A store on `dir` that judges coverage as the build of major `build_major`.
    fn store_for_major(
        dir: &TestDir,
        vectors: &Vectors,
        transport: &dyn TelegramTransport,
        build_major: u32,
    ) -> LicenseEntitlements {
        store(dir, vectors, transport).with_build_major(build_major)
    }

    #[test]
    fn the_build_major_is_the_package_major_read_at_compile_time() {
        assert_eq!(BUILD_MAJOR.to_string(), env!("CARGO_PKG_VERSION_MAJOR"));
        for (text, major) in [("0", 0), ("1", 1), ("12", 12), ("999", 999), ("007", 7)] {
            assert_eq!(parse_major(text), major, "{text}");
        }
        let dir = TestDir::new("license-build-major");
        let vectors = test_vectors::load();
        let loaded =
            LicenseEntitlements::load(dir.path().join(FILE_NAME), vectors.key_ring(), None);
        assert_eq!(loaded.build_major, BUILD_MAJOR);
        let production = LicenseEntitlements::production(dir.path().join(FILE_NAME), None);
        assert_eq!(production.build_major, BUILD_MAJOR);
    }

    #[test]
    fn a_key_applies_to_the_majors_up_to_its_maj() {
        let dir = TestDir::new("license-coverage");
        let vectors = test_vectors::load();
        let account = MockTransport::authorized();
        let one = token(&vectors, "valid");
        let two = token(&vectors, "maj_2");
        let two_status = EntitlementStatus {
            covers_major: Some(2),
            ..lifetime()
        };
        // (token, build major, what the store says): maj 1 covers 0.x and 1.x, not 2.x.
        for (key, build_major, expected) in [
            (&one, 0, lifetime()),
            (&one, 1, lifetime()),
            (&one, 2, older_major(1)),
            (&one, 3, older_major(1)),
            (&two, 0, two_status.clone()),
            (&two, 1, two_status.clone()),
            (&two, 2, two_status.clone()),
            (&two, 3, older_major(2)),
        ] {
            fs::write(dir.path().join(FILE_NAME), key).unwrap();
            let store = store_for_major(&dir, &vectors, &account, build_major);
            let granted = expected.plan == Plan::Lifetime;
            assert_eq!(store.status(), expected, "build {build_major}");
            for feature in Feature::ALL {
                assert_eq!(store.allows(feature), granted, "build {build_major}");
            }
            let problem = (!granted).then_some(LicenseError::OlderMajor);
            assert_eq!(store.problem(), problem, "build {build_major}");
        }
    }

    #[test]
    fn an_older_major_wins_over_the_account_checks() {
        let dir = TestDir::new("license-priority");
        let vectors = test_vectors::load();
        let account = MockTransport::authorized();
        let valid = token(&vectors, "valid");
        let other = token(&vectors, "other_account");
        let path = dir.path().join(FILE_NAME);

        // Signed in, the key is for another account: wrong account while the major is covered,
        // older major (and nothing said about the account) once it is not.
        fs::write(&path, &other).unwrap();
        let covered = store_for_major(&dir, &vectors, &account, 1);
        assert_eq!(
            covered.status(),
            status(
                EntitlementSource::Rejected,
                Some(false),
                Some(1),
                Some("LIC_WRONG_ACCOUNT")
            )
        );
        let refused = store_for_major(&dir, &vectors, &account, 2);
        assert_eq!(refused.status(), older_major(1));
        assert_eq!(refused.problem(), Some(LicenseError::OlderMajor));

        // Nobody signed in: pending while covered, older major (not pending) once it is not.
        fs::write(&path, &valid).unwrap();
        let load = |major| {
            LicenseEntitlements::load(path.clone(), vectors.key_ring(), None)
                .with_build_major(major)
        };
        assert_eq!(load(1).status(), pending());
        assert_eq!(load(2).status(), older_major(1));
        assert_eq!(load(2).problem(), Some(LicenseError::OlderMajor));
        assert!(!load(2).allows(Feature::AdvancedEncoding));

        // The signature still comes first: an unverified key says nothing about its major.
        fs::write(&path, token(&vectors, "payload_byte_altered")).unwrap();
        let invalid = store_for_major(&dir, &vectors, &account, 2);
        assert_eq!(
            invalid.status(),
            status(
                EntitlementSource::Rejected,
                None,
                None,
                Some("LIC_BAD_SIGNATURE")
            )
        );
    }

    #[test]
    fn save_refuses_an_older_major_and_keeps_the_file() {
        let dir = TestDir::new("license-save-major");
        let vectors = test_vectors::load();
        let account = MockTransport::authorized();
        let path = dir.path().join(FILE_NAME);
        let store = store_for_major(&dir, &vectors, &account, 2);
        // Nothing stored yet: the refusal creates no file.
        for name in ["valid", "other_account"] {
            let error = store.save(&token(&vectors, name)).unwrap_err();
            assert_eq!(
                error,
                StoreError::License(LicenseError::OlderMajor),
                "{name}"
            );
            assert_eq!(error.code(), "LIC_OLDER_MAJOR");
        }
        assert!(!path.exists());
        assert_eq!(store.status(), free_none());

        // A key that covers the build is stored, and a later older-major key leaves it be.
        let two = token(&vectors, "maj_2");
        let applies = EntitlementStatus {
            covers_major: Some(2),
            ..lifetime()
        };
        assert_eq!(store.save(&two), Ok(applies.clone()));
        assert_eq!(
            store.save(&token(&vectors, "valid")),
            Err(StoreError::License(LicenseError::OlderMajor))
        );
        assert_eq!(store.status(), applies);
        assert_eq!(fs::read_to_string(&path).unwrap(), format!("{two}\n"));

        // Signed out, a key is normally kept for sign-in; not one that cannot apply anyway.
        let signed_out = LicenseEntitlements::load(
            dir.path().join("other").join(FILE_NAME),
            vectors.key_ring(),
            None,
        )
        .with_build_major(2);
        assert_eq!(
            signed_out.save(&token(&vectors, "valid")),
            Err(StoreError::License(LicenseError::OlderMajor))
        );
        assert!(!dir.path().join("other").join(FILE_NAME).exists());
        assert_eq!(
            signed_out.save(&two).unwrap().source,
            EntitlementSource::Pending
        );

        // The major it was bought for is still accepted by a build that covers it.
        let covered = store_for_major(&dir, &vectors, &account, 1);
        assert_eq!(covered.save(&token(&vectors, "valid")), Ok(lifetime()));
    }

    #[test]
    fn the_status_page_shape_carries_covers_major() {
        let dir = TestDir::new("license-shape");
        let vectors = test_vectors::load();
        let account = MockTransport::authorized();
        let path = dir.path().join(FILE_NAME);
        let shape = |build_major: u32, name: &str| {
            fs::write(&path, token(&vectors, name)).unwrap();
            serde_json::to_value(store_for_major(&dir, &vectors, &account, build_major).status())
                .unwrap()
        };
        assert_eq!(
            shape(1, "valid"),
            json!({
                "plan": "lifetime",
                "source": "license",
                "features": ["advanced_encoding", "multi_destination", "watched_folder", "background"],
                "account_matches": true,
                "covers_major": 1,
                "reason": null
            })
        );
        assert_eq!(
            shape(2, "valid"),
            json!({
                "plan": "free",
                "source": "rejected",
                "features": [],
                "account_matches": null,
                "covers_major": 1,
                "reason": "LIC_OLDER_MAJOR"
            })
        );
        assert_eq!(shape(2, "maj_2")["covers_major"], json!(2));
        // Not verified: no `maj` to show, whatever the token claims.
        for name in [
            "payload_byte_altered",
            "unknown_kid",
            "truncated",
            "maj_missing",
            "maj_string",
        ] {
            let status = shape(1, name);
            assert_eq!(status["covers_major"], json!(null), "{name}");
            assert_ne!(status["reason"], json!(null), "{name}");
        }
    }

    #[test]
    fn an_older_major_is_journalled_with_the_code_and_the_maj_only() {
        let dir = TestDir::new("license-older-major-journal");
        let journal = TestJournal::new(&dir.path().join("logs"), "info");
        let vectors = test_vectors::load();
        let account = MockTransport::authorized();
        let store = store_for_major(&dir, &vectors, &account, 2);
        store.save(&token(&vectors, "valid")).unwrap_err();
        fs::write(dir.path().join(FILE_NAME), token(&vectors, "valid")).unwrap();
        let reloaded = store_for_major(&dir, &vectors, &account, 2);
        assert_eq!(reloaded.status(), older_major(1));
        let lines = journal.lines();
        let set = lines.iter().find(|l| l["event"] == "license_set").unwrap();
        assert_eq!(set["error_code"], json!("LIC_OLDER_MAJOR"));
        assert_eq!(set["fields"]["kid"], json!("test-1"));
        let evaluated = lines
            .iter()
            .find(|l| l["event"] == "license_status")
            .unwrap();
        assert_eq!(evaluated["error_code"], json!("LIC_OLDER_MAJOR"));
        assert_eq!(evaluated["fields"]["source"], json!("rejected"));
        assert_eq!(evaluated["fields"]["kid"], json!("test-1"));
        assert_eq!(evaluated["fields"]["covers_major"], json!(1));
        let logged = journal.text();
        let valid = token(&vectors, "valid");
        let license = license::verify(&valid, &vectors.key_ring()).unwrap();
        for secret in [
            valid.as_str(),
            valid.split('.').nth(1).unwrap(),
            &license.sub,
            &license.jti,
        ] {
            assert!(!logged.contains(secret));
        }
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
        assert_eq!(store.status(), pending());
        assert_eq!(store.problem(), Some(LicenseError::NoAccount));

        later
            .auth_submit(AuthInput::Phone("+33600000000".into()))
            .await
            .unwrap();
        later
            .auth_submit(AuthInput::Code(MOCK_CODE.into()))
            .await
            .unwrap();
        assert_eq!(store.status(), lifetime());
        assert!(store.allows(Feature::MultiDestination));

        later.log_out().await.unwrap();
        assert_eq!(store.status(), pending());
        assert!(!store.allows(Feature::MultiDestination));

        store.clear().unwrap();
        assert_eq!(store.status(), free_none());

        let lines: Vec<serde_json::Value> = journal
            .lines()
            .iter()
            .filter(|l| l["event"] == "license_status")
            .map(|l| {
                json!({
                    "source": l["fields"]["source"],
                    "kid": l["fields"]["kid"],
                    "covers_major": l["fields"]["covers_major"],
                    "features": l["fields"]["features"],
                })
            })
            .collect();
        assert_eq!(
            lines,
            [
                json!({ "source": "pending", "kid": "test-1", "covers_major": 1, "features": [] }),
                json!({
                    "source": "license",
                    "kid": "test-1",
                    "covers_major": 1,
                    "features": [
                        "advanced_encoding",
                        "multi_destination",
                        "watched_folder",
                        "background"
                    ],
                }),
                json!({ "source": "pending", "kid": "test-1", "covers_major": 1, "features": [] }),
                json!({ "source": "none", "kid": null, "covers_major": null, "features": [] }),
            ],
            "one line per change, none for repeated reads"
        );
    }

    #[test]
    fn journal_lines_carry_the_code_and_the_verified_kid_only() {
        let dir = TestDir::new("license-lines");
        let journal = TestJournal::new(&dir.path().join("logs"), "info");
        let vectors = test_vectors::load();
        let account = MockTransport::authorized();
        let store = store(&dir, &vectors, &account);
        store.save(&token(&vectors, "unknown_kid")).unwrap_err();
        store.save(&token(&vectors, "other_account")).unwrap_err();
        store.save(&token(&vectors, "valid")).unwrap();
        store.clear().unwrap();
        let lines = journal.lines();
        let set: Vec<(serde_json::Value, serde_json::Value)> = lines
            .iter()
            .filter(|l| l["event"] == "license_set")
            .map(|l| (l["error_code"].clone(), l["fields"]["kid"].clone()))
            .collect();
        // An unknown kid is text from the token: never written.
        assert_eq!(
            set,
            [
                (json!("LIC_UNKNOWN_KID"), serde_json::Value::Null),
                (json!("LIC_WRONG_ACCOUNT"), json!("test-1")),
                (serde_json::Value::Null, json!("test-1")),
            ]
        );
        assert!(lines.iter().any(|l| l["event"] == "license_cleared"));
        let license_events: Vec<&str> = lines
            .iter()
            .filter_map(|l| l["event"].as_str())
            .filter(|e| e.starts_with("license"))
            .collect();
        assert!(
            license_events
                .iter()
                .all(|e| ["license_set", "license_cleared", "license_status"].contains(e)),
            "{license_events:?}"
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
        let names: Vec<String> = zip
            .file_names()
            .map(|name| name.unwrap().into_owned())
            .collect();
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
        assert!(logged.contains("license_status") && logged.contains("license_set"));
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
