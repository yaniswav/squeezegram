## What's new in {{VERSION}}

TODO: describe what is new in this release before publishing it.

## Downloads

- **`{{SETUP_NAME}}`** (recommended): Inno Setup installer for the current user, with no administrator rights needed for the app itself. Installed copies check for new versions and update themselves when you press "Download and install". An older NSIS installation must first be uninstalled without deleting its data. Inno updates and uninstallation preserve application data.
- **`{{ZIP_NAME}}`**: portable build. Unzip it anywhere you can write. It never updates itself: it only tells you when a new version exists, and you replace the folder yourself.

`SHA256SUMS.txt`, `latest.json` and the `.sig` file are for verification and for the updater. You do not need to download them.

Windows 10 or 11, x64. Microsoft WebView2 Runtime and the Microsoft Visual C++ Redistributable (x64) are required. The installer runs Microsoft's WebView2 bootstrapper if that runtime is missing. ffmpeg is not included: press "Download ffmpeg (LGPL)" during setup, or select your own ffmpeg.

## SHA-256

- `{{SETUP_NAME}}`: `{{SETUP_SHA256}}`
- `{{ZIP_NAME}}`: `{{ZIP_SHA256}}`

Check a download in PowerShell before running it, and compare the result with the value above or with `SHA256SUMS.txt`. If they differ, do not run the file.

```powershell
Get-FileHash .\{{SETUP_NAME}} -Algorithm SHA256
```

## These builds are not code-signed

squeezegram is not signed with a Windows code-signing certificate.

- Windows SmartScreen may show "Windows protected your PC". If you trust the file after checking its hash, choose "More info", then "Run anyway".
- Some antivirus programs may flag or quarantine `squeezegram.exe` or `tdjson.dll` (the Telegram library). Unsigned software can trigger false positives, but an alert is not proof that a file is safe. Verify the download and investigate the alert before running it. The source code is public, and you can build it yourself.
