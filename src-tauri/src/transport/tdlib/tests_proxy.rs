//! The proxy over `Replay`: queued with the parameters, other proxies removed, changed while
//! running, tested without being applied, and a start refused when its secret is unreadable.

use std::time::Duration;

use serde_json::{Value, json};

use super::replay::Replay;
use super::tests::{CLOSE, TempDir, config, fixture, steps};
use super::*;
use crate::transport::proxy::{Proxy, ProxyKind};

const MT_SECRET: &str = "ee0123456789abcdef0123456789abcdef7777772e6578616d706c652e636f6d";

fn mtproto() -> Proxy {
    Proxy::new(ProxyKind::Mtproto, "mt.example", 443, "", MT_SECRET).unwrap()
}

/// The answer TDLib gives to the `addProxy` of `mtproto()`.
fn added() -> Value {
    json!({ "@type": "addedProxy", "id": 3, "last_used_date": 0, "is_enabled": true,
            "comment": "", "proxy": { "@type": "proxy", "server": "mt.example", "port": 443,
            "type": { "@type": "proxyTypeMtproto", "secret": MT_SECRET } } })
}

/// `start_wait_phone.json` with `addProxy`: TDLib answers the proxy queued before the
/// parameters once they are set (`add_reply`), then lists our proxy (id 3) and two left from
/// before.
fn start_with_proxy(add_reply: Value) -> String {
    json!({ "steps": [
        { "expect": "getOption", "reply": { "@type": "optionValueString", "value": "1.8.68" },
          "after": [{ "@type": "updateAuthorizationState",
                      "authorization_state": { "@type": "authorizationStateWaitTdlibParameters" } }] },
        { "expect": "addProxy", "reply": add_reply },
        { "expect": "setNetworkType", "reply": { "@type": "ok" } },
        { "expect": "setTdlibParameters", "reply": { "@type": "ok" },
          "after": [{ "@type": "updateAuthorizationState",
                      "authorization_state": { "@type": "authorizationStateWaitPhoneNumber" } }] },
        { "expect": "getProxies", "reply": { "@type": "addedProxies", "proxies": [
            { "@type": "addedProxy", "id": 1 }, { "@type": "addedProxy", "id": 3 },
            { "@type": "addedProxy", "id": 2 } ] } },
        { "expect": "removeProxy", "reply": { "@type": "ok" } },
        { "expect": "removeProxy", "reply": { "@type": "ok" } }
    ] })
    .to_string()
}

fn position(calls: &[String], call: &str) -> usize {
    calls
        .iter()
        .position(|c| c == call)
        .unwrap_or_else(|| panic!("{call} not in {calls:?}"))
}

#[tokio::test]
async fn the_proxy_is_queued_before_the_parameters_and_the_others_are_removed() {
    let dir = TempDir::new("proxy-start");
    let mut config = config(dir.path());
    config.proxy = Ok(Some(mtproto()));
    let start = start_with_proxy(added());
    let transport = TdlibJson::new(Replay::new(fixture(&[&start, CLOSE])), config);
    transport.start().await.unwrap();

    let replay = transport.bindings();
    let calls = replay.calls();
    assert!(
        position(&calls, "send:addProxy") < position(&calls, "send:setTdlibParameters"),
        "{calls:?}"
    );
    assert_eq!(
        replay.sent_of("addProxy")[0],
        json!({ "@type": "addProxy", "@extra": replay.sent_of("addProxy")[0]["@extra"],
                "enable": true, "comment": "",
                "proxy": { "@type": "proxy", "server": "mt.example", "port": 443,
                           "type": { "@type": "proxyTypeMtproto", "secret": MT_SECRET } } })
    );
    let removed: Vec<Value> = replay
        .sent_of("removeProxy")
        .iter()
        .map(|request| request["proxy_id"].clone())
        .collect();
    assert_eq!(removed, [json!(1), json!(2)], "ours (3) is kept");
    assert!(replay.sent_of("disableProxy").is_empty());
    assert!(replay.unexpected().is_empty(), "{:?}", replay.unexpected());

    // Debug never shows the secret or the address.
    let debug = format!("{transport:?}");
    assert!(
        !debug.contains(MT_SECRET) && !debug.contains("mt.example"),
        "{debug}"
    );
    transport.shutdown().await;
}

#[tokio::test]
async fn a_change_applies_at_once_and_a_test_leaves_the_proxy_alone() {
    let dir = TempDir::new("proxy-live");
    let transport = TdlibJson::new(
        Replay::new(fixture(&[super::tests::START, CLOSE])),
        config(dir.path()),
    );
    transport.start().await.unwrap();
    let replay = transport.bindings();
    // No proxy configured: the start disables whatever TDLib kept.
    assert_eq!(replay.sent_of("disableProxy").len(), 1);

    replay.push_steps(steps(json!([
        { "expect": "addProxy", "reply": { "@type": "addedProxy", "id": "8" } },
        { "expect": "getProxies", "reply": { "@type": "addedProxies",
          "proxies": [{ "@type": "addedProxy", "id": 8 }] } },
        { "expect": "pingProxy", "reply": { "@type": "seconds", "seconds": 0.25 } },
        { "expect": "pingProxy", "reply": { "@type": "error", "code": 400,
          "message": "Failed to connect to 10.9.9.9:1080" } },
        { "expect": "disableProxy", "reply": { "@type": "ok" } },
        { "expect": "getProxies", "reply": { "@type": "addedProxies",
          "proxies": [{ "@type": "addedProxy", "id": 8 }] } },
        { "expect": "removeProxy", "reply": { "@type": "ok" } }
    ])));
    let socks = Proxy::new(ProxyKind::Socks5, "10.9.9.9", 1080, "user", "pa55").unwrap();
    transport.set_proxy(Some(socks.clone())).await.unwrap();
    assert_eq!(
        replay.sent_of("addProxy")[0]["proxy"]["type"],
        json!({ "@type": "proxyTypeSocks5", "username": "user", "password": "pa55" })
    );
    assert!(
        replay.sent_of("removeProxy").is_empty(),
        "the new proxy is the only one"
    );

    let latency = transport.test_proxy(&mtproto()).await.unwrap();
    assert_eq!(latency, Duration::from_millis(250));
    let error = transport.test_proxy(&socks).await.unwrap_err();
    assert_eq!(
        error.code_and_params(),
        (
            "TG_PROXY_FAILED",
            json!({ "message": "Failed to connect to 10.9.9.9:1080" })
        )
    );
    assert_eq!(
        replay.sent_of("addProxy").len(),
        1,
        "a test never adds or enables a proxy"
    );

    transport.set_proxy(None).await.unwrap();
    assert_eq!(replay.sent_of("disableProxy").len(), 2);
    assert_eq!(replay.sent_of("removeProxy")[0]["proxy_id"], 8);
    assert!(replay.unexpected().is_empty(), "{:?}", replay.unexpected());
    assert_eq!(replay.remaining(), ["close"], "every pushed step was used");
    transport.shutdown().await;
}

#[tokio::test]
async fn a_refused_proxy_fails_the_start() {
    let dir = TempDir::new("proxy-refused");
    let mut config = config(dir.path());
    config.proxy = Ok(Some(mtproto()));
    let start =
        start_with_proxy(json!({ "@type": "error", "code": 400, "message": "Wrong proxy secret" }));
    let transport = TdlibJson::new(Replay::new(fixture(&[&start, CLOSE])), config);
    let error = transport.start().await.unwrap_err();
    assert_eq!(
        error.code_and_params(),
        (
            "TG_PROXY_FAILED",
            json!({ "message": "Wrong proxy secret" })
        )
    );
    assert!(matches!(
        *transport.auth_state().borrow(),
        AuthState::Closed
    ));
    transport.shutdown().await;
}

#[tokio::test]
async fn an_unreadable_secret_blocks_every_start_until_the_proxy_is_set_again() {
    let dir = TempDir::new("proxy-unreadable");
    let mut config = config(dir.path());
    config.proxy = Err(TransportError::ProxySecretUnavailable);
    let start = start_with_proxy(added());
    let transport = TdlibJson::new(Replay::new(fixture(&[&start, CLOSE])), config);
    let error = transport.start().await.unwrap_err();
    assert_eq!(error.code_and_params().0, "TG_PROXY_SECRET_UNAVAILABLE");
    assert!(
        transport.bindings().calls().is_empty(),
        "no client, no request: nothing could connect without the proxy"
    );

    // Entered again while no client runs: kept for the next start, which uses it.
    transport.set_proxy(Some(mtproto())).await.unwrap();
    assert!(transport.bindings().sent().is_empty());
    transport.start().await.unwrap();
    let replay = transport.bindings();
    assert_eq!(replay.sent_of("addProxy").len(), 1);
    assert!(replay.unexpected().is_empty(), "{:?}", replay.unexpected());
    transport.shutdown().await;
}
