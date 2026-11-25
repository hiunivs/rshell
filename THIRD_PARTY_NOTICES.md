# Third-party notices

RShell contains or distributes third-party software and assets. Those components remain under their own licenses; the RShell dual-license terms do not replace them.

## Embedded assets

- **Noto Sans SC Regular 2.004 subset** — Copyright 2014-2021 Adobe; licensed under the SIL Open Font License 1.1. The complete notice and license are in [`assets/fonts/OFL.txt`](assets/fonts/OFL.txt), with subset details in [`assets/fonts/README.md`](assets/fonts/README.md).
- **Lucide icons v0.468.0** — Copyright Lucide Contributors 2022; licensed under the ISC License. The complete notice and license are in [`assets/icons/lucide/LICENSE`](assets/icons/lucide/LICENSE).

## Rust dependencies

The complete dependency report for the exact locked dependency graph is generated as `THIRD_PARTY_LICENSES.html` with `cargo-about`. Release packages must contain that generated report. The generation policy and template are stored in [`about.toml`](about.toml) and [`about.hbs`](about.hbs).

## Platform update framework

The macOS package embeds **Sparkle 2**, which is licensed separately by the Sparkle Project. The release process copies the `LICENSE` file from the pinned and checksum-verified Sparkle distribution into the application bundle. RShell does not embed Sparkle in Windows packages or development `cargo run` builds.

This file is a convenience index and is not a substitute for the complete license texts included with a release.

