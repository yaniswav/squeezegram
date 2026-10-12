//! Public keys a build trusts for license keys, by `kid`.
//!
//! They only verify, so they are safe to ship. The throwaway key of `fixtures/vectors.json`
//! never appears here: it exists only in `cfg(test)` code (`license::test_vectors`).

use super::license::KeyRing;

/// `(kid, Ed25519 public key)`. A rotation appends the new key (`k2`, ...) and keeps the older
/// entries for as long as the licenses they signed must keep working.
pub const PRODUCTION_KEYS: [(&str, [u8; 32]); 1] = [(
    "k1",
    hex32("e5cfab9b583eda609d7d66850572e389d847e22fb3d437bc5fa1f3c08d03375e"),
)];

/// Lowercase hex to 32 bytes, at compile time: a mistyped key fails the build.
const fn hex32(text: &str) -> [u8; 32] {
    const fn nibble(c: u8) -> u8 {
        match c {
            b'0'..=b'9' => c - b'0',
            b'a'..=b'f' => c - b'a' + 10,
            _ => panic!("not a lowercase hex digit"),
        }
    }
    let bytes = text.as_bytes();
    assert!(bytes.len() == 64, "a public key is 64 hex digits");
    let mut out = [0u8; 32];
    let mut i = 0;
    while i < 32 {
        out[i] = (nibble(bytes[2 * i]) << 4) | nibble(bytes[2 * i + 1]);
        i += 1;
    }
    out
}

/// The ring of `entries`. An entry that is not a curve point is left out, so its licenses read
/// as `LIC_UNKNOWN_KID`; a test keeps that from happening to a shipped key.
pub fn ring_of(entries: &[(&str, [u8; 32])]) -> KeyRing {
    entries
        .iter()
        .fold(KeyRing::empty(), |ring, (kid, public)| {
            ring.clone().with_key(kid, public).unwrap_or(ring)
        })
}

/// The keys of this build.
pub fn production() -> KeyRing {
    ring_of(&PRODUCTION_KEYS)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entitlements::license::test_vectors::{self, hex32 as test_hex32};
    use crate::entitlements::license::{self, LicenseError};

    #[test]
    fn the_production_ring_holds_k1_and_every_key_is_a_curve_point() {
        let ring = production();
        assert_eq!(format!("{ring:?}"), r#"["k1"]"#);
        assert_eq!(
            PRODUCTION_KEYS[0].1,
            test_hex32("e5cfab9b583eda609d7d66850572e389d847e22fb3d437bc5fa1f3c08d03375e")
        );
        for (kid, public) in PRODUCTION_KEYS {
            assert!(
                KeyRing::empty().with_key(kid, &public).is_ok(),
                "{kid} is not a public key"
            );
        }
    }

    #[test]
    fn the_test_key_is_never_accepted_by_the_production_ring() {
        let vectors = test_vectors::load();
        let test_public = test_hex32(&vectors.test_key.public_hex);
        assert!(
            PRODUCTION_KEYS
                .iter()
                .all(|(kid, public)| *kid != vectors.test_key.kid && *public != test_public)
        );
        let ring = production();
        // Every vector, valid ones included, is refused: the test kid is unknown here.
        for case in &vectors.cases {
            let result = license::verify_for(&case.token, &ring, case.user_id);
            assert!(result.is_err(), "case {} verified", case.name);
        }
        assert_eq!(
            license::verify(&vectors.case("valid").token, &ring),
            Err(LicenseError::UnknownKid)
        );
    }

    /// No token signed by `k1` can be produced here (no private key): the path a production
    /// key takes (`ring_of`) is checked with the test key injected in its place, next to `k1`
    /// as after a rotation.
    #[test]
    fn a_listed_key_verifies_its_tokens_through_ring_of() {
        let vectors = test_vectors::load();
        let test_public = test_hex32(&vectors.test_key.public_hex);
        let entries = [
            PRODUCTION_KEYS[0],
            (vectors.test_key.kid.as_str(), test_public),
        ];
        let ring = ring_of(&entries);
        let valid = &vectors.case("valid");
        let license = license::verify_for(&valid.token, &ring, valid.user_id).unwrap();
        assert_eq!(license.kid, vectors.test_key.kid);
        // Not a point: left out, the other keys still load.
        let mut not_a_point = [0u8; 32];
        not_a_point[0] = 2;
        let ring = ring_of(&[("bad", not_a_point), PRODUCTION_KEYS[0]]);
        assert_eq!(format!("{ring:?}"), r#"["k1"]"#);
    }
}
