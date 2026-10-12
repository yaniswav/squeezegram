//! Build script: Tauri's, plus the product's Telegram identity.
//!
//! `SQUEEZEGRAM_TG_API_ID` and `SQUEEZEGRAM_TG_API_HASH` come from the environment (CI secret)
//! or, failing that, from `../.env` (never committed). They reach the code through
//! `cargo:rustc-env`, read with `option_env!`. Their values are never printed: no
//! `cargo:warning`, no message. Absent values are not an error: the TDLib transport then
//! refuses to start (`TG_NOT_CONFIGURED`).

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
        let from_env = std::env::var(key).ok();
        if let Some(value) = dotenv::resolve(key, from_env.as_deref(), dotenv_text.as_deref()) {
            // Instruction to Cargo (stored in target/, given to rustc), not a message.
            println!("cargo:rustc-env={key}={value}");
        }
    }
}

fn main() {
    inject_telegram_identity();
    tauri_build::build()
}
