//! Proxy for every Telegram connection, for users where Telegram is blocked or throttled:
//! SOCKS5, HTTP (`CONNECT`) or MTProto. Transport-neutral; `tdlib::proxy` turns it into TDLib
//! requests and keeps its secret.
//!
//! The settings hold `kind`, `host`, `port` and `username` (`settings.network.proxy`). The
//! secret (SOCKS5/HTTP password, MTProto secret) is protected by DPAPI in the TDLib folder,
//! like the personal `api_hash`: never in `settings.json`, never sent back to the page, never
//! in a log or a `Debug`.

use std::fmt;
use std::net::IpAddr;

use base64::Engine;
use base64::alphabet;
use base64::engine::{DecodePaddingMode, GeneralPurpose, GeneralPurposeConfig};
use serde::{Deserialize, Serialize};

/// SOCKS5 user names and passwords are at most 255 bytes (RFC 1929).
const MAX_CREDENTIAL_BYTES: usize = 255;
/// Longest DNS name.
const MAX_HOST_CHARS: usize = 253;
/// Fake-TLS domain after the 17 bytes of an `ee` MTProto secret (TDLib `MAX_DOMAIN_LENGTH`).
const MAX_TLS_DOMAIN: usize = 182;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProxyKind {
    Socks5,
    /// HTTP proxy that allows `CONNECT` (TDLib `proxyTypeHttp` with `http_only = false`).
    Http,
    Mtproto,
}

impl ProxyKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ProxyKind::Socks5 => "socks5",
            ProxyKind::Http => "http",
            ProxyKind::Mtproto => "mtproto",
        }
    }
}

/// The field a proxy is refused for (`PROXY_INVALID { field }`); never its value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProxyField {
    Host,
    Port,
    Username,
    Password,
    Secret,
}

impl ProxyField {
    pub fn as_str(self) -> &'static str {
        match self {
            ProxyField::Host => "host",
            ProxyField::Port => "port",
            ProxyField::Username => "username",
            ProxyField::Password => "password",
            ProxyField::Secret => "secret",
        }
    }
}

/// A validated proxy, ready for the transport. `secret` is the SOCKS5/HTTP password (may be
/// empty) or the MTProto secret (never empty).
#[derive(Clone, PartialEq, Eq)]
pub struct Proxy {
    pub kind: ProxyKind,
    pub host: String,
    pub port: u16,
    /// Empty for MTProto, and for a SOCKS5/HTTP proxy without authentication.
    pub username: String,
    pub secret: String,
}

/// Written by hand: the kind only. The address and the user name say who the user is behind.
impl fmt::Debug for Proxy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Proxy")
            .field("kind", &self.kind)
            .finish_non_exhaustive()
    }
}

impl Proxy {
    /// Checks every field, in the order of the form; the first refused one is returned.
    pub fn new(
        kind: ProxyKind,
        host: &str,
        port: u32,
        username: &str,
        secret: &str,
    ) -> Result<Self, ProxyField> {
        let host = check_host(host)?;
        let port = check_port(port)?;
        let (username, secret) = match kind {
            ProxyKind::Mtproto => {
                if !username.trim().is_empty() {
                    return Err(ProxyField::Username);
                }
                (String::new(), check_mtproto_secret(secret)?)
            }
            ProxyKind::Socks5 | ProxyKind::Http => (
                check_credential(username, ProxyField::Username)?,
                check_credential(secret, ProxyField::Password)?,
            ),
        };
        Ok(Self {
            kind,
            host,
            port,
            username,
            secret,
        })
    }
}

/// An IP address, or a DNS name (ASCII labels of letters, digits and inner hyphens). A URL, a
/// port or a space is refused: TDLib takes the bare server name.
pub fn check_host(host: &str) -> Result<String, ProxyField> {
    let host = host.trim();
    if host.parse::<IpAddr>().is_ok() {
        return Ok(host.to_owned());
    }
    let label_ok = |label: &str| {
        (1..=63).contains(&label.len())
            && label
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-')
            && !label.starts_with('-')
            && !label.ends_with('-')
    };
    if host.is_empty() || host.len() > MAX_HOST_CHARS || !host.split('.').all(label_ok) {
        return Err(ProxyField::Host);
    }
    Ok(host.to_owned())
}

pub fn check_port(port: u32) -> Result<u16, ProxyField> {
    u16::try_from(port)
        .ok()
        .filter(|port| *port != 0)
        .ok_or(ProxyField::Port)
}

/// SOCKS5/HTTP user name or password: at most 255 bytes, no control character. Kept as typed
/// (spaces included): a password may start or end with one.
fn check_credential(text: &str, field: ProxyField) -> Result<String, ProxyField> {
    if text.len() > MAX_CREDENTIAL_BYTES || text.chars().any(char::is_control) {
        return Err(field);
    }
    Ok(text.to_owned())
}

/// The encodings TDLib accepts for an MTProto secret (`ProxySecret::from_link`), tried in its
/// order: hexadecimal, then base64url, then base64; padding is optional.
fn decode_secret(text: &str) -> Option<Vec<u8>> {
    let lenient = GeneralPurposeConfig::new()
        .with_decode_padding_mode(DecodePaddingMode::Indifferent)
        .with_decode_allow_trailing_bits(true);
    if text.len().is_multiple_of(2) && text.bytes().all(|b| b.is_ascii_hexdigit()) {
        return (0..text.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&text[i..i + 2], 16).ok())
            .collect();
    }
    GeneralPurpose::new(&alphabet::URL_SAFE, lenient)
        .decode(text)
        .or_else(|_| GeneralPurpose::new(&alphabet::STANDARD, lenient).decode(text))
        .ok()
}

/// An MTProto secret as TDLib checks it (`ProxySecret::from_binary`): 16 bytes, `dd` + 16
/// bytes (padded intermediate), or `ee` + 16 bytes + the fake-TLS domain. Returned as typed
/// (trimmed): TDLib decodes it itself.
pub fn check_mtproto_secret(text: &str) -> Result<String, ProxyField> {
    let text = text.trim();
    let bytes = decode_secret(text).ok_or(ProxyField::Secret)?;
    let valid = match (bytes.len(), bytes.first()) {
        (16, _) | (17, Some(0xdd)) => true,
        (len, Some(0xee)) => (18..=17 + MAX_TLS_DOMAIN).contains(&len),
        _ => false,
    };
    if valid {
        Ok(text.to_owned())
    } else {
        Err(ProxyField::Secret)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Fake secrets: random bytes, no real proxy.
    const HEX16: &str = "0123456789abcdef0123456789abcdef";

    #[test]
    fn hosts_are_names_or_addresses() {
        for good in [
            "proxy.example.org",
            " 127.0.0.1 ",
            "localhost",
            "::1",
            "2001:db8::7",
            "xn--bcher-kva.example",
            "a-b.c1",
        ] {
            assert_eq!(check_host(good), Ok(good.trim().to_owned()), "{good}");
        }
        for bad in [
            "",
            "   ",
            "https://proxy.example.org",
            "proxy.example.org:1080",
            "pro xy.org",
            "-proxy.org",
            "proxy-.org",
            "proxy..org",
            ".proxy.org",
            "proxy.org.",
            "bücher.example",
            "[::1]",
            &"a".repeat(64),
            &format!("{}.org", ["abc"; 70].join(".")),
        ] {
            assert_eq!(check_host(bad), Err(ProxyField::Host), "{bad}");
        }
    }

    #[test]
    fn ports_go_from_1_to_65535() {
        assert_eq!(check_port(1), Ok(1));
        assert_eq!(check_port(65_535), Ok(65_535));
        assert_eq!(check_port(0), Err(ProxyField::Port));
        assert_eq!(check_port(65_536), Err(ProxyField::Port));
    }

    #[test]
    fn mtproto_secrets_follow_tdlib() {
        let ee_hex = format!("ee{HEX16}{}", hex(b"www.example.com"));
        let mut ee_bytes = vec![0xee];
        ee_bytes.extend_from_slice(&[7u8; 16]);
        ee_bytes.extend_from_slice(b"www.example.com");
        let url_safe = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&ee_bytes);
        let padded = base64::engine::general_purpose::STANDARD.encode([0xfbu8; 16]);
        for good in [
            HEX16.to_owned(),
            HEX16.to_uppercase(),
            format!("dd{HEX16}"),
            ee_hex,
            url_safe,
            padded,
            format!("  {HEX16}  "),
        ] {
            assert_eq!(
                check_mtproto_secret(&good),
                Ok(good.trim().to_owned()),
                "{good}"
            );
        }
        let too_long = format!("ee{HEX16}{}", "61".repeat(MAX_TLS_DOMAIN + 1));
        for bad in [
            String::new(),
            HEX16[..30].to_owned(),
            format!("ab{HEX16}"),
            format!("ee{HEX16}"),
            too_long,
            "not a secret!".to_owned(),
            format!("{HEX16}0"),
        ] {
            assert_eq!(check_mtproto_secret(&bad), Err(ProxyField::Secret), "{bad}");
        }
    }

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    #[test]
    fn a_proxy_is_checked_field_by_field_and_never_debugged() {
        let socks = Proxy::new(ProxyKind::Socks5, "10.0.0.2", 1080, "alice", " p@ss ").unwrap();
        assert_eq!(
            (socks.port, socks.username.as_str(), socks.secret.as_str()),
            (1080, "alice", " p@ss "),
            "credentials are kept as typed"
        );
        let open = Proxy::new(ProxyKind::Http, "proxy.lan", 3128, "", "").unwrap();
        assert!(open.username.is_empty() && open.secret.is_empty());
        let mtproto = Proxy::new(ProxyKind::Mtproto, "mt.example", 443, "  ", HEX16).unwrap();
        assert!(mtproto.username.is_empty());

        let refused = |kind, host, port, user: &str, secret: &str| {
            Proxy::new(kind, host, port, user, secret).unwrap_err()
        };
        assert_eq!(
            refused(ProxyKind::Socks5, "", 0, "", ""),
            ProxyField::Host,
            "the first field wins"
        );
        assert_eq!(
            refused(ProxyKind::Socks5, "h.org", 0, "", ""),
            ProxyField::Port
        );
        assert_eq!(
            refused(ProxyKind::Socks5, "h.org", 1, &"u".repeat(256), ""),
            ProxyField::Username
        );
        assert_eq!(
            refused(ProxyKind::Http, "h.org", 1, "u", "line\nbreak"),
            ProxyField::Password
        );
        assert_eq!(
            refused(ProxyKind::Mtproto, "h.org", 1, "user", HEX16),
            ProxyField::Username
        );
        assert_eq!(
            refused(ProxyKind::Mtproto, "h.org", 1, "", ""),
            ProxyField::Secret,
            "an MTProto proxy needs its secret"
        );

        let text = format!("{socks:?} {socks:#?} {mtproto:?}");
        for hidden in ["10.0.0.2", "alice", "p@ss", "1080", HEX16, "mt.example"] {
            assert!(!text.contains(hidden), "{text}");
        }
        assert!(text.contains("Socks5"));
    }
}
