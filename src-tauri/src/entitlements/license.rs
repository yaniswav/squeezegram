//! License key format: `SQZ1.<base64url(payload)>.<base64url(sig)>`.
//!
//! - base64url without padding, strict: `=` padding, the standard alphabet or non-canonical
//!   trailing bits are malformed.
//! - The signature is Ed25519 (`verify_strict`) over the ASCII bytes `SQZ1.<payload part>` exactly
//!   as received: nothing is decoded and re-encoded before verification.
//! - The payload is JSON: `v` (1), `kid`, `sub`, `plan` (`lifetime`), `maj`, `feat`, `iat`,
//!   `jti`. Unknown fields and unknown feature names are ignored; `iat` is informational (a date
//!   in the future is accepted: the user's clock is not trusted either way).
//! - `maj` is the highest app major version the key covers, an integer in 1..=999 and required:
//!   a token without it is malformed. Every key sold today has `maj` 1, which covers 0.x and 1.x;
//!   a build of a later major refuses it (`OlderMajor`, see `check_coverage`).
//! - `feat` is parsed and kept, but a `lifetime` licence grants every feature whatever it lists
//!   (`Plan::grants_every_feature`): licences issued before a feature existed unlock it too, as
//!   long as the app major is covered by `maj`.
//! - `sub` binds the license to a Telegram account without carrying its id: see `subject_for`.
//!
//! Neither the token nor the payload ever reaches a `Debug`, a `Display` or the journal.

use std::fmt;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ed25519_dalek::{Signature, VerifyingKey};
use serde::Deserialize;
use sha2::{Digest, Sha256};

use super::{Feature, Plan};

/// Format and version marker of the token.
pub const PREFIX: &str = "SQZ1";
/// `v` of the payload for `SQZ1`.
pub const PAYLOAD_VERSION: u32 = 1;
/// Bounds of `maj` (an issuer bug, not a real major, beyond them).
pub const MAJOR_MIN: u64 = 1;
pub const MAJOR_MAX: u64 = 999;
/// Domain separation of `subject_for`.
pub const SUBJECT_DOMAIN: &[u8] = b"squeezegram-lic-v1";
/// Far above any issued key (about 400 characters); longer input is refused before decoding.
pub const MAX_TOKEN_LEN: usize = 4096;

/// Why a license does not apply. The texts and `Debug` name the reason, never the token.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum LicenseError {
    #[error("license key is malformed")]
    Malformed,
    #[error("license key is signed by an unknown key id")]
    UnknownKid,
    #[error("license key signature does not verify")]
    BadSignature,
    #[error("license key was refunded and is no longer valid")]
    Revoked,
    #[error("license key belongs to another Telegram account")]
    WrongAccount,
    #[error("no Telegram account is signed in to check the license key against")]
    NoAccount,
    #[error("license key covers only older major versions of the app")]
    OlderMajor,
}

impl LicenseError {
    pub fn code(self) -> &'static str {
        match self {
            LicenseError::Malformed => "LIC_MALFORMED",
            LicenseError::UnknownKid => "LIC_UNKNOWN_KID",
            LicenseError::BadSignature => "LIC_BAD_SIGNATURE",
            LicenseError::Revoked => "LIC_REVOKED",
            LicenseError::WrongAccount => "LIC_WRONG_ACCOUNT",
            LicenseError::NoAccount => "LIC_NO_ACCOUNT",
            LicenseError::OlderMajor => "LIC_OLDER_MAJOR",
        }
    }
}

/// The public keys a build trusts, by `kid` (rotation: a new key gets a new `kid`, the old one
/// stays as long as its licenses must keep working). Empty in the public build.
#[derive(Clone, Default)]
pub struct KeyRing {
    keys: Vec<(String, VerifyingKey)>,
}

impl KeyRing {
    pub fn empty() -> Self {
        Self::default()
    }

    /// Adds a 32-byte Ed25519 public key under `kid`; refuses bytes that are not a curve point.
    pub fn with_key(
        mut self,
        kid: &str,
        public: &[u8; 32],
    ) -> Result<Self, ed25519_dalek::SignatureError> {
        let key = VerifyingKey::from_bytes(public)?;
        self.keys.retain(|(known, _)| known != kid);
        self.keys.push((kid.to_owned(), key));
        Ok(self)
    }

    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    fn get(&self, kid: &str) -> Option<&VerifyingKey> {
        self.keys
            .iter()
            .find(|(known, _)| known == kid)
            .map(|(_, key)| key)
    }
}

impl fmt::Debug for KeyRing {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list()
            .entries(self.keys.iter().map(|(kid, _)| kid))
            .finish()
    }
}

/// A license whose signature verified. Not yet checked against an account: `check_account`.
#[derive(Clone, PartialEq, Eq)]
pub struct License {
    pub kid: String,
    pub sub: String,
    pub plan: Plan,
    /// Highest app major version the key covers: every Premium feature of the majors up to it.
    pub maj: u32,
    /// The `feat` list: known features only, in license order. What the licence names, not
    /// what it grants: see `allows`.
    pub features: Vec<Feature>,
    pub iat: i64,
    pub jti: String,
}

impl License {
    /// Every feature on a plan that grants them all (`lifetime`, whatever `feat` lists), else
    /// only the listed ones.
    pub fn allows(&self, feature: Feature) -> bool {
        self.plan.grants_every_feature() || self.features.contains(&feature)
    }

    /// The features `allows` grants, in `Feature::ALL` order.
    pub fn granted(&self) -> impl Iterator<Item = Feature> + '_ {
        Feature::ALL
            .into_iter()
            .filter(|feature| self.allows(*feature))
    }
}

/// Written by hand: `sub` and `jti` identify a purchase; only the plan, the major and a count
/// show.
impl fmt::Debug for License {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("License")
            .field("plan", &self.plan)
            .field("maj", &self.maj)
            .field("features", &self.features.len())
            .finish_non_exhaustive()
    }
}

/// The JSON payload; fields not listed here are ignored.
#[derive(Deserialize)]
struct Payload {
    v: u32,
    kid: String,
    sub: String,
    plan: String,
    /// Raw JSON, judged after the signature (like `plan` and `sub`): a wrongly typed `maj` must
    /// not turn the error of an unsigned or badly signed token into `Malformed`. Absent and
    /// `null` both land on `None`.
    #[serde(default)]
    maj: Option<serde_json::Value>,
    feat: Vec<String>,
    iat: i64,
    jti: String,
}

/// `maj` as a JSON unsigned integer in `MAJOR_MIN..=MAJOR_MAX`: not a float (`1.0` included), a
/// string, a bool, `null`, a negative or a number too large for `u64`.
fn major_of(value: Option<&serde_json::Value>) -> Option<u32> {
    let major = value?.as_u64()?;
    if (MAJOR_MIN..=MAJOR_MAX).contains(&major) {
        u32::try_from(major).ok()
    } else {
        None
    }
}

/// `hex(SHA-256(b"squeezegram-lic-v1" || decimal ASCII of user_id))`, lowercase: what `sub`
/// holds for the account a license was issued to.
pub fn subject_for(user_id: i64) -> String {
    let mut hasher = Sha256::new();
    hasher.update(SUBJECT_DOMAIN);
    hasher.update(user_id.to_string().as_bytes());
    hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn decode_part(part: &str) -> Result<Vec<u8>, LicenseError> {
    URL_SAFE_NO_PAD
        .decode(part)
        .map_err(|_| LicenseError::Malformed)
}

/// Checks the format and the signature against `keys`. Surrounding whitespace is ignored
/// (copy and paste); anything else that is not the exact format is `Malformed`.
pub fn verify(token: &str, keys: &KeyRing) -> Result<License, LicenseError> {
    let token = token.trim();
    if token.len() > MAX_TOKEN_LEN || !token.is_ascii() {
        return Err(LicenseError::Malformed);
    }
    let (signed, signature_part) = token.rsplit_once('.').ok_or(LicenseError::Malformed)?;
    let (prefix, payload_part) = signed.split_once('.').ok_or(LicenseError::Malformed)?;
    if prefix != PREFIX || payload_part.is_empty() || payload_part.contains('.') {
        return Err(LicenseError::Malformed);
    }
    let signature = <[u8; 64]>::try_from(decode_part(signature_part)?.as_slice())
        .map_err(|_| LicenseError::Malformed)?;
    let payload: Payload =
        serde_json::from_slice(&decode_part(payload_part)?).map_err(|_| LicenseError::Malformed)?;
    if payload.v != PAYLOAD_VERSION {
        return Err(LicenseError::Malformed);
    }
    let key = keys.get(&payload.kid).ok_or(LicenseError::UnknownKid)?;
    key.verify_strict(signed.as_bytes(), &Signature::from_bytes(&signature))
        .map_err(|_| LicenseError::BadSignature)?;

    // Signed by us: what follows only guards against an issuer bug.
    let plan = match payload.plan.as_str() {
        "lifetime" => Plan::Lifetime,
        _ => return Err(LicenseError::Malformed),
    };
    let maj = major_of(payload.maj.as_ref()).ok_or(LicenseError::Malformed)?;
    let sub_ok = payload.sub.len() == 64
        && payload
            .sub
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
    if !sub_ok {
        return Err(LicenseError::Malformed);
    }
    let mut features = Vec::new();
    for feature in payload
        .feat
        .iter()
        .filter_map(|name| Feature::from_name(name))
    {
        if !features.contains(&feature) {
            features.push(feature);
        }
    }
    Ok(License {
        kid: payload.kid,
        sub: payload.sub,
        plan,
        maj,
        features,
        iat: payload.iat,
        jti: payload.jti,
    })
}

/// The key covers the app major version `build_major` (`maj` is the highest one it covers).
pub fn check_coverage(license: &License, build_major: u32) -> Result<(), LicenseError> {
    if build_major <= license.maj {
        Ok(())
    } else {
        Err(LicenseError::OlderMajor)
    }
}

/// The license applies only to the account it was issued to.
pub fn check_account(license: &License, user_id: Option<i64>) -> Result<(), LicenseError> {
    let user_id = user_id.ok_or(LicenseError::NoAccount)?;
    if license.sub == subject_for(user_id) {
        Ok(())
    } else {
        Err(LicenseError::WrongAccount)
    }
}

/// `verify` then `check_account`, for the tests. The major version of the build is not checked
/// here: the store does it (`check_coverage`) before the account, so nothing outside the tests
/// may grant from this.
#[cfg(test)]
pub fn verify_for(
    token: &str,
    keys: &KeyRing,
    user_id: Option<i64>,
) -> Result<License, LicenseError> {
    let license = verify(token, keys)?;
    check_account(&license, user_id)?;
    Ok(license)
}

#[cfg(test)]
pub(crate) mod test_vectors {
    //! `fixtures/vectors.json`: a throwaway key whose seed is published on purpose (TEST KEY,
    //! NOT FOR PRODUCTION) and tokens generated outside Rust (Python `cryptography`).

    use serde::Deserialize;

    use super::KeyRing;

    pub const JSON: &str = include_str!("fixtures/vectors.json");

    #[derive(Deserialize)]
    pub struct Vectors {
        pub test_key: TestKey,
        pub subjects: Vec<Subject>,
        pub cases: Vec<Case>,
    }

    #[derive(Deserialize)]
    pub struct TestKey {
        pub kid: String,
        pub seed_hex: String,
        pub public_hex: String,
    }

    #[derive(Deserialize)]
    pub struct Subject {
        pub user_id: i64,
        pub sub: String,
    }

    #[derive(Deserialize)]
    pub struct Case {
        pub name: String,
        pub token: String,
        pub user_id: Option<i64>,
        pub expect: String,
    }

    pub fn load() -> Vectors {
        serde_json::from_str(JSON).unwrap()
    }

    pub fn hex32(text: &str) -> [u8; 32] {
        let bytes: Vec<u8> = (0..text.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&text[i..i + 2], 16).unwrap())
            .collect();
        bytes.try_into().unwrap()
    }

    impl Vectors {
        pub fn key_ring(&self) -> KeyRing {
            KeyRing::empty()
                .with_key(&self.test_key.kid, &hex32(&self.test_key.public_hex))
                .unwrap()
        }

        pub fn case(&self, name: &str) -> &Case {
            self.cases.iter().find(|c| c.name == name).unwrap()
        }

        /// A token for `payload`, signed with the test key (its seed is public): for a payload
        /// the vectors do not have.
        pub fn sign(&self, payload: &serde_json::Value) -> String {
            self.sign_text(&serde_json::to_string(payload).unwrap())
        }

        /// Like `sign`, for payload text `serde_json::Value` cannot hold as written (`1.0`, a
        /// number beyond `u64`).
        pub fn sign_text(&self, payload: &str) -> String {
            use base64::Engine;
            use base64::engine::general_purpose::URL_SAFE_NO_PAD;
            use ed25519_dalek::{Signer, SigningKey};

            let signed = format!("{}.{}", super::PREFIX, URL_SAFE_NO_PAD.encode(payload));
            let signature =
                SigningKey::from_bytes(&hex32(&self.test_key.seed_hex)).sign(signed.as_bytes());
            format!("{signed}.{}", URL_SAFE_NO_PAD.encode(signature.to_bytes()))
        }

        /// A `lifetime` licence for `user_id` covering up to major `maj`, whose `feat` is `feat`.
        pub fn lifetime_token_for_major(
            &self,
            user_id: i64,
            maj: u32,
            feat: &[&str],
            jti: &str,
        ) -> String {
            self.sign(&serde_json::json!({
                "v": 1,
                "kid": self.test_key.kid,
                "sub": super::subject_for(user_id),
                "plan": "lifetime",
                "maj": maj,
                "feat": feat,
                "iat": 1_700_000_000,
                "jti": jti,
            }))
        }

        /// A `lifetime` licence for `user_id` covering major 1 (what is sold), whose `feat` is
        /// `feat`.
        pub fn lifetime_token(&self, user_id: i64, feat: &[&str], jti: &str) -> String {
            self.lifetime_token_for_major(user_id, 1, feat, jti)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_vectors::{self, hex32};
    use super::*;

    #[test]
    fn decode_part_is_strict_base64url_without_padding() {
        // Pinned across base64 upgrades: these are the three ways a key part can be malformed.
        assert_eq!(decode_part("AA"), Ok(vec![0]));
        assert_eq!(decode_part("-_8"), Ok(vec![0xfb, 0xff]));
        assert_eq!(
            decode_part("AB"),
            Err(LicenseError::Malformed),
            "trailing bits"
        );
        assert_eq!(decode_part("AA=="), Err(LicenseError::Malformed), "padding");
        assert_eq!(
            decode_part("+/8"),
            Err(LicenseError::Malformed),
            "standard alphabet"
        );
        assert_eq!(decode_part("A"), Err(LicenseError::Malformed), "length");
    }

    #[test]
    fn subject_matches_the_python_reference() {
        let vectors = test_vectors::load();
        assert!(!vectors.subjects.is_empty());
        for subject in &vectors.subjects {
            assert_eq!(subject_for(subject.user_id), subject.sub);
        }
        assert_eq!(subject_for(1).len(), 64);
        assert_ne!(subject_for(1), subject_for(10));
    }

    #[test]
    fn every_vector_gives_its_expected_result() {
        let vectors = test_vectors::load();
        let keys = vectors.key_ring();
        assert!(vectors.cases.len() >= 22);
        for case in &vectors.cases {
            let result = verify_for(&case.token, &keys, case.user_id);
            let got = match &result {
                Ok(_) => "ok",
                Err(error) => error.code(),
            };
            assert_eq!(got, case.expect, "case {}", case.name);
        }
    }

    #[test]
    fn a_valid_license_carries_its_plan_and_known_features() {
        let vectors = test_vectors::load();
        let keys = vectors.key_ring();
        let license = verify(&vectors.case("valid").token, &keys).unwrap();
        assert_eq!(license.plan, Plan::Lifetime);
        assert_eq!(
            license.features,
            [Feature::AdvancedEncoding, Feature::MultiDestination]
        );
        // Lifetime: every feature, listed or not.
        assert!(Feature::ALL.iter().all(|f| license.allows(*f)));
        assert_eq!(license.kid, "test-1");
        assert_eq!(license.maj, 1);
        // An unknown feature name (`teleport`) and an unknown field are ignored.
        let tolerant = verify(&vectors.case("unknown_field_tolerated").token, &keys).unwrap();
        assert_eq!(tolerant.features, [Feature::AdvancedEncoding]);
        // Surrounding whitespace from a copy and paste is fine.
        let pasted = format!("  {}\r\n", vectors.case("valid").token);
        assert!(verify(&pasted, &keys).is_ok());
        // No key in the ring (the public build): unknown kid.
        assert_eq!(
            verify(&vectors.case("valid").token, &KeyRing::empty()),
            Err(LicenseError::UnknownKid)
        );
    }

    #[test]
    fn a_license_issued_with_caption_templates_still_verifies() {
        // Caption templates left the Feature enum (they will be free), but licences already
        // sold list "caption_templates" in `feat`.
        let vectors = test_vectors::load();
        let token = vectors.lifetime_token(
            777000001,
            &["caption_templates", "advanced_encoding"],
            "legacy-caption-templates",
        );
        let license = verify(&token, &vectors.key_ring()).unwrap();
        assert_eq!(license.features, [Feature::AdvancedEncoding]);
        assert!(license.allows(Feature::AdvancedEncoding));
        assert_eq!(Feature::from_name("caption_templates"), None);
    }

    #[test]
    fn a_lifetime_license_grants_every_feature_whatever_it_lists() {
        // What is sold is "Premium, lifetime": licences issued before watched folders and
        // background mode existed list only the first features, and must unlock all of them.
        let vectors = test_vectors::load();
        let keys = vectors.key_ring();
        for feat in [
            &["advanced_encoding"][..],
            &[],
            &["teleport"],
            &["advanced_encoding", "multi_destination"],
        ] {
            let token = vectors.lifetime_token(777000001, feat, "short-list");
            let license = verify_for(&token, &keys, Some(777000001)).unwrap();
            assert_eq!(license.plan, Plan::Lifetime);
            // The list is still parsed as before (unknown names dropped)...
            let listed: Vec<Feature> = feat.iter().filter_map(|n| Feature::from_name(n)).collect();
            assert_eq!(license.features, listed, "{feat:?}");
            // ...but does not limit a lifetime licence.
            for feature in Feature::ALL {
                assert!(license.allows(feature), "{feat:?} {feature:?}");
            }
            assert_eq!(license.granted().collect::<Vec<_>>(), Feature::ALL);
        }
    }

    #[test]
    fn a_plan_that_is_not_lifetime_grants_only_its_list() {
        // `verify` accepts no plan but `lifetime` today, so no token can carry another one. The
        // rule is checked on a `License` built directly: `Plan::Free` stands in for a future
        // limited plan (`Plan::grants_every_feature` is false for it).
        assert!(Plan::Lifetime.grants_every_feature());
        assert!(!Plan::Free.grants_every_feature());
        let vectors = test_vectors::load();
        let payload = serde_json::json!({
            "v": 1,
            "kid": vectors.test_key.kid,
            "sub": subject_for(777000001),
            "plan": "limited",
            "maj": 1,
            "feat": ["advanced_encoding"],
            "iat": 1_700_000_000,
            "jti": "limited",
        });
        assert_eq!(
            verify(&vectors.sign(&payload), &vectors.key_ring()),
            Err(LicenseError::Malformed)
        );
        let limited = License {
            kid: "test-1".into(),
            sub: subject_for(777000001),
            plan: Plan::Free,
            maj: 1,
            features: vec![Feature::AdvancedEncoding],
            iat: 0,
            jti: "limited".into(),
        };
        assert!(limited.allows(Feature::AdvancedEncoding));
        for feature in [
            Feature::MultiDestination,
            Feature::WatchedFolder,
            Feature::Background,
        ] {
            assert!(!limited.allows(feature), "{feature:?}");
        }
        assert_eq!(
            limited.granted().collect::<Vec<_>>(),
            [Feature::AdvancedEncoding]
        );
    }

    /// The payload of `lifetime_token` with `maj` written as the JSON text `maj` (or left out).
    fn payload_with_maj(kid: &str, maj: Option<&str>) -> String {
        let maj = maj
            .map(|text| format!(r#""maj":{text},"#))
            .unwrap_or_default();
        format!(
            r#"{{"v":1,"kid":"{kid}","sub":"{}","plan":"lifetime",{maj}"feat":[],"iat":1700000000,"jti":"maj"}}"#,
            subject_for(777000001),
        )
    }

    #[test]
    fn maj_is_required_and_must_be_an_integer_from_1_to_999() {
        let vectors = test_vectors::load();
        let keys = vectors.key_ring();
        for (maj, expected) in [
            (Some("1"), Some(1)),
            (Some("2"), Some(2)),
            (Some("999"), Some(999)),
            (None, None),
            (Some("null"), None),
            (Some("0"), None),
            (Some("-1"), None),
            (Some("1.5"), None),
            (Some("1.0"), None),
            (Some("1e0"), None),
            (Some(r#""1""#), None),
            (Some("true"), None),
            (Some("false"), None),
            (Some("[1]"), None),
            (Some("{}"), None),
            (Some("1000"), None),
            (Some("4294967297"), None),
            (Some("18446744073709551616"), None),
            (Some("99999999999999999999999999"), None),
        ] {
            let token = vectors.sign_text(&payload_with_maj("test-1", maj));
            let got = verify(&token, &keys).map(|license| license.maj);
            let want = expected.ok_or(LicenseError::Malformed);
            assert_eq!(got, want, "maj {maj:?}");
        }
    }

    #[test]
    fn a_bad_maj_never_changes_the_error_of_a_token_that_does_not_verify() {
        // The signature is checked first, `maj` after (like `plan` and `sub`): whatever type
        // `maj` has, an unknown key id stays `UnknownKid` and a wrong signature `BadSignature`.
        let vectors = test_vectors::load();
        let keys = vectors.key_ring();
        let valid_signature = vectors.case("valid").token.rsplit('.').next().unwrap();
        for maj in [
            None,
            Some("null"),
            Some("0"),
            Some("1.0"),
            Some(r#""1""#),
            Some("true"),
            Some("18446744073709551616"),
        ] {
            let unknown = vectors.sign_text(&payload_with_maj("prod-9", maj));
            assert_eq!(
                verify(&unknown, &keys),
                Err(LicenseError::UnknownKid),
                "{maj:?}"
            );
            let signed = vectors.sign_text(&payload_with_maj("test-1", maj));
            let (head, _) = signed.rsplit_once('.').unwrap();
            let forged = format!("{head}.{valid_signature}");
            assert_eq!(
                verify(&forged, &keys),
                Err(LicenseError::BadSignature),
                "{maj:?}"
            );
        }
    }

    #[test]
    fn a_key_covers_the_majors_up_to_its_maj() {
        let vectors = test_vectors::load();
        let keys = vectors.key_ring();
        let one = verify(&vectors.case("valid").token, &keys).unwrap();
        let two = verify(&vectors.case("maj_2").token, &keys).unwrap();
        assert_eq!((one.maj, two.maj), (1, 2));
        // What is sold today: 0.x and 1.x, not 2.x.
        assert_eq!(check_coverage(&one, 0), Ok(()));
        assert_eq!(check_coverage(&one, 1), Ok(()));
        assert_eq!(check_coverage(&one, 2), Err(LicenseError::OlderMajor));
        assert_eq!(check_coverage(&one, 999), Err(LicenseError::OlderMajor));
        assert_eq!(check_coverage(&two, 0), Ok(()));
        assert_eq!(check_coverage(&two, 1), Ok(()));
        assert_eq!(check_coverage(&two, 2), Ok(()));
        assert_eq!(check_coverage(&two, 3), Err(LicenseError::OlderMajor));
        assert_eq!(LicenseError::OlderMajor.code(), "LIC_OLDER_MAJOR");
        // A key that covers a major is still bound to its account: `verify_for` does not look
        // at the build major.
        assert_eq!(
            verify_for(&vectors.case("valid").token, &keys, Some(1)),
            Err(LicenseError::WrongAccount)
        );
    }

    #[test]
    fn a_key_ring_refuses_bytes_that_are_not_a_public_key() {
        let vectors = test_vectors::load();
        let public = hex32(&vectors.test_key.public_hex);
        // A key ring replaces an existing kid instead of holding it twice.
        let ring = KeyRing::empty()
            .with_key("a", &public)
            .unwrap()
            .with_key("a", &public)
            .unwrap();
        assert_eq!(format!("{ring:?}"), r#"["a"]"#);
        // y = 2 does not decompress to a curve point.
        let mut not_a_point = [0u8; 32];
        not_a_point[0] = 2;
        assert!(KeyRing::empty().with_key("b", &not_a_point).is_err());
    }

    #[test]
    fn debug_and_display_never_show_the_token_or_the_payload() {
        let vectors = test_vectors::load();
        let keys = vectors.key_ring();
        let token = &vectors.case("valid").token;
        let license = verify(token, &keys).unwrap();
        let mut texts = vec![format!("{license:?} {license:#?} {keys:?}")];
        for case in &vectors.cases {
            if let Err(error) = verify_for(&case.token, &keys, case.user_id) {
                texts.push(format!("{error} {error:?} {error:#?}"));
            }
        }
        let parts: Vec<&str> = token.split('.').collect();
        let text = texts.join("\n");
        for secret in [
            token.as_str(),
            parts[1],
            parts[2],
            &license.sub,
            &license.jti,
            "lifetime",
            "advanced_encoding",
        ] {
            assert!(!text.contains(secret), "{secret} leaked: {text}");
        }
    }

    /// splitmix64: deterministic, no dependency.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^ (z >> 31)
        }

        fn below(&mut self, n: usize) -> usize {
            (self.next() % n as u64) as usize
        }
    }

    #[test]
    fn a_thousand_random_strings_never_panic_and_never_verify() {
        let vectors = test_vectors::load();
        let keys = vectors.key_ring();
        let valid = vectors.case("valid").token.clone();
        let alphabet: Vec<char> =
            "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_+/=. \n\t{}\"é€😀\0"
                .chars()
                .collect();
        let mut rng = Rng(0x5157_5A31);
        let (mut round, mut tested) = (0usize, 0usize);
        while tested < 1000 {
            round += 1;
            let candidate = match round % 4 {
                // Free text from the alphabet, any length up to past the size cap.
                0 => {
                    let len = rng.below(if round % 40 == 0 { 5000 } else { 120 });
                    (0..len)
                        .map(|_| alphabet[rng.below(alphabet.len())])
                        .collect::<String>()
                }
                // The prefix and two dots, random parts.
                1 => {
                    let part = |rng: &mut Rng| {
                        (0..rng.below(100))
                            .map(|_| alphabet[rng.below(64)])
                            .collect::<String>()
                    };
                    format!("SQZ1.{}.{}", part(&mut rng), part(&mut rng))
                }
                // The valid token with one character replaced.
                2 => {
                    let mut chars: Vec<char> = valid.chars().collect();
                    let at = rng.below(chars.len());
                    let replacement = alphabet[rng.below(alphabet.len())];
                    if chars[at] == replacement {
                        continue;
                    }
                    chars[at] = replacement;
                    chars.into_iter().collect()
                }
                // The valid token cut at a random place, or with something appended.
                _ => {
                    let cut = rng.below(valid.len());
                    if rng.below(2) == 0 {
                        valid[..cut].to_owned()
                    } else {
                        format!("{valid}{}", alphabet[rng.below(alphabet.len())])
                    }
                }
            };
            tested += 1;
            let result = verify_for(&candidate, &keys, Some(777_000_001));
            // A change inside the surrounding whitespace is the only harmless one.
            if candidate.trim() != valid {
                assert!(result.is_err(), "round {round} verified: {candidate:?}");
            }
        }
    }
}
