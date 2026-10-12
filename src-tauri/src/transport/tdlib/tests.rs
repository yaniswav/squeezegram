//! Harness: `TdlibJson` over `Replay` fixtures reproducing TDLib's startup pitfalls, plus
//! one ignored test on the real `tdjson.dll` (offline, never a login).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde_json::{Value, json};

use super::replay::{Fixture, Replay, Step};
use super::*;
use crate::transport::PeerKind;

const API_HASH: &str = "0123456789abcdef0123456789abcdef";
const WAIT: Duration = Duration::from_secs(3);

pub(super) const START: &str = include_str!("fixtures/start_wait_phone.json");
pub(super) const CLOSE: &str = include_str!("fixtures/close.json");
const BAD_KEY: &str = include_str!("fixtures/bad_key.json");
const LOGIN_CODE: &str = include_str!("fixtures/login_code_email_2fa.json");
const LOGIN_QR: &str = include_str!("fixtures/login_qr_logout.json");

/// A folder under the system temp directory, removed on drop.
pub(crate) struct TempDir(PathBuf);

impl TempDir {
    pub(crate) fn new(tag: &str) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "sg-tdlib-{tag}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    pub(crate) fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

pub(super) fn fixture(parts: &[&str]) -> Fixture {
    let mut all = Fixture::default();
    for part in parts {
        let fixture: Fixture = serde_json::from_str(part).unwrap();
        all.steps.extend(fixture.steps);
        all.execute.extend(fixture.execute);
    }
    all
}

pub(super) fn steps(value: Value) -> Vec<Step> {
    serde_json::from_value(value).unwrap()
}

pub(super) fn config(dir: &Path) -> TdlibConfig {
    let mut config = TdlibConfig::new(
        dir.to_owned(),
        12345,
        API_HASH.to_owned(),
        "Test PC".to_owned(),
    );
    config.network_disabled = true;
    config.request_timeout = Duration::from_secs(2);
    config.start_timeout = Duration::from_secs(2);
    config.close_timeout = Duration::from_secs(1);
    // Fixtures without `updateChatFolders` only wait this long for it.
    config.folders_wait = Duration::from_millis(60);
    config
}

pub(super) fn transport(parts: &[&str], dir: &TempDir) -> TdlibJson<Replay> {
    TdlibJson::new(Replay::new(fixture(parts)), config(dir.path()))
}

/// An `updateAuthorizationState` as TDLib would emit it for client 1.
fn update(state: Value) -> String {
    json!({ "@type": "updateAuthorizationState", "authorization_state": state, "@client_id": 1 })
        .to_string()
}

async fn wait_state(
    transport: &TdlibJson<Replay>,
    what: &str,
    check: impl FnMut(&AuthState) -> bool,
) -> AuthState {
    let mut states = transport.auth_state();
    match tokio::time::timeout(WAIT, states.wait_for(check)).await {
        Ok(Ok(state)) => state.clone(),
        Ok(Err(_)) => panic!("state channel closed waiting for {what}"),
        Err(_) => panic!(
            "timed out waiting for {what}: {:?}",
            *transport.auth_state().borrow()
        ),
    }
}

fn position(calls: &[String], call: &str) -> usize {
    calls
        .iter()
        .position(|c| c == call)
        .unwrap_or_else(|| panic!("{call} not in {calls:?}"))
}

#[tokio::test]
async fn starts_offline_to_wait_phone_with_the_spec_parameters() {
    let dir = TempDir::new("start");
    let transport = transport(&[START, CLOSE], &dir);
    transport.start().await.unwrap();
    wait_state(&transport, "WaitPhone", |s| {
        matches!(s, AuthState::WaitPhone)
    })
    .await;
    assert_eq!(transport.tdlib_version().as_deref(), Some("1.8.68"));

    let replay = transport.bindings();
    let calls = replay.calls();
    // Verbosity, then the log file, both before the client exists.
    assert_eq!(calls[0], "execute:setLogVerbosityLevel");
    assert_eq!(calls[1], "execute:setLogStream");
    assert_eq!(calls[2], "create");
    // The network is off before TDLib may use it.
    assert!(position(&calls, "send:setNetworkType") < position(&calls, "send:setTdlibParameters"));

    let executed = replay.executed();
    assert_eq!(executed[0]["new_verbosity_level"], 1);
    let stream = &executed[1]["log_stream"];
    assert_eq!(stream["@type"], "logStreamFile");
    assert_eq!(stream["max_file_size"], 5 * 1024 * 1024);
    assert_eq!(stream["redirect_stderr"], false);
    assert_eq!(
        Path::new(stream["path"].as_str().unwrap()),
        dir.path().join(LOG_FILE)
    );

    let parameters = &replay.sent_of("setTdlibParameters")[0];
    assert_eq!(parameters["use_file_database"], true);
    assert_eq!(parameters["use_chat_info_database"], true);
    assert_eq!(parameters["use_message_database"], false);
    assert_eq!(parameters["api_id"], 12345);
    assert_eq!(parameters["api_hash"], API_HASH);
    assert_eq!(parameters["device_model"], "Test PC");
    assert_eq!(
        Path::new(parameters["database_directory"].as_str().unwrap()),
        dir.path().join("db")
    );
    assert_eq!(
        Path::new(parameters["files_directory"].as_str().unwrap()),
        dir.path().join("files")
    );
    let sent_key = STANDARD
        .decode(parameters["database_encryption_key"].as_str().unwrap())
        .unwrap();
    assert_eq!(sent_key.len(), 32);
    let stored = DatabaseKey::load_or_create(dir.path()).unwrap();
    assert_eq!(sent_key, stored.bytes());
    assert_eq!(
        replay.sent_of("setNetworkType")[0]["type"]["@type"],
        "networkTypeNone"
    );

    // Debug shows neither the api_hash nor the key.
    let debug = format!("{transport:?} {:#?}", config(dir.path()));
    assert!(!debug.contains(API_HASH), "{debug}");
    assert!(!debug.contains(&stored.base64()), "{debug}");

    // A second start is a no-op.
    transport.start().await.unwrap();
    assert_eq!(replay.calls().iter().filter(|c| *c == "create").count(), 1);

    transport.shutdown().await;
    assert!(matches!(
        *transport.auth_state().borrow(),
        AuthState::Closed
    ));
    assert!(lock(&transport.inner.receiver).is_none());
    assert_eq!(transport.inner.current_client(), 0);
    // Shutting down twice sends nothing more.
    transport.shutdown().await;
    assert_eq!(replay.sent_of("close").len(), 1);
    assert!(replay.unexpected().is_empty(), "{:?}", replay.unexpected());
    assert!(replay.remaining().is_empty(), "{:?}", replay.remaining());
}

#[tokio::test]
async fn verbosity_is_clamped_and_device_model_never_empty() {
    let dir = TempDir::new("clamp");
    let mut config = config(dir.path());
    config.log_verbosity = 5;
    config.device_model = "   ".into();
    let transport = TdlibJson::new(Replay::new(fixture(&[START, CLOSE])), config);
    transport.start().await.unwrap();
    let replay = transport.bindings();
    assert_eq!(replay.executed()[0]["new_verbosity_level"], 2);
    assert_eq!(
        replay.sent_of("setTdlibParameters")[0]["device_model"],
        "squeezegram"
    );
    transport.shutdown().await;
}

#[tokio::test]
async fn wrong_database_key_is_bad_database_key_and_closes_the_client() {
    let dir = TempDir::new("badkey");
    let transport = transport(&[BAD_KEY], &dir);
    let started = Instant::now();
    let error = transport.start().await.unwrap_err();
    assert!(matches!(error, TransportError::BadDatabaseKey), "{error}");
    assert_eq!(error.code_and_params().0, "TG_BAD_DB_KEY");
    // `setNetworkType` was queued before the parameters and never answered: nobody waits
    // for it forever.
    assert!(started.elapsed() < Duration::from_secs(2));
    let replay = transport.bindings();
    assert_eq!(replay.sent_of("close").len(), 1);
    assert!(matches!(
        *transport.auth_state().borrow(),
        AuthState::StartFailed(TransportError::BadDatabaseKey)
    ));
    assert_eq!(transport.inner.current_client(), 0);
    // `setNetworkType` and `disableProxy`, queued before the parameters, never answered.
    assert!(lock(&transport.inner.pending).len() <= 2);
    assert!(replay.unexpected().is_empty(), "{:?}", replay.unexpected());
    transport.shutdown().await;
    assert!(lock(&transport.inner.pending).is_empty());
}

#[tokio::test]
async fn other_401_is_not_authorized() {
    let dir = TempDir::new("401");
    let transport = transport(&[START, CLOSE], &dir);
    transport.start().await.unwrap();
    transport.bindings().push_steps(steps(json!([{
        "expect": "getMe",
        "reply": { "@type": "error", "code": 401, "message": "Unauthorized" }
    }])));
    assert!(matches!(
        transport.me().await,
        Err(TransportError::NotAuthorized)
    ));
    transport.shutdown().await;
}

#[tokio::test]
async fn a_request_never_answered_times_out_and_closes() {
    let dir = TempDir::new("noanswer");
    let mut config = config(dir.path());
    config.start_timeout = Duration::from_millis(300);
    config.close_timeout = Duration::from_millis(300);
    let replay = Replay::new(fixture(&[r#"{"steps":[{"expect":"getOption"}]}"#]));
    let transport = TdlibJson::new(replay, config);
    let started = Instant::now();
    let error = transport.start().await.unwrap_err();
    assert!(
        matches!(error, TransportError::Timeout("getOption")),
        "{error}"
    );
    assert_eq!(error.code_and_params().0, "TG_TIMEOUT");
    // Start timeout plus the bounded close (not answered either).
    assert!(started.elapsed() < Duration::from_millis(1500));
    assert_eq!(transport.bindings().unexpected(), ["close"]);
    assert!(lock(&transport.inner.pending).is_empty());
    assert!(matches!(
        *transport.auth_state().borrow(),
        AuthState::StartFailed(TransportError::Timeout("getOption"))
    ));
    transport.shutdown().await;
}

#[tokio::test]
async fn a_login_step_never_answered_times_out() {
    let dir = TempDir::new("authtimeout");
    let mut config = config(dir.path());
    config.request_timeout = Duration::from_millis(300);
    let transport = TdlibJson::new(Replay::new(fixture(&[START, CLOSE])), config);
    transport.start().await.unwrap();
    transport
        .bindings()
        .push_steps(steps(json!([{ "expect": "checkAuthenticationCode" }])));
    let error = transport
        .auth_submit(AuthInput::Code("00000".into()))
        .await
        .unwrap_err();
    assert!(
        matches!(error, TransportError::Timeout("checkAuthenticationCode")),
        "{error}"
    );
    assert!(lock(&transport.inner.pending).is_empty());
    transport.shutdown().await;
}

#[tokio::test]
async fn set_tdlib_parameters_failures() {
    // Refused (e.g. invalid api_id): Telegram's code, not a key problem.
    let dir = TempDir::new("params400");
    let refused = fixture(&[
        r#"{"steps":[
          {"expect":"getOption","reply":{"@type":"optionValueString","value":"1.8.68"},
           "after":[{"@type":"updateAuthorizationState","authorization_state":{"@type":"authorizationStateWaitTdlibParameters"}}]},
          {"expect":"setNetworkType"},
          {"expect":"setTdlibParameters","reply":{"@type":"error","code":400,"message":"Valid api_id must be provided"}}
        ]}"#,
        CLOSE,
    ]);
    let transport = TdlibJson::new(Replay::new(refused), config(dir.path()));
    let error = transport.start().await.unwrap_err();
    assert!(
        matches!(&error, TransportError::Other { code: 400, message } if message.contains("api_id")),
        "{error}"
    );
    assert!(matches!(
        *transport.auth_state().borrow(),
        AuthState::StartFailed(TransportError::Other { code: 400, .. })
    ));

    // Never answered: bounded by the start timeout.
    let dir = TempDir::new("paramsnone");
    let mut config = config(dir.path());
    config.start_timeout = Duration::from_millis(300);
    let silent = fixture(&[
        r#"{"steps":[
          {"expect":"getOption","reply":{"@type":"optionValueString","value":"1.8.68"},
           "after":[{"@type":"updateAuthorizationState","authorization_state":{"@type":"authorizationStateWaitTdlibParameters"}}]},
          {"expect":"setNetworkType"},
          {"expect":"setTdlibParameters"}
        ]}"#,
        CLOSE,
    ]);
    let transport = TdlibJson::new(Replay::new(silent), config);
    let error = transport.start().await.unwrap_err();
    assert!(
        matches!(error, TransportError::Timeout("setTdlibParameters")),
        "{error}"
    );
}

#[tokio::test]
async fn phone_email_code_and_password_flow() {
    let dir = TempDir::new("loginflow");
    let transport = transport(&[START, LOGIN_CODE, CLOSE], &dir);
    transport.start().await.unwrap();
    wait_state(&transport, "WaitPhone", |s| {
        matches!(s, AuthState::WaitPhone)
    })
    .await;

    let flood = transport
        .auth_submit(AuthInput::Phone("+00000000000".into()))
        .await
        .unwrap_err();
    assert!(
        matches!(flood, TransportError::FloodWait { retry_after } if retry_after == Duration::from_secs(3600)),
        "{flood}"
    );
    transport
        .auth_submit(AuthInput::Phone("+00000000000".into()))
        .await
        .unwrap();
    wait_state(&transport, "WaitEmail", |s| {
        matches!(s, AuthState::WaitEmail)
    })
    .await;
    transport
        .auth_submit(AuthInput::Email("nobody@example.invalid".into()))
        .await
        .unwrap();
    wait_state(&transport, "WaitEmailCode", |s| {
        matches!(s, AuthState::WaitEmailCode)
    })
    .await;
    transport
        .auth_submit(AuthInput::EmailCode("111111".into()))
        .await
        .unwrap();
    wait_state(&transport, "WaitCode 60 s", |s| {
        matches!(s, AuthState::WaitCode { resend_in: Some(d) } if *d == Duration::from_secs(60))
    })
    .await;
    transport.auth_submit(AuthInput::ResendCode).await.unwrap();
    wait_state(&transport, "WaitCode no resend", |s| {
        matches!(s, AuthState::WaitCode { resend_in: None })
    })
    .await;
    let wrong = transport
        .auth_submit(AuthInput::Code("00000".into()))
        .await
        .unwrap_err();
    assert!(
        matches!(&wrong, TransportError::Other { code: 400, message } if message == "PHONE_CODE_INVALID"),
        "{wrong}"
    );
    transport
        .auth_submit(AuthInput::Code("22222".into()))
        .await
        .unwrap();
    wait_state(
        &transport,
        "WaitPassword",
        |s| matches!(s, AuthState::WaitPassword { hint } if hint == "the usual"),
    )
    .await;
    transport
        .auth_submit(AuthInput::Password("not-a-real-password".into()))
        .await
        .unwrap();
    let ready = wait_state(&transport, "Ready", |s| matches!(s, AuthState::Ready(_))).await;
    let AuthState::Ready(me) = ready else {
        unreachable!()
    };
    assert_eq!(me.user_id, 777_000_001);
    assert_eq!(me.display_name, "Test Account");
    assert_eq!(me.username.as_deref(), Some("test_account"));
    assert!(me.is_premium);
    assert_eq!(me.caption_max_chars, 4096);
    assert_eq!(me.upload_limit_bytes, 4000 * MIB);

    let replay = transport.bindings();
    let phone = &replay.sent_of("setAuthenticationPhoneNumber")[0];
    assert_eq!(phone["phone_number"], "+00000000000");
    assert_eq!(
        replay.sent_of("checkAuthenticationEmailCode")[0]["code"]["code"],
        "111111"
    );
    assert_eq!(
        replay.sent_of("getOption").last().unwrap()["name"],
        "message_caption_length_max"
    );
    transport.shutdown().await;
    assert!(replay.unexpected().is_empty(), "{:?}", replay.unexpected());
    assert!(replay.remaining().is_empty(), "{:?}", replay.remaining());
}

#[tokio::test]
async fn qr_flow_renewed_link_then_log_out_restarts_the_client() {
    let dir = TempDir::new("qr");
    let transport = transport(&[START, LOGIN_QR, CLOSE], &dir);
    transport.start().await.unwrap();
    wait_state(&transport, "WaitPhone", |s| {
        matches!(s, AuthState::WaitPhone)
    })
    .await;
    transport.auth_submit(AuthInput::RequestQr).await.unwrap();
    wait_state(
        &transport,
        "first link",
        |s| matches!(s, AuthState::WaitQr { link } if link.ends_with("FIRST")),
    )
    .await;
    let replay = transport.bindings();
    replay.inject(update(json!({
        "@type": "authorizationStateWaitOtherDeviceConfirmation",
        "link": "tg://login?token=SECOND"
    })));
    wait_state(
        &transport,
        "renewed link",
        |s| matches!(s, AuthState::WaitQr { link } if link.ends_with("SECOND")),
    )
    .await;
    // The phone scanned the code.
    replay.inject(update(json!({ "@type": "authorizationStateReady" })));
    let AuthState::Ready(me) =
        wait_state(&transport, "Ready", |s| matches!(s, AuthState::Ready(_))).await
    else {
        unreachable!()
    };
    assert_eq!(me.display_name, "Qr");
    assert_eq!(me.username, None);
    assert!(!me.is_premium);
    assert_eq!(me.caption_max_chars, 1024);
    assert_eq!(me.upload_limit_bytes, 2000 * MIB);

    transport.log_out().await.unwrap();
    // A fresh client (id 2) is back at the login screen.
    wait_state(&transport, "WaitPhone again", |s| {
        matches!(s, AuthState::WaitPhone)
    })
    .await;
    assert_eq!(transport.inner.current_client(), 2);
    assert_eq!(replay.calls().iter().filter(|c| *c == "create").count(), 2);
    assert_eq!(replay.sent_of("logOut").len(), 1);
    transport.shutdown().await;
    assert!(replay.unexpected().is_empty(), "{:?}", replay.unexpected());
    assert!(replay.remaining().is_empty(), "{:?}", replay.remaining());
}

#[tokio::test]
async fn unexpected_login_steps_are_other_step() {
    let dir = TempDir::new("otherstep");
    let transport = transport(&[START, CLOSE], &dir);
    transport.start().await.unwrap();
    let replay = transport.bindings();
    for name in [
        "authorizationStateWaitRegistration",
        "authorizationStateWaitPremiumPurchase",
        "authorizationStateWaitSomethingFromTheFuture",
    ] {
        replay.inject(update(json!({ "@type": name })));
        wait_state(
            &transport,
            name,
            |s| matches!(s, AuthState::OtherStep { name: n } if n == name),
        )
        .await;
    }
    transport.shutdown().await;
}

#[tokio::test]
async fn ready_without_me_is_a_generic_step_and_never_overrides_a_newer_state() {
    let dir = TempDir::new("readyme");
    let transport = transport(&[START, CLOSE], &dir);
    transport.start().await.unwrap();
    let replay = transport.bindings();

    replay.push_steps(steps(json!([{
        "expect": "getMe",
        "reply": { "@type": "error", "code": 500, "message": "Request aborted" }
    }])));
    replay.inject(update(json!({ "@type": "authorizationStateReady" })));
    wait_state(
        &transport,
        "me_unavailable",
        |s| matches!(s, AuthState::OtherStep { name } if name == ME_UNAVAILABLE),
    )
    .await;

    // `getMe` answers late; meanwhile TDLib already moved on: `Ready` must not come back.
    replay.push_steps(steps(json!([
        { "expect": "getMe", "delay_ms": 300, "reply": {
            "@type": "user", "id": 1, "first_name": "Late", "is_premium": false } },
        { "expect": "getOption", "reply": { "@type": "optionValueInteger", "value": "1024" } }
    ])));
    replay.inject(update(json!({ "@type": "authorizationStateReady" })));
    tokio::time::sleep(Duration::from_millis(50)).await;
    replay.inject(update(json!({ "@type": "authorizationStateLoggingOut" })));
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert!(
        matches!(*transport.auth_state().borrow(), AuthState::LoggingOut),
        "{:?}",
        *transport.auth_state().borrow()
    );
    assert_eq!(replay.sent_of("getOption").len(), 2);
    transport.shutdown().await;
}

#[tokio::test]
async fn malformed_tdlib_json_never_panics() {
    let dir = TempDir::new("malformed");
    let transport = transport(&[START, CLOSE], &dir);
    transport.start().await.unwrap();
    wait_state(&transport, "WaitPhone", |s| {
        matches!(s, AuthState::WaitPhone)
    })
    .await;
    let replay = transport.bindings();
    replay.inject("this is not JSON");
    replay.inject(r#"{"@type":"updateAuthorizationState"}"#);
    replay.inject(update(json!({ "@type": "authorizationStateWaitPassword" })));
    replay.inject(r#"{"@extra":"not a number","@type":"ok"}"#);
    replay.inject(r#"[1,2,3]"#);
    // Answers without `@type`, a user without id, an option of the wrong kind.
    replay.push_steps(steps(json!([
        { "expect": "getMe", "reply": { "no_type": true } },
        { "expect": "getMe", "reply": { "@type": "user", "first_name": "x", "is_premium": false } },
        { "expect": "getMe", "reply": { "@type": "user", "id": 5, "first_name": "x", "is_premium": false } },
        { "expect": "getOption", "reply": { "@type": "optionValueEmpty" } },
    ])));
    for _ in 0..3 {
        let error = transport.me().await.unwrap_err();
        assert!(matches!(error, TransportError::Protocol(_)), "{error}");
    }
    // The receive loop is still alive and the state did not move.
    assert!(matches!(
        *transport.auth_state().borrow(),
        AuthState::WaitPhone
    ));
    assert!(
        lock(&transport.inner.receiver)
            .as_ref()
            .is_some_and(|r| !r.handle.is_finished())
    );
    transport.shutdown().await;
    assert!(matches!(
        *transport.auth_state().borrow(),
        AuthState::Closed
    ));
}

#[tokio::test]
async fn chat_is_made_known_before_any_operation() {
    let dir = TempDir::new("chat");
    let transport = transport(&[START, CLOSE], &dir);
    transport.start().await.unwrap();
    let replay = transport.bindings();
    replay.push_steps(steps(json!([
        { "expect": "createPrivateChat", "reply": { "@type": "chat", "id": 7 } },
        { "expect": "createBasicGroupChat", "reply": { "@type": "chat", "id": -42 } },
        { "expect": "createSupergroupChat", "reply": { "@type": "chat", "id": -1_001_234_567_890i64 } },
        { "expect": "createSupergroupChat", "reply": { "@type": "chat", "id": -1_009_999_999_999i64 } },
        // Unknown to TDLib: loadChats of the main list until 404, one more try; then the
        // archive and the folder (an archived chat), a last try, still refused.
        { "expect": "createSupergroupChat", "reply": { "@type": "error", "code": 400, "message": "Chat not found" } },
        { "expect": "loadChats", "reply": { "@type": "ok" } },
        { "expect": "loadChats", "reply": { "@type": "error", "code": 404, "message": "Not Found" } },
        { "expect": "createSupergroupChat", "reply": { "@type": "error", "code": 400, "message": "Chat not found" } },
        { "expect": "loadChats", "reply": { "@type": "error", "code": 404, "message": "Not Found" } },
        { "expect": "loadChats", "reply": { "@type": "error", "code": 400, "message": "Chat list not found" } },
        { "expect": "createSupergroupChat", "reply": { "@type": "error", "code": 400, "message": "Chat not found" } },
        { "expect": "createPrivateChat", "reply": { "@type": "ok" } },
    ])));
    // One chat folder (whose list TDLib no longer knows: its 400 is skipped).
    replay.inject(
        json!({ "@type": "updateChatFolders", "@client_id": 1, "main_chat_list_position": 0,
            "are_tags_enabled": false, "chat_folders": [{ "@type": "chatFolderInfo", "id": 3,
            "name": { "@type": "chatFolderName", "animate_custom_emoji": false,
                "text": { "@type": "formattedText", "text": "Work", "entities": [] } },
            "icon": { "@type": "chatFolderIcon", "name": "Work" }, "color_id": -1,
            "is_shareable": false, "has_my_invite_links": false }] })
        .to_string(),
    );
    tokio::time::sleep(Duration::from_millis(100)).await;
    let user = Peer {
        kind: PeerKind::User,
        raw_id: 7,
    };
    let group = Peer {
        kind: PeerKind::BasicGroup,
        raw_id: 42,
    };
    let channel = Peer {
        kind: PeerKind::Channel,
        raw_id: 1_234_567_890,
    };
    assert_eq!(transport.ensure_chat_known(&user).await.unwrap(), 7);
    assert_eq!(transport.ensure_chat_known(&group).await.unwrap(), -42);
    assert_eq!(
        transport.ensure_chat_known(&channel).await.unwrap(),
        -1_001_234_567_890
    );
    // TDLib answering another chat is a protocol error, never a silent wrong target.
    assert!(matches!(
        transport.ensure_chat_known(&channel).await,
        Err(TransportError::Protocol(_))
    ));
    assert!(matches!(
        transport.ensure_chat_known(&channel).await,
        Err(TransportError::Other { code: 400, .. })
    ));
    assert!(matches!(
        transport.ensure_chat_known(&user).await,
        Err(TransportError::Protocol(_))
    ));
    assert_eq!(replay.sent_of("createPrivateChat")[0]["user_id"], 7);
    assert_eq!(replay.sent_of("createPrivateChat")[0]["force"], false);
    assert_eq!(
        replay.sent_of("createBasicGroupChat")[0]["basic_group_id"],
        42
    );
    assert_eq!(
        replay.sent_of("createSupergroupChat")[0]["supergroup_id"],
        1_234_567_890
    );
    let loads = replay.sent_of("loadChats");
    let lists: Vec<&Value> = loads.iter().map(|l| &l["chat_list"]).collect();
    assert_eq!(
        lists,
        [
            &json!({ "@type": "chatListMain" }),
            &json!({ "@type": "chatListMain" }),
            &json!({ "@type": "chatListArchive" }),
            &json!({ "@type": "chatListFolder", "chat_folder_id": 3 }),
        ]
    );
    transport.shutdown().await;
    assert!(replay.unexpected().is_empty(), "{:?}", replay.unexpected());
    assert!(replay.remaining().is_empty(), "{:?}", replay.remaining());
}

#[tokio::test]
async fn nothing_works_before_start() {
    let dir = TempDir::new("notstarted");
    let transport = transport(&[], &dir);
    assert!(matches!(
        transport.auth_submit(AuthInput::RequestQr).await,
        Err(TransportError::NotAuthorized)
    ));
    assert!(matches!(
        transport.log_out().await,
        Err(TransportError::NotAuthorized)
    ));
    assert!(matches!(
        transport.me().await,
        Err(TransportError::NotAuthorized)
    ));
    transport.shutdown().await;
    assert!(transport.bindings().calls().is_empty());
}

/// DPAPI refuses `key.bin` (Windows password reset, profile migration, copy to another
/// account): the start fails before any client, says why, and the reset of the current
/// identity's session (here a personal one, beside the product's) gets back to `WaitPhone`.
#[tokio::test]
async fn an_unreadable_key_is_a_failed_start_that_a_session_reset_fixes() {
    let dir = TempDir::new("reset-dpapi");
    let root = dir.path();
    let session = root.join("custom-777");
    std::fs::create_dir_all(session.join("db")).unwrap();
    std::fs::write(session.join("db").join("td.binlog"), b"old database").unwrap();
    std::fs::write(session.join(key::KEY_FILE), b"not a DPAPI blob").unwrap();
    // The product's session and the files that are not part of any session.
    std::fs::create_dir_all(root.join("db")).unwrap();
    for file in [key::KEY_FILE, "api_hash.bin", "proxy_secret.bin"] {
        std::fs::write(root.join(file), b"kept").unwrap();
    }
    let transport = TdlibJson::new(Replay::new(fixture(&[START, CLOSE])), config(&session));

    let error = transport.start().await.unwrap_err();
    assert!(matches!(error, TransportError::BadDatabaseKey), "{error}");
    assert!(matches!(
        *transport.auth_state().borrow(),
        AuthState::StartFailed(TransportError::BadDatabaseKey)
    ));
    assert!(!transport.bindings().calls().contains(&"create".to_owned()));
    // Logging out cannot help: there is no client.
    assert!(matches!(
        transport.log_out().await,
        Err(TransportError::NotAuthorized)
    ));

    transport.reset_session().await.unwrap();
    wait_state(&transport, "WaitPhone", |s| {
        matches!(s, AuthState::WaitPhone)
    })
    .await;
    assert!(!session.join("db").join("td.binlog").exists());
    // A fresh key, readable, is the one TDLib got.
    let sent = &transport.bindings().sent_of("setTdlibParameters")[0];
    let key = DatabaseKey::load_or_create(&session).unwrap();
    assert_eq!(sent["database_encryption_key"], key.base64());
    for file in [key::KEY_FILE, "api_hash.bin", "proxy_secret.bin"] {
        assert_eq!(std::fs::read(root.join(file)).unwrap(), b"kept", "{file}");
    }
    assert!(root.join("db").is_dir());
    transport.shutdown().await;
    let replay = transport.bindings();
    assert!(replay.unexpected().is_empty(), "{:?}", replay.unexpected());
}

/// TDLib refuses the key (`401` on `setTdlibParameters`): the client is closed, then the
/// reset deletes the session and a new client starts with a new key.
#[tokio::test]
async fn a_key_tdlib_refuses_is_fixed_by_a_session_reset() {
    let dir = TempDir::new("reset-401");
    let transport = transport(&[BAD_KEY, START, CLOSE], &dir);
    let first_key = {
        transport.start().await.unwrap_err();
        DatabaseKey::load_or_create(dir.path()).unwrap().base64()
    };
    std::fs::write(dir.path().join("db").join("td.binlog"), b"x").unwrap();
    assert!(matches!(
        *transport.auth_state().borrow(),
        AuthState::StartFailed(TransportError::BadDatabaseKey)
    ));

    transport.reset_session().await.unwrap();
    wait_state(&transport, "WaitPhone", |s| {
        matches!(s, AuthState::WaitPhone)
    })
    .await;
    let replay = transport.bindings();
    assert_eq!(replay.calls().iter().filter(|c| *c == "create").count(), 2);
    let parameters = replay.sent_of("setTdlibParameters");
    assert_eq!(parameters[0]["database_encryption_key"], first_key);
    assert_ne!(parameters[1]["database_encryption_key"], first_key);
    assert!(!dir.path().join("db").join("td.binlog").exists());
    transport.shutdown().await;
    assert!(replay.unexpected().is_empty(), "{:?}", replay.unexpected());
    assert!(replay.remaining().is_empty(), "{:?}", replay.remaining());
}

/// A session that runs (or has not failed) is never deleted.
#[tokio::test]
async fn a_session_in_use_is_never_reset() {
    let dir = TempDir::new("reset-in-use");
    let transport = transport(&[START, CLOSE], &dir);
    // Before any start.
    assert!(matches!(
        transport.reset_session().await,
        Err(TransportError::SessionInUse)
    ));
    transport.start().await.unwrap();
    wait_state(&transport, "WaitPhone", |s| {
        matches!(s, AuthState::WaitPhone)
    })
    .await;
    let error = transport.reset_session().await.unwrap_err();
    assert_eq!(error.code_and_params().0, "TG_SESSION_IN_USE");
    assert!(dir.path().join(key::KEY_FILE).is_file() && dir.path().join("db").is_dir());
    // After a shutdown (`Closed`) neither.
    transport.shutdown().await;
    assert!(matches!(
        transport.reset_session().await,
        Err(TransportError::SessionInUse)
    ));
    assert!(dir.path().join(key::KEY_FILE).is_file());
}

#[tokio::test]
async fn a_missing_dll_fails_start_with_lib_missing_and_writes_nothing() {
    let dir = TempDir::new("nodll");
    let transport = TdlibJson::with_dll(dir.path().join("tdjson.dll"), config(dir.path()));
    let error = transport.start().await.unwrap_err();
    assert!(matches!(error, TransportError::LibMissing(_)), "{error}");
    assert_eq!(error.code_and_params().0, "TG_LIB_MISSING");
    assert!(!dir.path().join(key::KEY_FILE).exists());
    assert!(matches!(
        *transport.auth_state().borrow(),
        AuthState::StartFailed(TransportError::LibMissing(_))
    ));
    transport.shutdown().await;
}

/// The real `tdjson.dll`, offline: api_id 12345 (fake), `networkTypeNone` before the
/// parameters, no phone number, no code, no login. Run with
/// `SQUEEZEGRAM_TEST_TDJSON=<path to tdjson.dll> cargo test --lib real_tdjson -- --ignored`.
#[tokio::test]
#[ignore = "needs a real tdjson.dll (SQUEEZEGRAM_TEST_TDJSON); offline, never logs in"]
async fn real_tdjson_offline_reaches_wait_phone_and_closes() {
    let Some(lib) = std::env::var_os("SQUEEZEGRAM_TEST_TDJSON") else {
        panic!("set SQUEEZEGRAM_TEST_TDJSON to the path of tdjson.dll");
    };
    let dir = TempDir::new("real");
    let mut config = TdlibConfig::new(
        dir.path().to_owned(),
        12345,
        API_HASH.to_owned(),
        "squeezegram test".to_owned(),
    );
    config.network_disabled = true;
    let transport = TdlibJson::with_dll(PathBuf::from(lib), config);
    let started = Instant::now();
    transport.start().await.unwrap();
    let mut states = transport.auth_state();
    tokio::time::timeout(
        Duration::from_secs(10),
        states.wait_for(|s| matches!(s, AuthState::WaitPhone)),
    )
    .await
    .expect("WaitPhone in time")
    .unwrap();
    println!(
        "TDLib {:?}: WaitPhone after {:?}",
        transport.tdlib_version(),
        started.elapsed()
    );
    let closing = Instant::now();
    transport.shutdown().await;
    println!("closed in {:?}", closing.elapsed());
    assert!(matches!(
        *transport.auth_state().borrow(),
        AuthState::Closed
    ));
    // The database is encrypted and the log holds neither the api_hash nor the key.
    let key = DatabaseKey::load_or_create(dir.path()).unwrap().base64();
    if let Ok(db) = std::fs::read(dir.path().join("db").join("db.sqlite")) {
        assert!(!db.starts_with(b"SQLite format 3"));
    }
    let log = std::fs::read_to_string(dir.path().join(LOG_FILE)).unwrap_or_default();
    assert!(!log.contains(API_HASH));
    assert!(!log.contains(&key));
}

/// Crash diagnostics: a numbered `tdlib_ffi` line before every call into `tdjson` made from
/// the caller's thread, naming the request type only (no api_hash, no key).
#[tokio::test]
async fn every_tdjson_call_is_journaled_before_it_runs_without_content() {
    let dir = TempDir::new("ffi-journal");
    let journal = crate::logging::testing::TestJournal::new(&dir.path().join("logs"), "trace");
    let transport = transport(&[START, CLOSE], &dir);
    transport.start().await.unwrap();
    wait_state(&transport, "WaitPhone", |s| {
        matches!(s, AuthState::WaitPhone)
    })
    .await;
    transport.shutdown().await;

    let lines = journal.lines();
    let ffi: Vec<_> = lines
        .iter()
        .filter(|l| l["event"] == "tdlib_ffi")
        .map(|l| &l["fields"])
        .collect();
    let calls: Vec<String> = ffi
        .iter()
        .map(|f| {
            format!(
                "{}:{}",
                f["call"].as_str().unwrap(),
                f["what"].as_str().unwrap()
            )
        })
        .collect();
    let replay: Vec<String> = transport
        .bindings()
        .calls()
        .iter()
        .map(|c| match c.split_once(':') {
            Some(("execute", what)) => format!("td_execute:{what}"),
            Some(("send", what)) => format!("td_send:{what}"),
            _ => "td_create_client_id:".to_owned(),
        })
        .collect();
    // Same calls, same order: one line per call, written by the thread that makes it.
    assert_eq!(calls, replay);
    let numbers: Vec<u64> = ffi.iter().map(|f| f["n"].as_u64().unwrap()).collect();
    assert!(numbers.windows(2).all(|w| w[1] == w[0] + 1), "{numbers:?}");
    assert!(ffi.iter().all(|f| f["os_thread"].as_u64().is_some()));
    let text = journal.text();
    let key = DatabaseKey::load_or_create(dir.path()).unwrap().base64();
    assert!(!text.contains(API_HASH) && !text.contains(&key));
}

/// `SQUEEZEGRAM_TEST_TDJSON`, or a panic naming it.
fn real_dll() -> PathBuf {
    std::env::var_os("SQUEEZEGRAM_TEST_TDJSON")
        .map(PathBuf::from)
        .expect("set SQUEEZEGRAM_TEST_TDJSON to the path of tdjson.dll")
}

fn env_number(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|text| text.trim().parse().ok())
        .unwrap_or(default)
}

/// One app-like run on the real DLL, network on: start, `WaitPhone`, hold, `auth_state` and
/// `me` (refused: nobody is signed in), shutdown, `Closed`.
async fn online_round(lib: &Path, dir: &Path, hold: Duration, round: u64) {
    let config = TdlibConfig::new(
        dir.to_owned(),
        12345,
        API_HASH.to_owned(),
        "squeezegram test".to_owned(),
    );
    let reused = dir.join(key::KEY_FILE).exists();
    let transport = TdlibJson::with_dll(lib.to_owned(), config);
    let started = Instant::now();
    transport.start().await.unwrap();
    let mut states = transport.auth_state();
    tokio::time::timeout(
        Duration::from_secs(20),
        states.wait_for(|s| matches!(s, AuthState::WaitPhone)),
    )
    .await
    .expect("WaitPhone in time")
    .unwrap();
    let wait_phone = started.elapsed();
    // The app's crash came about 70 ms after WaitPhone: poll often at first, then slower.
    let held = Instant::now();
    let mut polls = 0u32;
    while held.elapsed() < hold {
        let pause = if held.elapsed() < Duration::from_secs(1) {
            10
        } else {
            250
        };
        tokio::time::sleep(Duration::from_millis(pause)).await;
        assert!(
            matches!(*transport.auth_state().borrow(), AuthState::WaitPhone),
            "round {round}: state left WaitPhone"
        );
        polls += 1;
    }
    let me = transport.me().await;
    assert!(me.is_err(), "round {round}: nobody is signed in");
    let closing = Instant::now();
    transport.shutdown().await;
    assert!(matches!(
        *transport.auth_state().borrow(),
        AuthState::Closed
    ));
    println!(
        "round {round}: reused_db={reused} WaitPhone after {wait_phone:?}, {polls} polls, me={}, closed in {:?}",
        me.err()
            .map_or("ok".into(), |e| e.code_and_params().0.to_owned()),
        closing.elapsed()
    );
}

/// Repro of the `STATUS_ACCESS_VIOLATION` seen right after `WaitPhone` in the app: the real
/// DLL with the network ON (anonymous connection, fake api_id 12345, no phone, no login),
/// rounds alternating between a fresh folder and one reused database (`key.bin` + `db\`).
/// One extra `DynLib` keeps the DLL loaded for the whole run, as the app does. Rounds and hold
/// time: `SQUEEZEGRAM_TEST_ROUNDS` (30), `SQUEEZEGRAM_TEST_HOLD_SECS` (10). Run with
/// `SQUEEZEGRAM_TEST_TDJSON=<path> cargo test --lib real_tdjson_online -- --ignored --nocapture`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs a real tdjson.dll (SQUEEZEGRAM_TEST_TDJSON); network on, never logs in"]
async fn real_tdjson_online_start_close_loop() {
    let lib = real_dll();
    let keeper = DynLib::new(&lib);
    keeper.create_client_id().unwrap();
    let rounds = env_number("SQUEEZEGRAM_TEST_ROUNDS", 30);
    let hold = Duration::from_secs(env_number("SQUEEZEGRAM_TEST_HOLD_SECS", 10));
    let reused = TempDir::new("online-reused");
    for round in 1..=rounds {
        if round % 2 == 0 {
            online_round(&lib, reused.path(), hold, round).await;
        } else {
            let fresh = TempDir::new("online-fresh");
            online_round(&lib, fresh.path(), hold, round).await;
        }
    }
    let log = std::fs::read_to_string(reused.path().join(LOG_FILE)).unwrap_or_default();
    assert!(!log.contains(API_HASH));
}

/// Same, but each round loads its own `DynLib` and nothing else keeps the DLL: dropping the
/// transport unloads `tdjson.dll` while TDLib's own threads still exist. Run alone (a crash
/// takes the test process down): `cargo test --lib real_tdjson_reload -- --ignored`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs a real tdjson.dll (SQUEEZEGRAM_TEST_TDJSON); network on, never logs in"]
async fn real_tdjson_reload_after_drop() {
    let lib = real_dll();
    let rounds = env_number("SQUEEZEGRAM_TEST_ROUNDS", 3);
    let hold = Duration::from_secs(env_number("SQUEEZEGRAM_TEST_HOLD_SECS", 2));
    let reused = TempDir::new("reload");
    for round in 1..=rounds {
        online_round(&lib, reused.path(), hold, round).await;
        // The receive thread holds the last `Arc<Inner>`; let it drop the DLL.
        std::thread::sleep(RECEIVE_POLL * 3);
    }
}
