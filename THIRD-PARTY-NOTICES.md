# Third-party notices

squeezegram itself is licensed under the PolyForm Noncommercial License 1.0.0 (see `LICENSE`). It uses the third-party components below, each under its own license. Those licenses apply to those components, not to the squeezegram license.

## Where the notices are

The complete notices, with every license text, are in `THIRD-PARTY-NOTICES.txt`. It is generated at release time and shipped in the installer and in the portable zip, next to `squeezegram.exe`. It is not committed: it is built from the lock files, so it cannot drift from what is built.

```powershell
pwsh -File scripts\install-cargo-about.ps1   # once: the pinned cargo-about
pnpm install --frozen-lockfile               # once: the npm packages
pwsh -File scripts\generate-notices.ps1      # writes src-tauri\target\notices\THIRD-PARTY-NOTICES.txt
```

`scripts\build-release.ps1` runs the generation before bundling and stops if it fails. The build workflow (`build.yml`) runs it on every push, so a dependency without a usable license text fails the build instead of the release.

| Part                                                            | Source of the text                                                                      | Tool                                                              |
| --------------------------------------------------------------- | --------------------------------------------------------------------------------------- | ----------------------------------------------------------------- |
| TDLib, OpenSSL, zlib, SQLCipher/SQLite, WebView2 loader, FFmpeg | `notices/THIRD-PARTY-NOTICES.template.txt` and the license texts in `notices/licenses/` | checked in, edited by hand                                        |
| Rust crates compiled into `squeezegram.exe`                     | `src-tauri/Cargo.lock`                                                                  | cargo-about 0.9.2 (`src-tauri/about.toml`, `src-tauri/about.hbs`) |
| npm packages bundled into the interface                         | `pnpm-lock.yaml`                                                                        | `pnpm licenses list --prod` (`notices/npm.json`)                  |

The generation fails when a crate of the Windows build is missing from the Rust section, when a crate ships a `NOTICE` file, when a crate or package has no license text, or when a placeholder of the template is left. The accepted licenses in `about.toml` are the ones `src-tauri/deny.toml` allows, so `cargo deny check` and the notices agree.

## What it covers

- **tdjson.dll**: TDLib 1.8.68 (Boost 1.0), with OpenSSL 3.6.5 (Apache-2.0), zlib 1.3.2 (zlib license) and SQLite with the SQLCipher changes (Zetetic BSD-style license) linked statically.
- **squeezegram.exe**: the Rust crates of the Windows build (permissive licenses, plus five MPL-2.0 crates used unmodified, listed with their source in the notices), the bundled SQLite of `rusqlite` (public domain) and the static WebView2 loader (BSD-3-Clause, Microsoft).
- **The user interface**: the npm production packages, plus Tailwind CSS, whose styles end up in the stylesheet.
- **FFmpeg is not included.** When the user presses "Download ffmpeg (LGPL)", the app downloads one pinned, unmodified LGPL v3 build of BtbN FFmpeg-Builds, hosted on this project's `ffmpeg-n9.0.2-33` release (BtbN deletes its builds after about 14 days). This project is then the distributor: the same release holds the corresponding source (FFmpeg source and BtbN build scripts at the build's commits). The notices give the pinned URL, SHA-256 and both commits.
- `tdl` (AGPL-3.0) was only a development tool and is part of no build. The Microsoft Visual C++ runtime and the WebView2 Runtime are required on the target machine and are not redistributed.

## When to update it

- **tdjson.dll rebuilt**: change the versions and the commit in section 1 of the template (and the license text if the SQLCipher license changed). The OpenSSL and zlib versions are the ones found as strings in the DLL.
- **FFmpeg pin changed** (`src-tauri/src/sidecars/mod.rs`): change section 5 of the template (URL, size, SHA-256, FFmpeg commit, BtbN commit and tag) and the values in `scripts/mirror-ffmpeg.ps1`, then have the owner run that script to create the new hosting release.
- **cargo-about version**: change the pin in `scripts/install-cargo-about.ps1` only, regenerate and compare.
- **A package without a license file**: add its copyright holder to `notices/npm.json` (MIT only) after reading its repository.

## Open points

The license analysis here is a reading of the license texts by the developer, not legal advice. H.264 patent licensing for commercial distribution has not been reviewed. A legal review is planned before any commercial release.

The Inno installer and the portable zip include the generated file next to `tdjson.dll` and `squeezegram.exe`. The optional NSIS packaging path uses Tauri resources for those files. Before publishing a release, check that `THIRD-PARTY-NOTICES.txt` is present next to the executable in both artifacts.
