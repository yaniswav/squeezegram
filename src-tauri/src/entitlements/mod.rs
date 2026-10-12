//! Entitlements: which paid features may run (open-core, see `docs/LICENSE-KEY.md`).
//!
//! The public source holds no paid feature: the app state carries `FreeOnly`, which refuses
//! every one, so a build made from this repository behaves exactly as the free app. The
//! official build swaps in an implementation backed by a verified license (`store`), with the
//! production public keys supplied from outside this repository.
//!
//! - `license`: the `SQZ1` token format, Ed25519 verification, account binding
//! - `store`: `license.txt` next to `settings.json`, evaluated against the signed-in account

pub mod license;
pub mod store;

use serde::Serialize;

/// A paid feature. Closed on purpose: a license names features by `as_str`, and a name this
/// version does not know is ignored, never mapped to something else.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Feature {
    AdvancedEncoding,
    MultiDestination,
    /// Declared, not wired to anything yet.
    WatchedFolder,
    /// Declared, not wired to anything yet.
    Background,
    /// Declared, not wired to anything yet.
    CaptionTemplates,
}

impl Feature {
    pub const ALL: [Feature; 5] = [
        Feature::AdvancedEncoding,
        Feature::MultiDestination,
        Feature::WatchedFolder,
        Feature::Background,
        Feature::CaptionTemplates,
    ];

    /// Name in a license's `feat` list.
    pub fn as_str(self) -> &'static str {
        match self {
            Feature::AdvancedEncoding => "advanced_encoding",
            Feature::MultiDestination => "multi_destination",
            Feature::WatchedFolder => "watched_folder",
            Feature::Background => "background",
            Feature::CaptionTemplates => "caption_templates",
        }
    }

    /// `None` for a name this version does not know.
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|feature| feature.as_str() == name)
    }
}

/// The plan in effect.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Plan {
    Free,
    Lifetime,
}

impl Plan {
    pub fn as_str(self) -> &'static str {
        match self {
            Plan::Free => "free",
            Plan::Lifetime => "lifetime",
        }
    }
}

/// Why the plan is what it is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EntitlementSource {
    /// This build has no paid features (built from the public source).
    Build,
    /// No license is stored.
    None,
    /// A stored license verifies and belongs to the signed-in account.
    License,
    /// A license is stored but does not apply (malformed, unknown key, bad signature, another
    /// account, or no account signed in to check it against); the journal has the code.
    Rejected,
}

impl EntitlementSource {
    pub fn as_str(self) -> &'static str {
        match self {
            EntitlementSource::Build => "build",
            EntitlementSource::None => "none",
            EntitlementSource::License => "license",
            EntitlementSource::Rejected => "rejected",
        }
    }
}

/// Result of `entitlements_status`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct EntitlementStatus {
    pub plan: Plan,
    pub source: EntitlementSource,
}

impl EntitlementStatus {
    pub const FREE_BUILD: Self = Self {
        plan: Plan::Free,
        source: EntitlementSource::Build,
    };
}

/// What the app asks before running a paid feature. Read at call time: the answer may change
/// with the signed-in account.
pub trait Entitlements: Send + Sync {
    fn allows(&self, feature: Feature) -> bool;
    fn status(&self) -> EntitlementStatus;
}

/// The public build: every paid feature is refused.
#[derive(Clone, Copy, Debug, Default)]
pub struct FreeOnly;

impl Entitlements for FreeOnly {
    fn allows(&self, _feature: Feature) -> bool {
        false
    }

    fn status(&self) -> EntitlementStatus {
        EntitlementStatus::FREE_BUILD
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    #[test]
    fn free_only_refuses_every_feature() {
        let entitlements: Arc<dyn Entitlements> = Arc::new(FreeOnly);
        for feature in Feature::ALL {
            assert!(!entitlements.allows(feature), "{feature:?}");
        }
        assert_eq!(
            serde_json::to_value(entitlements.status()).unwrap(),
            serde_json::json!({ "plan": "free", "source": "build" })
        );
    }

    #[test]
    fn feature_names_round_trip_and_unknown_names_are_none() {
        for feature in Feature::ALL {
            assert_eq!(Feature::from_name(feature.as_str()), Some(feature));
        }
        for unknown in ["", "teleport", "Advanced_Encoding", "advanced_encoding "] {
            assert_eq!(Feature::from_name(unknown), None, "{unknown:?}");
        }
    }
}
