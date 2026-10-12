//! `updateAuthorizationState` → `AuthState`, `AuthInput` → TDLib request, and TDLib errors →
//! `TransportError` (names from `td_api.tl` 1.8.61).

use std::time::Duration;

use serde_json::{Value, json};

use crate::transport::{AuthInput, AuthState, TransportError};

pub const WAIT_PARAMETERS: &str = "authorizationStateWaitTdlibParameters";
pub const READY: &str = "authorizationStateReady";
pub const CLOSED: &str = "authorizationStateClosed";

/// What the receive thread publishes for one `authorizationState*` object.
#[derive(Debug)]
pub enum Mapped {
    State(AuthState),
    /// `authorizationStateReady`: `Me` must be fetched first (`getMe`).
    Ready,
}

fn text<'a>(object: &'a Value, field: &str) -> Result<&'a str, TransportError> {
    object[field]
        .as_str()
        .ok_or_else(|| protocol(object, field))
}

fn protocol(object: &Value, field: &str) -> TransportError {
    let kind = object["@type"].as_str().unwrap_or("?");
    TransportError::Protocol(format!("{kind}: missing {field}"))
}

/// Maps `updateAuthorizationState.authorization_state`. Unknown steps (sign-up, Premium
/// purchase required, passkey…) become `OtherStep` with the TDLib type as name.
pub fn map_state(state: &Value) -> Result<Mapped, TransportError> {
    let kind = text(state, "@type")?;
    let mapped = match kind {
        WAIT_PARAMETERS => AuthState::Starting,
        "authorizationStateWaitPhoneNumber" => AuthState::WaitPhone,
        "authorizationStateWaitOtherDeviceConfirmation" => AuthState::WaitQr {
            link: text(state, "link")?.to_owned(),
        },
        "authorizationStateWaitCode" => {
            let info = &state["code_info"];
            if !info.is_object() {
                return Err(protocol(state, "code_info"));
            }
            // `timeout`: seconds before `resendAuthenticationCode` is allowed; no `next_type`
            // means there is nothing to resend to.
            let resend_in = match (&info["next_type"], info["timeout"].as_i64()) {
                (Value::Object(_), Some(seconds)) => {
                    Some(Duration::from_secs(u64::try_from(seconds).unwrap_or(0)))
                }
                (Value::Object(_), None) => return Err(protocol(info, "timeout")),
                _ => None,
            };
            AuthState::WaitCode { resend_in }
        }
        "authorizationStateWaitEmailAddress" => AuthState::WaitEmail,
        "authorizationStateWaitEmailCode" => AuthState::WaitEmailCode,
        "authorizationStateWaitPassword" => AuthState::WaitPassword {
            hint: text(state, "password_hint")?.to_owned(),
        },
        READY => return Ok(Mapped::Ready),
        // `Closing` follows `close` and `logOut`; the closest state the UI knows.
        "authorizationStateLoggingOut" | "authorizationStateClosing" => AuthState::LoggingOut,
        CLOSED => AuthState::Closed,
        other => AuthState::OtherStep {
            name: other.to_owned(),
        },
    };
    Ok(Mapped::State(mapped))
}

/// The request for a login step. The values go to TDLib only, never to a log.
pub fn request_for(input: AuthInput) -> (&'static str, Value) {
    match input {
        AuthInput::Phone(phone_number) => (
            "setAuthenticationPhoneNumber",
            // `settings`: null = TDLib defaults.
            json!({ "@type": "setAuthenticationPhoneNumber", "phone_number": phone_number, "settings": null }),
        ),
        AuthInput::RequestQr => (
            "requestQrCodeAuthentication",
            json!({ "@type": "requestQrCodeAuthentication", "other_user_ids": [] }),
        ),
        AuthInput::Code(code) => (
            "checkAuthenticationCode",
            json!({ "@type": "checkAuthenticationCode", "code": code }),
        ),
        AuthInput::ResendCode => (
            "resendAuthenticationCode",
            json!({ "@type": "resendAuthenticationCode", "reason": null }),
        ),
        AuthInput::Email(email_address) => (
            "setAuthenticationEmailAddress",
            json!({ "@type": "setAuthenticationEmailAddress", "email_address": email_address }),
        ),
        AuthInput::EmailCode(code) => (
            "checkAuthenticationEmailCode",
            json!({
                "@type": "checkAuthenticationEmailCode",
                "code": { "@type": "emailAddressAuthenticationCode", "code": code },
            }),
        ),
        AuthInput::Password(password) => (
            "checkAuthenticationPassword",
            json!({ "@type": "checkAuthenticationPassword", "password": password }),
        ),
    }
}

/// `retry after X` in TDLib's 429 message (`Global::get_retry_after`).
fn retry_after(message: &str) -> Option<Duration> {
    let tail = message.split("retry after ").nth(1)?;
    let digits: String = tail.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok().map(Duration::from_secs)
}

/// A TDLib `error` object → `TransportError`. 401 is `NotAuthorized` here; only the caller of
/// `setTdlibParameters` turns it into `BadDatabaseKey`. A 429 without a readable delay stays
/// `Other` (refused, never retried at once) rather than an invented delay.
pub fn map_error(error: &Value) -> TransportError {
    let Some(code) = error["code"].as_i64().and_then(|c| i32::try_from(c).ok()) else {
        return TransportError::Protocol("error without code".into());
    };
    let message = error["message"].as_str().unwrap_or_default().to_owned();
    match code {
        401 => TransportError::NotAuthorized,
        429 => match retry_after(&message) {
            Some(retry_after) => TransportError::FloodWait { retry_after },
            None => TransportError::Other { code, message },
        },
        _ => TransportError::Other { code, message },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(value: Value) -> AuthState {
        match map_state(&value).unwrap() {
            Mapped::State(state) => state,
            Mapped::Ready => panic!("ready"),
        }
    }

    #[test]
    fn every_known_state_maps() {
        assert!(matches!(
            state(json!({"@type": WAIT_PARAMETERS})),
            AuthState::Starting
        ));
        assert!(matches!(
            state(json!({"@type": "authorizationStateWaitPhoneNumber"})),
            AuthState::WaitPhone
        ));
        assert!(matches!(
            state(json!({"@type": "authorizationStateWaitOtherDeviceConfirmation", "link": "tg://login?token=a"})),
            AuthState::WaitQr { link } if link == "tg://login?token=a"
        ));
        let code = json!({"@type": "authorizationStateWaitCode", "code_info": {
            "@type": "authenticationCodeInfo", "phone_number": "+0",
            "type": {"@type": "authenticationCodeTypeTelegramMessage", "length": 5},
            "next_type": {"@type": "authenticationCodeTypeSms", "length": 5}, "timeout": 60}});
        assert!(matches!(
            state(code),
            AuthState::WaitCode { resend_in: Some(d) } if d == Duration::from_secs(60)
        ));
        let no_resend = json!({"@type": "authorizationStateWaitCode", "code_info": {
            "@type": "authenticationCodeInfo", "type": {"@type": "authenticationCodeTypeSms"},
            "next_type": null, "timeout": 0}});
        assert!(matches!(
            state(no_resend),
            AuthState::WaitCode { resend_in: None }
        ));
        assert!(matches!(
            state(json!({"@type": "authorizationStateWaitEmailAddress"})),
            AuthState::WaitEmail
        ));
        assert!(matches!(
            state(json!({"@type": "authorizationStateWaitEmailCode"})),
            AuthState::WaitEmailCode
        ));
        assert!(matches!(
            state(json!({"@type": "authorizationStateWaitPassword", "password_hint": "cat"})),
            AuthState::WaitPassword { hint } if hint == "cat"
        ));
        assert!(matches!(
            map_state(&json!({"@type": READY})),
            Ok(Mapped::Ready)
        ));
        assert!(matches!(
            state(json!({"@type": "authorizationStateLoggingOut"})),
            AuthState::LoggingOut
        ));
        assert!(matches!(
            state(json!({"@type": "authorizationStateClosing"})),
            AuthState::LoggingOut
        ));
        assert!(matches!(state(json!({"@type": CLOSED})), AuthState::Closed));
        for other in [
            "authorizationStateWaitRegistration",
            "authorizationStateWaitPremiumPurchase",
            "authorizationStateSomethingNew",
        ] {
            assert!(matches!(
                state(json!({"@type": other})),
                AuthState::OtherStep { name } if name == other
            ));
        }
    }

    #[test]
    fn malformed_states_are_protocol_errors() {
        for bad in [
            json!({}),
            json!({"@type": 3}),
            json!({"@type": "authorizationStateWaitOtherDeviceConfirmation"}),
            json!({"@type": "authorizationStateWaitPassword"}),
            json!({"@type": "authorizationStateWaitCode"}),
            json!({"@type": "authorizationStateWaitCode", "code_info": {"next_type": {"@type": "x"}}}),
        ] {
            assert!(
                matches!(map_state(&bad), Err(TransportError::Protocol(_))),
                "{bad}"
            );
        }
    }

    #[test]
    fn errors_map() {
        assert!(matches!(
            map_error(&json!({"@type": "error", "code": 401, "message": "Unauthorized"})),
            TransportError::NotAuthorized
        ));
        assert!(matches!(
            map_error(&json!({"@type": "error", "code": 429, "message": "Too Many Requests: retry after 3600"})),
            TransportError::FloodWait { retry_after } if retry_after == Duration::from_secs(3600)
        ));
        assert!(matches!(
            map_error(&json!({"@type": "error", "code": 429, "message": "Too Many Requests"})),
            TransportError::Other { code: 429, .. }
        ));
        assert!(matches!(
            map_error(&json!({"@type": "error", "code": 400, "message": "PHONE_CODE_INVALID"})),
            TransportError::Other { code: 400, message } if message == "PHONE_CODE_INVALID"
        ));
        assert!(matches!(
            map_error(&json!({"@type": "error", "message": "x"})),
            TransportError::Protocol(_)
        ));
    }

    #[test]
    fn requests_use_the_tdlib_names() {
        let cases = [
            (
                AuthInput::Phone("+1".into()),
                "setAuthenticationPhoneNumber",
            ),
            (AuthInput::RequestQr, "requestQrCodeAuthentication"),
            (AuthInput::Code("1".into()), "checkAuthenticationCode"),
            (AuthInput::ResendCode, "resendAuthenticationCode"),
            (
                AuthInput::Email("a@b".into()),
                "setAuthenticationEmailAddress",
            ),
            (
                AuthInput::EmailCode("2".into()),
                "checkAuthenticationEmailCode",
            ),
            (
                AuthInput::Password("p".into()),
                "checkAuthenticationPassword",
            ),
        ];
        for (input, name) in cases {
            let (what, body) = request_for(input);
            assert_eq!(what, name);
            assert_eq!(body["@type"], name);
        }
        let (_, body) = request_for(AuthInput::EmailCode("2".into()));
        assert_eq!(body["code"]["@type"], "emailAddressAuthenticationCode");
        assert_eq!(body["code"]["code"], "2");
        let (_, body) = request_for(AuthInput::RequestQr);
        assert_eq!(body["other_user_ids"], json!([]));
    }
}
