//! Effective upload limit: one value shared by the processing plan and
//! the upload, resolved when an item starts its media work (`media_start`).
//!
//! Order: `limit.custom_mb` when set; else `limit.premium` when forced (4000 or 2000 MiB);
//! else (`null`, automatic) the limit of the signed-in account as the transport read it
//! (`me()` when the session became ready, carried by `AuthState::Ready` for the session and
//! read again at each sign-in); 2000 MiB while no account is ready. The safety margin is
//! applied by the plan, on top of this value.

use std::sync::Arc;

use serde::Serialize;
use tokio::sync::watch;

use crate::media::plan::MIB;
use crate::settings::{Limit, SettingsStore};
use crate::transport::{AuthState, Me};

/// Limit of a standard account.
pub const STANDARD_BYTES: u64 = 2000 * MIB;
/// Limit of a Premium account.
pub const PREMIUM_BYTES: u64 = 4000 * MIB;

/// Where the effective limit comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LimitSource {
    /// `limit.custom_mb`.
    Custom,
    /// `limit.premium` set to true or false.
    Forced,
    /// The signed-in account (`limit.premium` is `null`).
    Account,
    /// Automatic, but no account is ready: the standard limit.
    Default,
}

impl LimitSource {
    pub fn as_str(self) -> &'static str {
        match self {
            LimitSource::Custom => "custom",
            LimitSource::Forced => "forced",
            LimitSource::Account => "account",
            LimitSource::Default => "default",
        }
    }
}

/// Result of `limit_status`, and what the plan and the upload use.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct EffectiveLimit {
    pub limit_bytes: u64,
    pub source: LimitSource,
    /// Premium as far as known: forced, else the account's; false when unknown.
    pub premium: bool,
}

/// The signed-in account, if the session is ready.
pub fn account(state: &AuthState) -> Option<&Me> {
    match state {
        AuthState::Ready(me) => Some(me),
        _ => None,
    }
}

/// Pure resolution: custom, else forced, else the account, else the standard limit.
pub fn resolve(limit: &Limit, account: Option<&Me>) -> EffectiveLimit {
    let premium = limit
        .premium
        .or(account.map(|me| me.is_premium))
        .unwrap_or(false);
    if let Some(mb) = limit.custom_mb {
        return EffectiveLimit {
            limit_bytes: mb.saturating_mul(MIB),
            source: LimitSource::Custom,
            premium,
        };
    }
    match (limit.premium, account) {
        (Some(forced), _) => EffectiveLimit {
            limit_bytes: if forced {
                PREMIUM_BYTES
            } else {
                STANDARD_BYTES
            },
            source: LimitSource::Forced,
            premium: forced,
        },
        (None, Some(me)) => EffectiveLimit {
            limit_bytes: me.upload_limit_bytes,
            source: LimitSource::Account,
            premium: me.is_premium,
        },
        (None, None) => EffectiveLimit {
            limit_bytes: STANDARD_BYTES,
            source: LimitSource::Default,
            premium: false,
        },
    }
}

/// The settings plus the transport's login state: what `resolve` needs, read at call time.
pub struct LimitResolver {
    settings: Arc<SettingsStore>,
    /// `None`: no transport (tests of the media stage alone): never an account.
    auth: Option<watch::Receiver<AuthState>>,
}

impl LimitResolver {
    pub fn new(settings: Arc<SettingsStore>, auth: Option<watch::Receiver<AuthState>>) -> Self {
        Self { settings, auth }
    }

    /// The limit that applies now (no journal line: `limit_status` and the upload fallback).
    pub fn current(&self) -> EffectiveLimit {
        let settings = self.settings.get();
        match &self.auth {
            Some(auth) => resolve(&settings.limit, account(&auth.borrow())),
            None => resolve(&settings.limit, None),
        }
    }

    /// `current`, with its `limit_resolved` line: source, bytes, Premium. Never the account's
    /// identity.
    pub fn resolve_logged(&self) -> EffectiveLimit {
        let limit = self.current();
        tracing::info!(
            event = "limit_resolved",
            source = limit.source.as_str(),
            limit_bytes = limit.limit_bytes,
            premium = limit.premium,
            "upload limit resolved"
        );
        limit
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use serde_json::json;

    use super::*;
    use crate::logging::testing::TestJournal;
    use crate::media::plan::{self, Kind, PlanInput, ResolutionCap};
    use crate::media::probe::MediaInfo;
    use crate::queue::store::SendMode;
    use crate::queue::test_util::TestDir;
    use crate::transport::mock::{MOCK_ME_ID, MockTransport};
    use crate::transport::{NoTransport, TelegramTransport};

    fn settings(dir: &TestDir, patch: serde_json::Value) -> Arc<SettingsStore> {
        let store =
            Arc::new(SettingsStore::load(dir.path().join(crate::settings::FILE_NAME)).unwrap());
        store.set(&patch).unwrap();
        store
    }

    fn resolver(
        dir: &TestDir,
        patch: serde_json::Value,
        transport: &dyn TelegramTransport,
    ) -> LimitResolver {
        LimitResolver::new(settings(dir, patch), Some(transport.auth_state()))
    }

    fn limit(premium: Option<bool>, custom_mb: Option<u64>) -> Limit {
        Limit {
            premium,
            custom_mb,
            ..Limit::default()
        }
    }

    #[test]
    fn custom_wins_then_forced_then_the_account() {
        let premium = MockTransport::authorized().with_premium();
        let standard = MockTransport::authorized();
        let me = |mock: &MockTransport| match &*mock.auth_state().borrow() {
            AuthState::Ready(me) => me.clone(),
            _ => panic!("signed in"),
        };
        let (premium, standard) = (me(&premium), me(&standard));

        // Custom: first, whatever is forced or detected.
        for (forced, account) in [
            (None, Some(&premium)),
            (Some(true), Some(&standard)),
            (Some(false), None),
        ] {
            let resolved = resolve(&limit(forced, Some(50)), account);
            assert_eq!(
                (resolved.limit_bytes, resolved.source),
                (50 * MIB, LimitSource::Custom)
            );
        }
        // Forced: the account does not matter.
        assert_eq!(
            resolve(&limit(Some(true), None), Some(&standard)),
            EffectiveLimit {
                limit_bytes: PREMIUM_BYTES,
                source: LimitSource::Forced,
                premium: true
            }
        );
        assert_eq!(
            resolve(&limit(Some(false), None), Some(&premium)),
            EffectiveLimit {
                limit_bytes: STANDARD_BYTES,
                source: LimitSource::Forced,
                premium: false
            }
        );
        // Automatic: the account's limit.
        assert_eq!(
            resolve(&limit(None, None), Some(&premium)),
            EffectiveLimit {
                limit_bytes: PREMIUM_BYTES,
                source: LimitSource::Account,
                premium: true
            }
        );
        assert_eq!(
            resolve(&limit(None, None), None),
            EffectiveLimit {
                limit_bytes: STANDARD_BYTES,
                source: LimitSource::Default,
                premium: false
            }
        );
    }

    #[tokio::test]
    async fn automatic_follows_the_transport_session() {
        let dir = TestDir::new("limit-auto");
        let auto = json!({});

        let premium = MockTransport::authorized().with_premium();
        let resolved = resolver(&dir, auto.clone(), &premium).current();
        assert_eq!(
            (resolved.limit_bytes, resolved.source, resolved.premium),
            (4000 * MIB, LimitSource::Account, true)
        );
        assert_eq!(resolved.limit_bytes, 4_194_304_000);

        let standard = MockTransport::authorized();
        let resolved = resolver(&dir, auto.clone(), &standard).current();
        assert_eq!(
            (resolved.limit_bytes, resolved.source, resolved.premium),
            (2000 * MIB, LimitSource::Account, false)
        );

        // Not signed in yet, signed out, no Telegram at all, no transport: 2000 MiB.
        let signed_out = MockTransport::new().with_premium();
        signed_out.start().await.unwrap();
        let none = NoTransport::new();
        for transport in [&signed_out as &dyn TelegramTransport, &none] {
            let resolved = resolver(&dir, auto.clone(), transport).current();
            assert_eq!(
                (resolved.limit_bytes, resolved.source),
                (STANDARD_BYTES, LimitSource::Default)
            );
        }
        let detached = LimitResolver::new(settings(&dir, auto.clone()), None).current();
        assert_eq!(detached.source, LimitSource::Default);

        // The session is read at call time: a sign-in, then a sign-out, are followed.
        let later = MockTransport::new().with_premium();
        let resolver = resolver(&dir, auto, &later);
        assert_eq!(resolver.current().source, LimitSource::Default);
        later.start().await.unwrap();
        later
            .auth_submit(crate::transport::AuthInput::Phone("+33600000000".into()))
            .await
            .unwrap();
        later
            .auth_submit(crate::transport::AuthInput::Code(
                crate::transport::mock::MOCK_CODE.into(),
            ))
            .await
            .unwrap();
        assert_eq!(resolver.current().limit_bytes, PREMIUM_BYTES);
        later.log_out().await.unwrap();
        assert_eq!(resolver.current().limit_bytes, STANDARD_BYTES);
    }

    #[test]
    fn forced_and_custom_settings_reach_the_resolver() {
        let dir = TestDir::new("limit-settings");
        let premium = MockTransport::authorized().with_premium();
        let forced = resolver(&dir, json!({ "limit": { "premium": false } }), &premium);
        assert_eq!(
            (forced.current().limit_bytes, forced.current().source),
            (STANDARD_BYTES, LimitSource::Forced)
        );
        let custom = resolver(
            &dir,
            json!({ "limit": { "premium": true, "custom_mb": 1500 } }),
            &premium,
        );
        assert_eq!(
            (custom.current().limit_bytes, custom.current().source),
            (1500 * MIB, LimitSource::Custom)
        );
    }

    /// A conforming 3.04 GiB H.264/AAC file: under the Premium limit it is never encoded.
    #[test]
    fn a_3_04_gib_file_is_not_encoded_for_a_premium_account() {
        let info: MediaInfo = serde_json::from_value(json!({
            "format_name": "matroska,webm", "duration_s": 1800.0, "bit_rate": null,
            "video": { "index": 0, "codec": "h264", "profile": "High", "width": 1920,
                "height": 1080, "sar": [1, 1], "pix_fmt": "yuv420p",
                "field_order": "progressive", "color_transfer": null, "rotation": 0,
                "bit_rate": null, "hdr": false },
            "audio": { "index": 1, "codec": "aac", "channels": 2, "bit_rate": null }
        }))
        .unwrap();
        let size = (3.04 * (1u64 << 30) as f64) as u64;
        assert_eq!(size, 3_264_175_144);
        let plan_for = |account: Option<&Me>| {
            let effective = resolve(&Limit::default(), account);
            let input = PlanInput {
                source: Path::new("D:\\clips\\long.mkv"),
                source_size: size,
                send_mode: SendMode::Video,
                limit_bytes: effective.limit_bytes,
                margin: 0.03,
                audio_kbps: 128,
                cap: ResolutionCap::P1080,
                moov_first: None,
            };
            plan::build_plan(Some(&info), &input).unwrap()
        };
        let premium = match &*MockTransport::authorized()
            .with_premium()
            .auth_state()
            .borrow()
        {
            AuthState::Ready(me) => me.clone(),
            _ => unreachable!(),
        };
        let plan = plan_for(Some(&premium));
        assert_eq!(plan.kind, Kind::Remux, "remuxed into MP4, never encoded");
        assert_eq!((plan.limit_bytes, plan.video_bps), (PREMIUM_BYTES, None));
        // The same file with no account ready: over 2000 MiB, encoded.
        assert_eq!(plan_for(None).kind, Kind::Encode);
    }

    #[test]
    fn limit_resolved_names_the_source_and_never_the_account() {
        let dir = TestDir::new("limit-journal");
        let journal = TestJournal::new(&dir.path().join("logs"), "info");
        let premium = MockTransport::authorized().with_premium();
        let resolved = resolver(&dir, json!({}), &premium).resolve_logged();
        assert_eq!(resolved.source, LimitSource::Account);
        let lines = journal.lines();
        let line = lines
            .iter()
            .find(|l| l["event"] == "limit_resolved")
            .expect("limit_resolved line");
        assert_eq!(line["fields"]["source"], "account");
        assert_eq!(line["fields"]["limit_bytes"], PREMIUM_BYTES);
        assert_eq!(line["fields"]["premium"], true);
        let text = journal.text();
        for identity in [&MOCK_ME_ID.to_string(), "Mock User", "mock_user"] {
            assert!(!text.contains(identity), "{identity} in the journal");
        }
    }
}
