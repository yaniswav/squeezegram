# squeezegram

squeezegram is a Windows app that sends videos and photos from your computer to Telegram chats, groups and forum topics, from your own Telegram account. When a video is over Telegram's upload limit, it first compresses it to just under that limit, using your graphics card when it can (NVENC, Quick Sync or AMF), otherwise the processor.

- The limit is 2,000 MiB (about 2 GB), or 4,000 MiB (about 4 GB) if your Telegram account has Telegram Premium. squeezegram reads which one applies from your account, and you can override it in Settings.
- Files already under the limit are sent as they are, or repackaged without re-encoding when that is enough.
- Uploads go through a persistent queue: one upload at a time, pause, cancel, reorder, and recovery after a crash. A file whose upload outcome is unknown after a crash is never resent automatically; you decide.
- The core is free. An optional one-time purchase, squeezegram Premium, adds a few conveniences (see below).

Windows 10 or 11, x64 only.

## Download

Get the latest version from the [Releases page](https://github.com/yaniswav/squeezegram/releases):

- **Installer** (`squeezegram-<version>-setup-x64.exe`): Inno Setup installer for the current user, with no administrator rights needed for the app itself. The app downloads a new version only when you press "Download and install". Microsoft WebView2 Runtime is required; the installer runs Microsoft's bootstrapper if it is missing.
- **Portable zip** (`squeezegram-<version>-portable-x64.zip`): unzip anywhere and run `squeezegram.exe`. It does not update itself; it only shows a link to the release page when a newer version exists.

Each release lists the SHA-256 of both files in `SHA256SUMS.txt`. To check what you downloaded:

```powershell
Get-FileHash .\squeezegram-<version>-setup-x64.exe -Algorithm SHA256
```

If the hash differs from the one in `SHA256SUMS.txt`, do not run the file.

If you used an earlier NSIS installer, uninstall that version from Windows Settings without selecting "Delete the application data", then run the new installer. The Inno installer refuses to install while the old NSIS installation is registered. Updates between Inno installations keep the same installation folder.

### SmartScreen and antivirus

The executable is not code-signed. Windows SmartScreen may show "Windows protected your PC": if you trust the file after checking its hash, choose "More info", then "Run anyway". Some antivirus programs may flag the executable or `tdjson.dll` (the Telegram library); unsigned software often triggers false positives. The source is public, so you can build it yourself (see the end of this page).

## First launch

1. **ffmpeg.** squeezegram needs ffmpeg to compress. Press "Download ffmpeg (LGPL)": it downloads one pinned LGPL build from the BtbN FFmpeg-Builds release on GitHub, checks its SHA-256, and installs it. You can also point it at an ffmpeg you already have ("Use my own ffmpeg").
2. **Sign in to Telegram** with a QR code (scan it from Telegram on your phone) or with your phone number.
3. Add files, choose the destination chat or topic, and send.

## What it does

Free:

- Compression to just under the upload limit, with automatic encoder choice (graphics card first, processor as fallback).
- Sending to chats, groups and forum topics, with caption templates (variables are listed in the app), thumbnails, and a minimum interval between sends set per destination.
- Duplicate detection: a file already sent to a destination waits for your confirmation, with a link to the earlier message.
- Removal of location and camera data (GPS position, camera serial numbers, editing history) from photos, and the recorded location of compliant MP4 videos, when they are sent without re-encoding. HEIC files are sent unchanged.
- Proxy support (SOCKS5, HTTP, MTProto).
- The option to use your own Telegram `api_id` (from <https://my.telegram.org>) instead of squeezegram's.
- Automatic updates (installer) or a portable build.
- Seven interface languages: English, plus French, Russian, Spanish, Arabic, Brazilian Portuguese and Indonesian. The six others are machine translations not yet reviewed by a native speaker, and are marked "beta" in the language list.

### Free and Premium

squeezegram Premium is a lifetime licence bought once in Telegram Stars from `@squeezegram_bot` on Telegram. The key is tied to your Telegram account and checked on your computer, with no network request. It unlocks every Premium feature, including those added in later versions. It is unrelated to Telegram Premium, which only raises the upload limit.

| | Free | squeezegram Premium |
| --- | --- | --- |
| Compression and sending | yes | yes |
| Caption templates, thumbnails, duplicate detection, metadata removal | yes | yes |
| Proxy, own `api_id`, languages, updates | yes | yes |
| Video codec (H.264 or HEVC), encoder speed, target size, maximum resolution, audio bitrate, per-file settings | automatic values | adjustable |
| Send one batch to several chats or topics | one destination per batch | several |
| Watched folders (new files wait in the queue for your approval, never sent automatically) | no | yes |
| Background mode (notification-area icon, start with Windows) | no | yes |

## Privacy

squeezegram has no analytics or telemetry and no server of its own. Your files stay on your computer until you choose to send them. Settings, the queue and the licence key are stored in `%APPDATA%\squeezegram`; the Telegram session, ffmpeg and logs are stored in `%LOCALAPPDATA%\squeezegram`. Temporary files use `%LOCALAPPDATA%\squeezegram\tmp` by default, or the temporary folder selected in Settings.

What leaves your computer:

- **To Telegram**, through TDLib (Telegram's own client library): what you choose to send, meaning the file (compressed or as is, with location and camera data removed if that option is on), its caption and its destination, plus the normal traffic of a Telegram client. Nothing is sent that you did not queue.
- **To GitHub**, to download ffmpeg on first setup.
- **To GitHub**, to look for a newer version: at startup at most once every 24 hours (Settings, Updates, "Check automatically" turns this off) and when you press "Check now". It is a plain HTTPS request for `latest.json`; GitHub sees your IP address, as for any download. No identifier, Windows version or usage data is sent.
- **To your proxy**, if you set one, which then carries all connections to Telegram.

During installation, Microsoft's WebView2 bootstrapper may download the WebView2 Runtime from Microsoft if it is missing.

The Telegram session is stored in an encrypted TDLib database whose key is protected with Windows DPAPI. The local journal is kept for 7 days at most, never contains your phone number, codes, passwords, e-mail address or captions, and is exported only if you create a debug report yourself.

Uninstall squeezegram from Windows Settings (Apps). The Inno uninstaller removes the application but always keeps its data, as do updates. There is no data-deletion checkbox. To remove the remaining data yourself after uninstalling and closing every copy of the app, delete only `%LOCALAPPDATA%\squeezegram` (Telegram session, logs, ffmpeg and temporary files) and `%APPDATA%\squeezegram` (queue, settings and licence key). This cannot be undone without a backup.

### A warning about Telegram

Sending messages to groups from a user account is subject to Telegram's terms. Repeated sends to groups where you lack permissions, or sending many files in a short time, can get an account limited or restricted by Telegram. squeezegram respects the waiting times Telegram asks for and offers a per-destination interval, but it cannot prevent a restriction. You use it at your own risk.

## Build from source

You do not need this to use squeezegram. Requirements:

- Windows 10 or 11, x64
- Node.js 22.12 or later (`.node-version`) and pnpm 10 (the exact version is pinned in `package.json`; `corepack enable` installs it)
- Rust 1.90 or later with the MSVC toolchain (Visual Studio Build Tools, "Desktop development with C++"); CI uses 1.99.0
- Microsoft Edge WebView2 Runtime (preinstalled on Windows 11)
- Microsoft Visual C++ Redistributable (x64), which `tdjson.dll` needs

### Build and run

```powershell
pnpm install
pnpm tauri dev
```

This starts the app with a mock transport that sends nothing to Telegram. To use the real Telegram transport:

1. Get the pinned `tdjson.dll` (TDLib 1.8.68, commit `c15d3f5a5de6e3ba5839822c451152e5e18bb700`, Windows x64, OpenSSL and zlib statically linked): either build it yourself from <https://github.com/tdlib/td>, or take the `tdjson.dll` asset of this repository's `tdjson-1.8.68` pre-release, which the release workflow also uses. Then install it. The script checks the SHA-256 of the pinned build and copies the DLL to `%LOCALAPPDATA%\squeezegram\tdlib\bin`; it never downloads anything:

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

Checks (the CI workflow in `.github/workflows/build.yml` runs all of them; for the Rust tests it runs `cargo test --locked --lib`):

```powershell
pnpm typecheck; pnpm lint; pnpm format:check
cd src-tauri; cargo fmt --check; cargo clippy --locked -- -D warnings; cargo test
```

### Local installer and portable zip

```powershell
cmd /c "start /low /wait /b pwsh -NoProfile -File scripts\build-release.ps1"
```

The script stages the pinned TDLib library, generates notices, builds the app, compiles the Inno installer and creates the portable zip and `SHA256SUMS.txt` in `dist-release`. It downloads and verifies the pinned Inno Setup compiler and Microsoft's WebView2 bootstrapper when needed. The compiler preparation does not install squeezegram. `-InstallerEngine Nsis` selects the older packaging path explicitly.

Do not publish these local outputs: the executable embeds the Telegram API identity supplied at build time, including values from your local `.env`. Public release artifacts are built by the release workflow with the intended release identity. Updater signatures are separate from Windows code signing; a local build without the updater signing environment has no `.sig` file.

## License

squeezegram is source-available under the [PolyForm Noncommercial License 1.0.0](LICENSE). You may use, modify and share it for noncommercial purposes as defined in that license. Commercial use requires a separate written agreement with the author: open an issue on this repository to ask.

Third-party components keep their own licenses: see [THIRD-PARTY-NOTICES.md](THIRD-PARTY-NOTICES.md). Releases include the full notices as `THIRD-PARTY-NOTICES.txt`, next to the executable.

## Contributing and security

See [CONTRIBUTING.md](CONTRIBUTING.md) and [SECURITY.md](SECURITY.md).
