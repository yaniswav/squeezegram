//! Build script: Tauri's, plus the product's Telegram identity.
//!
//! `SQUEEZEGRAM_TG_API_ID` and `SQUEEZEGRAM_TG_API_HASH` come from the environment (CI secret)
//! or, failing that, from `../.env` (never committed). The code reads them with `option_env!`.
//! A value already in the environment reaches rustc by inheritance, so this script does not
//! repeat it: Cargo copies a build script's stdout into the build log when the script fails,
//! and that log is public in CI. Only a value read from `.env` (local builds) is passed with
//! `cargo:rustc-env`. Absent values are not an error: the TDLib transport then refuses to
//! start (`TG_NOT_CONFIGURED`).

use std::path::Path;

#[path = "src/transport/tdlib/dotenv.rs"]
mod dotenv;

const IDENTITY_KEYS: [&str; 2] = ["SQUEEZEGRAM_TG_API_ID", "SQUEEZEGRAM_TG_API_HASH"];

fn inject_telegram_identity() {
    // When `.env` does not exist Cargo reruns this script on every build (cheap).
    println!("cargo:rerun-if-changed=../.env");
    for key in IDENTITY_KEYS {
        println!("cargo:rerun-if-env-changed={key}");
    }
    let manifest = std::env::var_os("CARGO_MANIFEST_DIR").unwrap_or_default();
    let dotenv_text = std::fs::read_to_string(Path::new(&manifest).join("..").join(".env")).ok();
    for key in IDENTITY_KEYS {
        if std::env::var(key).is_ok_and(|value| !value.trim().is_empty()) {
            // rustc inherits the variable: printing it would only expose it in a failure log.
            continue;
        }
        if let Some(value) = dotenv::resolve(key, None, dotenv_text.as_deref()) {
            // Instruction to Cargo (stored in target/, given to rustc), not a message.
            println!("cargo:rustc-env={key}={value}");
        }
    }
}

/// `test-keyring` embeds `keys/t1.pub`; without it say what to put there (include_str! alone
/// only names the missing path).
fn require_test_key() {
    println!("cargo:rerun-if-changed=keys/t1.pub");
    let manifest = std::env::var_os("CARGO_MANIFEST_DIR").unwrap_or_default();
    let path = Path::new(&manifest).join("keys").join("t1.pub");
    if std::env::var_os("CARGO_FEATURE_TEST_KEYRING").is_some() && !path.is_file() {
        panic!(
            "feature test-keyring needs {}: the test bot's PUBLIC key as 64 lowercase hex digits (see keys/README.md)",
            path.display()
        );
    }
}

fn main() {
    inject_telegram_identity();
    require_test_key();
    tauri_build::build()
}
