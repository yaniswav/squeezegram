# Security policy

## Supported versions

squeezegram is in alpha and has no release. Only the latest commit on the `main` branch is supported.

## Reporting a vulnerability

Do not open a public issue for a security problem. Use GitHub's private vulnerability reporting: open the Security tab of this repository and choose "Report a vulnerability". Only the author sees the report.

Include the affected version or commit, steps to reproduce, and the impact you observed. Do not include your Telegram credentials, session files or personal media in the report; the debug export (Settings › Diagnostics) masks paths and chat identifiers, but review it before sending.

This is a one-person project and no response time is guaranteed. Please give the author a reasonable delay to fix the problem before you disclose it publicly.

## Scope

In scope: the code in this repository, including the handling of the Telegram session, the TDLib database key, the downloaded FFmpeg binaries and the log files.

Out of scope: vulnerabilities in TDLib, FFmpeg, Tauri, WebView2 or other third-party components, which should go to their own maintainers. Tell us as well if squeezegram's use of them makes the problem worse.
