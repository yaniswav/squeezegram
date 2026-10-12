# Contributing

Bug reports and feature requests are welcome as GitHub issues. For a security problem, follow [SECURITY.md](SECURITY.md) instead of opening a public issue.

## Contributor license agreement

squeezegram is licensed under the PolyForm Noncommercial License 1.0.0, and the author also offers separate commercial licenses. To keep that possible, every external contribution (code, documentation, translations) requires a signed contributor license agreement (CLA) that gives the author the right to relicense the contribution, including under commercial terms.

The CLA is not written yet. Until it exists, pull requests from external contributors cannot be merged. If you want to contribute, open an issue first to discuss the change.

## Before you open a pull request

- Discuss larger changes in an issue first.
- Run the checks listed in the README ("Build and run") and make sure they pass.
- Keep one change per pull request, and explain why it is needed in the description.
- Never commit secrets: `.env`, Telegram API credentials, session files, signing keys or logs.
- Telegram tests on a real account go only to "Saved Messages", with small generated files.
