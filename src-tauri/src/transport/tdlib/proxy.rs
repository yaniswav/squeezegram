//! The proxy in TDLib. Requests and fields follow `td_api.tl` of TDLib 1.8.68 (checked against
//! tdlib/td commit c15d3f5, "Update version to 1.8.68"):
//! `addProxy proxy:proxy enable:Bool comment:string = AddedProxy`, `disableProxy = Ok`,
//! `getProxies = AddedProxies`, `removeProxy proxy_id:int32 = Ok`,
//! `pingProxy proxy:proxy = Seconds`; `proxy server:string port:int32 type:ProxyType`,
//! `proxyTypeSocks5 username password`, `proxyTypeHttp username password http_only`,
//! `proxyTypeMtproto secret`.
//!
//! Before `setTdlibParameters` TDLib queues these requests and runs them right after creating
//! its `ConnectionCreator`, before the first connection (`Td::init_options_and_network`). So a
//! start queues the user's choice with the parameters: nothing ever leaves without the proxy.
//! TDLib also keeps every proxy it was given in its (encrypted) database; once the choice is
//! applied the others are removed, so no old secret stays there.
//!
//! The secret is stored in `proxy_secret.bin` of the TDLib folder, protected by DPAPI for the
//! current Windows user (`key.rs`), like the personal `api_hash`.

use std::fs;
use std::io;
use std::path::Path;
use std::time::Duration;

use serde_json::{Value, json};

use super::key::{protect, unprotect};
use super::{Inner, TdBindings};
use crate::settings::ProxySettings;
use crate::transport::TransportError;
use crate::transport::proxy::{Proxy, ProxyField, ProxyKind};

pub const SECRET_FILE: &str = "proxy_secret.bin";

/// The file is named, never its content.
fn io_error(error: io::Error) -> TransportError {
    TransportError::Protocol(format!("proxy secret file: {}", error.kind()))
}

/// Stores the secret protected by DPAPI (written beside then renamed); an empty secret removes
/// the file.
pub fn store_secret(dir: &Path, secret: &str) -> Result<(), TransportError> {
    if secret.is_empty() {
        return clear_secret(dir);
    }
    // `protect` words its failure for the database key; this is the proxy's.
    let blob = protect(secret.as_bytes())
        .map_err(|_| TransportError::Protocol("proxy secret: DPAPI protect failed".into()))?;
    fs::create_dir_all(dir).map_err(io_error)?;
    let temporary = dir.join(format!("{SECRET_FILE}.tmp"));
    fs::write(&temporary, &blob).map_err(io_error)?;
    fs::rename(&temporary, dir.join(SECRET_FILE)).map_err(io_error)
}

/// The stored secret, `None` when there is none. A blob DPAPI refuses (other user, other
/// machine, corrupt) is `ProxySecretUnavailable`, never an empty secret.
pub fn load_secret(dir: &Path) -> Result<Option<String>, TransportError> {
    let blob = match fs::read(dir.join(SECRET_FILE)) {
        Ok(blob) => blob,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(io_error(error)),
    };
    let mut clear = unprotect(&blob).map_err(|_| TransportError::ProxySecretUnavailable)?;
    let secret = String::from_utf8(clear.clone()).ok();
    clear.fill(0);
    secret
        .map(Some)
        .ok_or(TransportError::ProxySecretUnavailable)
}

pub fn clear_secret(dir: &Path) -> Result<(), TransportError> {
    match fs::remove_file(dir.join(SECRET_FILE)) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => Err(io_error(error)),
        _ => Ok(()),
    }
}

/// The proxy of the settings with its stored secret (`None`: direct). A secret that is missing
/// where one is required, or unreadable, is `ProxySecretUnavailable`: the transport then refuses
/// to start rather than connect without the proxy the user chose.
pub fn load(dir: &Path, settings: Option<&ProxySettings>) -> Result<Option<Proxy>, TransportError> {
    let Some(settings) = settings else {
        return Ok(None);
    };
    let secret = load_secret(dir)?.unwrap_or_default();
    Proxy::new(
        settings.kind,
        &settings.host,
        settings.port,
        &settings.username,
        &secret,
    )
    .map(Some)
    .map_err(|field| match field {
        ProxyField::Secret | ProxyField::Password => TransportError::ProxySecretUnavailable,
        // `Settings::validate` already checked the others.
        field => TransportError::Protocol(format!("proxy settings: {}", field.as_str())),
    })
}

/// TDLib `proxy` object.
pub fn proxy_object(proxy: &Proxy) -> Value {
    let kind = match proxy.kind {
        ProxyKind::Socks5 => json!({
            "@type": "proxyTypeSocks5",
            "username": proxy.username,
            "password": proxy.secret,
        }),
        ProxyKind::Http => json!({
            "@type": "proxyTypeHttp",
            "username": proxy.username,
            "password": proxy.secret,
            // Telegram needs a plain TCP tunnel (`CONNECT`).
            "http_only": false,
        }),
        ProxyKind::Mtproto => json!({ "@type": "proxyTypeMtproto", "secret": proxy.secret }),
    };
    json!({ "@type": "proxy", "server": proxy.host, "port": proxy.port, "type": kind })
}

/// `addProxy` with `enable`, or `disableProxy` for a direct connection.
pub fn choice_request(proxy: Option<&Proxy>) -> (&'static str, Value) {
    match proxy {
        Some(proxy) => (
            "addProxy",
            json!({ "@type": "addProxy", "proxy": proxy_object(proxy), "enable": true, "comment": "" }),
        ),
        None => ("disableProxy", json!({ "@type": "disableProxy" })),
    }
}

/// TDLib refuses a proxy with a 400 (`Wrong proxy secret`, `Wrong port number`…) and fails a
/// ping with the connection error: both become `Proxy` with its message.
fn proxy_error(error: TransportError) -> TransportError {
    match error {
        TransportError::Other { message, .. } => TransportError::Proxy(message),
        other => other,
    }
}

fn int(value: &Value) -> Option<i64> {
    match value {
        Value::String(text) => text.parse().ok(),
        value => value.as_i64(),
    }
}

impl<B: TdBindings> Inner<B> {
    /// Waits for the answer to `choice_request`, then removes every other proxy TDLib holds.
    pub(super) async fn finish_proxy(
        &self,
        what: &'static str,
        extra: u64,
        receiver: tokio::sync::oneshot::Receiver<Value>,
        timeout: Duration,
    ) -> Result<(), TransportError> {
        let answer = self
            .wait(what, extra, receiver, timeout)
            .await
            .map_err(proxy_error)?;
        let keep = match answer["@type"].as_str() {
            Some("addedProxy") => Some(
                int(&answer["id"])
                    .ok_or_else(|| TransportError::Protocol("addProxy: missing id".into()))?,
            ),
            _ => None,
        };
        let list = self
            .request("getProxies", json!({ "@type": "getProxies" }), timeout)
            .await?;
        let others: Vec<i64> = list["proxies"]
            .as_array()
            .ok_or_else(|| TransportError::Protocol("getProxies: missing proxies".into()))?
            .iter()
            .filter_map(|added| int(&added["id"]))
            .filter(|id| Some(*id) != keep)
            .collect();
        for id in &others {
            self.request(
                "removeProxy",
                json!({ "@type": "removeProxy", "proxy_id": id }),
                timeout,
            )
            .await
            .map_err(proxy_error)?;
        }
        tracing::info!(
            event = "tdlib_proxy",
            enabled = keep.is_some(),
            removed = others.len(),
            "Telegram proxy applied"
        );
        Ok(())
    }

    /// Applies the choice to the running client.
    pub(super) async fn apply_proxy(&self, proxy: Option<&Proxy>) -> Result<(), TransportError> {
        let (what, body) = choice_request(proxy);
        let (extra, receiver) = self.submit(what, body)?;
        self.finish_proxy(what, extra, receiver, self.config.request_timeout)
            .await
    }

    /// `pingProxy`: the round trip to a Telegram server through `proxy`; the proxy in use is
    /// not touched.
    pub(super) async fn ping_proxy(&self, proxy: &Proxy) -> Result<Duration, TransportError> {
        let answer = self
            .request(
                "pingProxy",
                json!({ "@type": "pingProxy", "proxy": proxy_object(proxy) }),
                self.config.request_timeout,
            )
            .await
            .map_err(proxy_error)?;
        answer["seconds"]
            .as_f64()
            .filter(|seconds| seconds.is_finite() && *seconds >= 0.0)
            .map(Duration::from_secs_f64)
            .ok_or_else(|| TransportError::Protocol("pingProxy: missing seconds".into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::tdlib::tests::TempDir;

    const MT_SECRET: &str = "dd0123456789abcdef0123456789abcdef";

    #[test]
    fn requests_follow_the_1_8_68_schema() {
        let socks = Proxy::new(ProxyKind::Socks5, "10.1.2.3", 1080, "u", "p").unwrap();
        assert_eq!(
            choice_request(Some(&socks)).1,
            json!({ "@type": "addProxy", "enable": true, "comment": "",
                    "proxy": { "@type": "proxy", "server": "10.1.2.3", "port": 1080,
                               "type": { "@type": "proxyTypeSocks5", "username": "u", "password": "p" } } })
        );
        let http = Proxy::new(ProxyKind::Http, "proxy.lan", 3128, "", "").unwrap();
        assert_eq!(
            proxy_object(&http)["type"],
            json!({ "@type": "proxyTypeHttp", "username": "", "password": "", "http_only": false })
        );
        let mtproto = Proxy::new(ProxyKind::Mtproto, "mt.example", 443, "", MT_SECRET).unwrap();
        assert_eq!(
            proxy_object(&mtproto)["type"],
            json!({ "@type": "proxyTypeMtproto", "secret": MT_SECRET })
        );
        assert_eq!(
            choice_request(None),
            ("disableProxy", json!({ "@type": "disableProxy" }))
        );
    }

    #[test]
    fn errors_of_a_proxy_request_carry_tdlib_s_message() {
        let refused = proxy_error(TransportError::Other {
            code: 400,
            message: "Wrong proxy secret".into(),
        });
        assert_eq!(
            refused.code_and_params(),
            (
                "TG_PROXY_FAILED",
                json!({ "message": "Wrong proxy secret" })
            )
        );
        assert!(matches!(
            proxy_error(TransportError::Timeout("pingProxy")),
            TransportError::Timeout(_)
        ));
    }

    #[cfg(windows)]
    #[test]
    fn the_secret_is_dpapi_protected_and_loaded_with_the_settings() {
        let dir = TempDir::new("proxy-secret");
        let settings = ProxySettings {
            kind: ProxyKind::Mtproto,
            host: "mt.example".into(),
            port: 443,
            username: String::new(),
        };
        // No secret stored: an MTProto proxy cannot be used.
        assert!(matches!(
            load(dir.path(), Some(&settings)),
            Err(TransportError::ProxySecretUnavailable)
        ));
        assert_eq!(load(dir.path(), None).unwrap(), None);

        store_secret(dir.path(), MT_SECRET).unwrap();
        let blob = fs::read(dir.path().join(SECRET_FILE)).unwrap();
        assert!(
            !blob
                .windows(MT_SECRET.len())
                .any(|w| w == MT_SECRET.as_bytes()),
            "the file is a DPAPI blob, not the secret"
        );
        assert!(!dir.path().join(format!("{SECRET_FILE}.tmp")).exists());
        let proxy = load(dir.path(), Some(&settings)).unwrap().unwrap();
        assert_eq!(
            (proxy.kind, proxy.port, proxy.secret.as_str()),
            (ProxyKind::Mtproto, 443, MT_SECRET)
        );

        // A SOCKS5 proxy without a password needs no file.
        store_secret(dir.path(), "").unwrap();
        assert_eq!(load_secret(dir.path()).unwrap(), None);
        let open = ProxySettings {
            kind: ProxyKind::Socks5,
            ..settings.clone()
        };
        assert!(
            load(dir.path(), Some(&open))
                .unwrap()
                .unwrap()
                .secret
                .is_empty()
        );

        // A blob DPAPI refuses is never read as "no secret".
        fs::write(dir.path().join(SECRET_FILE), b"not a DPAPI blob").unwrap();
        assert!(matches!(
            load(dir.path(), Some(&open)),
            Err(TransportError::ProxySecretUnavailable)
        ));
        clear_secret(dir.path()).unwrap();
        clear_secret(dir.path()).unwrap();
        assert_eq!(load_secret(dir.path()).unwrap(), None);
    }
}
