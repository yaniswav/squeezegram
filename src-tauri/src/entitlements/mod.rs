//! Entitlements: which paid features may run.
//!
//! The app state carries `LicenseEntitlements` (`store`): the license stored next to
//! `settings.json`, verified offline against the public keys of `keys` and bound to the
//! signed-in Telegram account. `FreeOnly` refuses every feature; tests use it as the free plan.
//!
//! - `license`: the `SQZ1` token format, Ed25519 verification, account binding
//! - `keys`: the production public keys, by `kid`
//! - `store`: `license.txt`, evaluated against the signed-in account

pub mod keys;
pub mod license;
pub mod store;

use serde::Serialize;

pub use store::LicenseEntitlements;

/// Where a license is bought: a Telegram bot. Opened by the core, never chosen by the page.
pub const PURCHASE_URL: &str = "https://t.me/squeezegram_bot";

/// A paid feature. Closed on purpose: a license names features by `as_str`, and a name this
/// version does not know is ignored, never mapped to something else.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Feature {
    /// Maximum resolution and audio bitrate of an encode (`settings.advanced_encoding`).
    AdvancedEncoding,
    /// More than one destination (chats or forum topics) for one `queue_add`: one item per file
    /// and destination, one encode per file. Refused in `commands::add_files` without it.
    /// Only the batch is paid: reusing an encoded file is not gated, so a free user who queues
    /// the same file for two destinations in two adds also gets one encode.
    MultiDestination,
    /// Watched folders (`watch`): new files are queued for approval. Folders stay configured
    /// without it; the watcher lists them only while it is granted.
    WatchedFolder,
    /// Declared, not wired to anything yet.
    Background,
}

impl Feature {
    pub const ALL: [Feature; 4] = [
        Feature::AdvancedEncoding,
        Feature::MultiDestination,
        Feature::WatchedFolder,
        Feature::Background,
    ];

    /// Name in a license's `feat` list (also its serialized form).
    pub fn as_str(self) -> &'static str {
        match self {
            Feature::AdvancedEncoding => "advanced_encoding",
            Feature::MultiDestination => "multi_destination",
            Feature::WatchedFolder => "watched_folder",
            Feature::Background => "background",
        }
    }

    /// Whether the feature does something in this version. The licence screen lists only
    /// these: a licence may name features that are declared but not wired yet, and showing
    /// them as "Unlocked" would promise something that does not exist. `allows` ignores this.
    /// Flip a feature to `true` in the batch that wires it.
    pub fn shipped(self) -> bool {
        match self {
            Feature::AdvancedEncoding | Feature::MultiDestination | Feature::WatchedFolder => true,
            Feature::Background => false,
        }
    }

    /// `None` for a name this version does not know. Caption templates were removed (they
    /// will be free), but licences already issued still list `caption_templates`: it lands
    /// here and is ignored, so those licences keep verifying.
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
    /// `FreeOnly`: no license is ever read.
    Build,
    /// No license is stored.
    None,
    /// A stored license verifies and belongs to the signed-in account.
    License,
    /// A stored license verifies, and no account is signed in to check it against: it applies
    /// once its account signs in.
    Pending,
    /// A license is stored but does not apply (malformed, unknown key, bad signature, or
    /// another account); `reason` has the code.
    Rejected,
}

impl EntitlementSource {
    pub fn as_str(self) -> &'static str {
        match self {
            EntitlementSource::Build => "build",
            EntitlementSource::None => "none",
            EntitlementSource::License => "license",
            EntitlementSource::Pending => "pending",
            EntitlementSource::Rejected => "rejected",
        }
    }
}

/// Result of `entitlements_status` and payload of `entitlements://status`. Carries no
/// identifier: neither the license, nor its `sub`, nor the account.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct EntitlementStatus {
    pub plan: Plan,
    pub source: EntitlementSource,
    /// Features granted now (empty unless `source` is `license`).
    pub features: Vec<Feature>,
    /// Whether the stored license was issued to the signed-in account; `null` when there is no
    /// verified license or no account to compare with.
    pub account_matches: Option<bool>,
    /// The `LIC_*` code that keeps a stored license from applying.
    pub reason: Option<&'static str>,
}

impl EntitlementStatus {
    pub const FREE_BUILD: Self = Self {
        plan: Plan::Free,
        source: EntitlementSource::Build,
        features: Vec::new(),
        account_matches: None,
        reason: None,
    };
}

/// What the app asks before running a paid feature. Read at call time: the answer may change
/// with the signed-in account.
pub trait Entitlements: Send + Sync {
    fn allows(&self, feature: Feature) -> bool;
    fn status(&self) -> EntitlementStatus;
}

/// Every paid feature refused, no license read.
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
    fn advanced_encoding_multi_destination_and_watched_folder_are_shipped_today() {
        let shipped: Vec<_> = Feature::ALL.into_iter().filter(|f| f.shipped()).collect();
        assert_eq!(
            shipped,
            [
                Feature::AdvancedEncoding,
                Feature::MultiDestination,
                Feature::WatchedFolder
            ]
        );
    }

    #[test]
    fn free_only_refuses_every_feature() {
        let entitlements: Arc<dyn Entitlements> = Arc::new(FreeOnly);
        for feature in Feature::ALL {
            assert!(!entitlements.allows(feature), "{feature:?}");
        }
        assert_eq!(
            serde_json::to_value(entitlements.status()).unwrap(),
            serde_json::json!({
                "plan": "free",
                "source": "build",
                "features": [],
                "account_matches": null,
                "reason": null
            })
        );
    }

    #[test]
    fn feature_names_round_trip_and_unknown_names_are_none() {
        for feature in Feature::ALL {
            assert_eq!(Feature::from_name(feature.as_str()), Some(feature));
            assert_eq!(
                serde_json::to_value(feature).unwrap(),
                serde_json::json!(feature.as_str())
            );
        }
        for unknown in ["", "teleport", "Advanced_Encoding", "advanced_encoding "] {
            assert_eq!(Feature::from_name(unknown), None, "{unknown:?}");
        }
    }
}
