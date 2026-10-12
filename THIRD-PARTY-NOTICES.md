# Third-party notices

squeezegram itself is licensed under the PolyForm Noncommercial License 1.0.0 (see `LICENSE`). It uses the third-party components below, each under its own license. Those licenses apply to those components, not the squeezegram license.

This file covers the native components, which no package manager tracks. The Rust crates and npm packages are listed by `scripts/generate-notices.ps1`, which reads `cargo metadata` and `pnpm licenses list`:

```powershell
pwsh -File scripts\generate-notices.ps1                 # inventory only
pwsh -File scripts\generate-notices.ps1 -IncludeTexts   # inventory + license texts, for a release
```

No release exists yet. Before the first one, the release pipeline must ship the generated file with license texts next to the binary, together with the full license texts of the native components listed here. This has not been done or checked yet.

## Rust crates under MPL-2.0

The following crates come in transitively through Tauri (`tauri-utils` → `dom_query` for the first four, `dirs` → `dirs-sys` for `option-ext`). They are used unmodified. Their source code is available at the upstream repositories:

| Crate            | Version | Source                                  |
| ---------------- | ------- | --------------------------------------- |
| cssparser        | 0.37.0  | https://github.com/servo/rust-cssparser |
| cssparser-macros | 0.7.1   | https://github.com/servo/rust-cssparser |
| selectors        | 0.38.0  | https://github.com/servo/stylo          |
| dtoa-short       | 0.3.5   | https://github.com/upsuper/dtoa-short   |
| option-ext       | 0.2.0   | https://github.com/soc/option-ext       |

Versions match `src-tauri/Cargo.lock` at the time of writing; rerun the script for the current list. All other Rust crates and npm production packages use permissive licenses (MIT, Apache-2.0, BSD, ISC, Zlib, Unicode-3.0, CC0-1.0, CDLA-Permissive-2.0, 0BSD, Unlicense), and `cargo deny check` enforces that allow-list (`src-tauri/deny.toml`).

## TDLib (`tdjson.dll`)

- Telegram Database Library, version 1.8.68. License: Boost Software License 1.0. Source: https://github.com/tdlib/td
- The pinned Windows build is compiled from the upstream sources (https://github.com/tdlib/td, commit c15d3f5a5de6e3ba5839822c451152e5e18bb700) and is installed for development by `scripts/install-tdjson.ps1`, which checks its SHA-256. The plan is to ship it inside the installer.
- TDLib includes SQLite with SQLCipher changes. SQLite is in the public domain; the SQLCipher changes are under the Zetetic BSD-style license (https://www.zetetic.net/sqlcipher/license/).
- Statically linked into that build (found as strings in the DLL):
  - OpenSSL 3.6.5, Apache License 2.0, https://www.openssl.org/source/license.html
  - zlib 1.3.2, zlib license, https://zlib.net/zlib_license.html
- The OpenSSL and zlib versions follow the DLL build: update this section whenever the DLL is rebuilt.

## SQLite (application database)

`rusqlite` is built with the `bundled` feature, which compiles SQLite into the application. SQLite is in the public domain (https://www.sqlite.org/copyright.html).

## FFmpeg

- squeezegram runs `ffmpeg.exe` and `ffprobe.exe` as separate processes. It does not link to FFmpeg libraries.
- The app downloads an LGPL build on first launch from the BtbN FFmpeg-Builds GitHub release pinned in `src-tauri/src/sidecars/` (LGPL v3, built without `--enable-gpl` or `--enable-nonfree`, no `libx264`). The download comes from BtbN's release, not from a copy hosted by this project.
- FFmpeg is licensed under the GNU Lesser General Public License; see https://ffmpeg.org/legal.html. Source code: https://ffmpeg.org/download.html, and the build scripts at https://github.com/BtbN/FFmpeg-Builds.
- That build links `libopenh264` (BSD-2-Clause, https://github.com/cisco/openh264) and other libraries under their own licenses; BtbN's build scripts list them.
- If this project ever hosts the FFmpeg binary itself (for example in a releases repository), it must also host the corresponding source archive in the same place.
- A user can point the app to an FFmpeg build they obtained themselves. The app never downloads a GPL build.

## Not distributed

- `tdl` (AGPL-3.0) is used only as a development tool and is not part of any build.
- The Microsoft Visual C++ runtime and WebView2 are required on the target machine; they are not redistributed by this repository.

## Open legal points

The license analysis in this file is a reading of the license texts by the developer, not legal advice. H.264 patent licensing for commercial distribution has not been reviewed. A legal review is planned before any commercial release.
