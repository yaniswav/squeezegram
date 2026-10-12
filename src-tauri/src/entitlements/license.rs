//! License key format (`docs/LICENSE-KEY.md`): `SQZ1.<base64url(payload)>.<base64url(sig)>`.
//!
//! - base64url without padding, strict: `=` padding, the standard alphabet or non-canonical
//!   trailing bits are malformed.
//! - The signature is Ed25519 (`verify_strict`) over the ASCII bytes `SQZ1.<payload part>` exactly
//!   as received: nothing is decoded and re-encoded before verification.
//! - The payload is JSON: `v` (1), `kid`, `sub`, `plan` (`lifetime`), `feat`, `iat`, `jti`.
//!   Unknown fields and unknown feature names are ignored; `iat` is informational (a date in
//!   the future is accepted: the user's clock is not trusted either way).
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
    #[error("license key belongs to another Telegram account")]
    WrongAccount,
    #[error("no Telegram account is signed in to check the license key against")]
    NoAccount,
}

impl LicenseError {
    pub fn code(self) -> &'static str {
        match self {
            LicenseError::Malformed => "LIC_MALFORMED",
            LicenseError::UnknownKid => "LIC_UNKNOWN_KID",
            LicenseError::BadSignature => "LIC_BAD_SIGNATURE",
            LicenseError::WrongAccount => "LIC_WRONG_ACCOUNT",
            LicenseError::NoAccount => "LIC_NO_ACCOUNT",
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
    /// Known features only, in license order.
    pub features: Vec<Feature>,
    pub iat: i64,
    pub jti: String,
}

impl License {
    pub fn allows(&self, feature: Feature) -> bool {
        self.features.contains(&feature)
    }
}

/// Written by hand: `sub` and `jti` identify a purchase; only the plan and a count show.
impl fmt::Debug for License {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("License")
            .field("plan", &self.plan)
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
    feat: Vec<String>,
    iat: i64,
    jti: String,
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
        features,
        iat: payload.iat,
        jti: payload.jti,
    })
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

/// `verify` then `check_account`.
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
    }
}

#[cfg(test)]
mod tests {
    use super::test_vectors::{self, hex32};
    use super::*;

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
        assert!(vectors.cases.len() >= 12);
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
        assert!(license.allows(Feature::MultiDestination) && !license.allows(Feature::Background));
        assert_eq!(license.kid, "test-1");
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
