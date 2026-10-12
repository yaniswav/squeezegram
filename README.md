# squeezegram

squeezegram is a Windows desktop app that sends local videos and photos to Telegram chats, groups and forum topics. When a video is over Telegram's upload limit (2,000 MiB, or 4,000 MiB with Premium), the app compresses it to just under the limit with the best encoder it finds on the machine: NVENC, Quick Sync or AMF on a GPU, otherwise a CPU encoder.

It is built with Tauri 2 (Rust) and React. Telegram access goes through [TDLib](https://github.com/tdlib/td), and compression through FFmpeg running as a separate process.

## Status

Alpha. There is no release and no installer yet; the only way to run it is to build it from source.

Implemented, with unit and integration tests:

- Persistent queue (SQLite) with one upload at a time, pause, cancel, reorder, and recovery after a crash. An item whose upload outcome is unknown is never resent automatically.
- Media pipeline: `ffprobe` analysis, encoder detection by test encode, encoding to a size target with a corrective pass, remux when re-encoding is not needed, BLAKE3 content hash for duplicate detection.
- FFmpeg setup: download of a pinned LGPL build with SHA-256 check, or use of an FFmpeg already on the machine.
- A structured local log and a debug export with paths and identifiers masked.

In progress or missing:

- The TDLib transport is being integrated. The sign-in flow is tested against recorded TDLib responses only. Real sign-in, destination listing, uploads, FLOOD_WAIT handling and reconciliation are not verified end to end yet.
- In a debug build the app uses a mock transport by default, so nothing reaches Telegram.
- Not written yet: watched folder, thumbnails, auto-update, installer and release pipeline.
- Windows 10/11 x64 only.

## Requirements

- Windows 10 or 11, x64
- Node.js 22.12 or later (`.node-version`) and pnpm 10 (the exact version is pinned in `package.json`; `corepack enable` installs it)
- Rust stable 1.90 or later with the MSVC toolchain (Visual Studio Build Tools, "Desktop development with C++")
- Microsoft Edge WebView2 Runtime (preinstalled on Windows 11)
- For the TDLib transport: the Microsoft Visual C++ Redistributable (x64), which `tdjson.dll` needs

## Build and run

```powershell
pnpm install
pnpm tauri dev
```

This starts the app with the mock transport. To use the real Telegram transport:

1. Get a `tdjson.dll` built from TDLib 1.8.68 (https://github.com/tdlib/td, commit `c15d3f5a5de6e3ba5839822c451152e5e18bb700`, Windows x64, OpenSSL 3.6.5 and zlib 1.3.2 statically linked). No prebuilt copy is published yet. The script checks the SHA-256 of the pinned build and copies the DLL to `%LOCALAPPDATA%\squeezegram\tdlib\bin`:

   ```powershell
   pwsh -File scripts\install-tdjson.ps1 -FromFile C:\path\to\tdjson.dll
   ```

   A DLL with a different hash is reported at startup; release builds refuse it.

2. Create a Telegram API application at <https://my.telegram.org> (API development tools) to get an `api_id` and an `api_hash`.

3. Create a `.env` file at the repository root with your own values:

   ```
   SQUEEZEGRAM_TG_API_ID=<your api_id>
   SQUEEZEGRAM_TG_API_HASH=<your api_hash>
   ```

   `src-tauri/build.rs` reads these at build time (environment variables take precedence over `.env`) and compiles them into the binary. `.env` is in `.gitignore`; never commit it.

4. Select the transport and start the app:

   ```powershell
   $env:SQUEEZEGRAM_TRANSPORT = 'tdlib'   # tdlib | mock | none
   pnpm tauri dev
   ```

   Without `SQUEEZEGRAM_TRANSPORT`, a debug build uses `mock` and a release build uses `tdlib`.

Checks (the CI workflow in `.github/workflows/build.yml` runs all of them except `cargo test`):

```powershell
pnpm typecheck; pnpm lint; pnpm format:check
cd src-tauri; cargo fmt --check; cargo clippy --locked -- -D warnings; cargo test
```

## Privacy

squeezegram collects no analytics or telemetry and has no server of its own. It makes two kinds of outgoing connections:

- to Telegram's servers, through TDLib, when the Telegram transport is enabled;
- to GitHub, to download FFmpeg from the pinned BtbN release on first setup.

The Telegram session is stored locally in an encrypted TDLib database whose key is protected with Windows DPAPI. Logs stay on your machine; the debug export is a file you create and share yourself.

Sending messages to groups through a user account is subject to Telegram's terms. Repeated sends to groups where you lack permissions can get an account restricted.

## Antivirus warnings

Builds are not code-signed. Windows SmartScreen and some antivirus products may warn about an unsigned executable or about `tdjson.dll`, and false positives are possible. Building from source lets you check what you run.

## License

squeezegram is source-available under the [PolyForm Noncommercial License 1.0.0](LICENSE). You may use, modify and share it for noncommercial purposes as defined in that license.

Third-party components keep their own licenses: see [THIRD-PARTY-NOTICES.md](THIRD-PARTY-NOTICES.md).

### Commercial use

Commercial use requires a separate written agreement with the author. To ask, open an issue on this repository.

## Contributing and security

See [CONTRIBUTING.md](CONTRIBUTING.md) and [SECURITY.md](SECURITY.md). The maintainer's internal design notes are not published in this repository.
