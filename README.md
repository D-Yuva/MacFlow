# MacFlow

[![CI](https://github.com/D-Yuva/MacFlow/actions/workflows/ci.yml/badge.svg)](https://github.com/D-Yuva/MacFlow/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![macOS 14+](https://img.shields.io/badge/macOS-14%2B-black.svg)](#requirements)

MacFlow is a fast, memory-bounded, native Rust window switcher for macOS.
It switches individual windows—including several windows from the same app and
fullscreen windows on other Spaces—instead of stopping at the application
level.

## Requirements

MacFlow requires macOS 14 Sonoma or newer.

## Download and install

The source can be downloaded with Git or GitHub's **Code → Download ZIP**
button and built using the instructions below. A prebuilt download will appear
on the [Releases page](https://github.com/D-Yuva/MacFlow/releases) only after it
has been Developer ID signed, Apple notarized, and passed the automated release
verification.
There is no official prebuilt release yet.

After building, move `MacFlow.app` to `/Applications` or run the explicit
install command. MacFlow runs in the menu bar rather than the Dock. Open its
menu and choose **Open Accessibility Settings…** to grant the required
permission. Screen Recording is optional and is requested only when you choose
**Enable Screen Recording…** for live thumbnails.

## Use

The default shortcut is **Control + Option + Tab**:

- Tap and release to switch directly to the next recent window.
- Hold for 90 ms to reveal the full chooser.
- Press Tab or Shift + Tab while holding the modifiers to move through windows.
- Use the arrow keys for grid navigation, Return to confirm, or Escape to
  cancel.

The menu-bar **Shortcut** submenu can replace this with **Command + Tab** or
another Tab/Space combination. Choosing Command + Tab replaces the built-in
macOS application switcher while MacFlow is running. The chosen shortcut is
saved across launches.

## Permissions and privacy

MacFlow works locally and contains no telemetry or analytics. It does not
upload window metadata or thumbnail pixels, and it has no background network
client.

- **Accessibility** lets MacFlow listen for the global shortcut, discover
  window metadata, and focus the selected window.
- **Screen Recording** lets ScreenCaptureKit render thumbnails. Captured pixels
  remain in a bounded in-memory cache; the app does not save them to disk.
- **Check for Updates** opens this repository's Releases page in the default
  browser. It does not perform a background download.
- Preferences are stored locally in macOS user defaults.
- The global listener examines only shortcut/navigation key events; unrelated
  keyboard events are passed through and are not stored.

Thumbnail capture can be set to **Off** at any time. Window switching continues
to work and the chooser uses application icons instead. Permission status and
settings links are available from the menu-bar item.

See the full [privacy policy](PRIVACY.md) and [security policy](SECURITY.md).

## Preferences

The menu-bar item provides controls for:

- shortcut selection and instant quick switching;
- chooser reveal delay;
- **Off**, **Selected**, **Visible**, or **Fresh** thumbnail policies;
- chooser placement on the focused-window, pointer, or main display;
- temporary pause and Launch at Login;
- Accessibility and Screen Recording settings;
- update checking and a copyable diagnostics report.

Preferences are restored on launch. The diagnostics report contains versions,
settings, permission states, timings, counts, and cache usage—not window titles
or thumbnail images. Review it before sharing in case the surrounding clipboard
or report context contains information you consider private.

## Build from source

Install the current stable Rust toolchain (Rust 1.88 or newer) and Xcode command
line tools, then run:

```sh
git clone https://github.com/D-Yuva/MacFlow.git
cd MacFlow
./build.sh
```

The app bundle is written to `build/MacFlow.app`. The build script uses an
Apple Development identity when one is available and otherwise creates an
ad-hoc local signature. It never modifies `/Applications` unless you explicitly
run:

```sh
./build.sh install
```

Ad-hoc builds are suitable for development, but macOS may ask for Accessibility
and Screen Recording permission again after a rebuild. `./setup_cert.sh`
explains how to create an Apple Development identity safely through Xcode.

## Repository layout

```text
src/core/       Bounded state, window ordering, cache, layout, and input logic
src/platform/   AppKit, Accessibility, event-tap, capture, and focus integration
Resources/      Bundle metadata, compiled assets, and application icon
scripts/        Non-interactive verification tools
docs/           Maintainer and release documentation
.github/        CI, release automation, and contribution templates
```

The shipping application is implemented entirely in `src/`; obsolete prototype
implementations are intentionally excluded from the public tree.

## Resource bounds

- Thumbnail pixels use an 8 MiB byte-accounted LRU cache.
- Window-shaped capture canvases and centered aspect-fit rendering keep small,
  wide, and tall previews uncropped without increasing the cache budget.
- Only one ScreenCaptureKit screenshot and one pending batch are allowed at a
  time.
- A quick headless switch performs no screenshot capture; thumbnails begin only
  if the hold reveals the chooser.
- Each capture batch is capped at 12 thumbnails. The overlay retains only cards
  intersecting the viewport, at most 16 during a partial-row trackpad scroll in
  the maximum 1280 x 760 panel.
- The event-tap path performs no UI, Accessibility, capture, or unbounded queue
  work.
- Discovery is coalesced, cancellable, time-budgeted, and capped before render.
- The overlay is a compact 1280 x 760 maximum nonactivating panel rather than a
  full-display blur surface.
- macOS 26 uses the native Liquid Glass view. macOS 14–25 use a lightweight HUD
  material without changing the deployment floor.

## Cross-Space behavior

Exact cross-Space selection uses a fail-closed, runtime-resolved SkyLight
fallback on smoke-tested macOS major versions 14–26. Set
`MACFLOW_DISABLE_PRIVATE_SPACES=1` before launch to disable it.

Builds intended for the Mac App Store should use `--features app-store`, which
compiles that private fallback out and retains the public
Accessibility/application-focus path. Finder uses the same exact-window path:
selecting an existing Finder card does not open its folder URL, create a new
tab, or replace the tab already displayed in that window.

## Verification

```sh
./scripts/verify.sh
```

The verifier runs formatting, compilation, tests, and warning-free Clippy for
the default, all-features, and no-default-features configurations. It then
builds, signs, and validates `build/MacFlow.app`. It is non-interactive: it
does not install or launch the app or request macOS permissions.

The stress suite exercises hundreds of thousands of mailbox, cache, queue,
generation, and shutdown transitions while asserting fixed capacity and byte
budget invariants. GitHub Actions runs the same three-way Cargo matrix on every
pull request and push to `main`.

Tags such as `v1.4.0` run the Release workflow. The tag must match the bundle's
short version. Publishing requires a Developer ID Application certificate and
Apple notarization credentials configured in a protected release environment;
the workflow fails closed instead of distributing an ad-hoc build. It creates
a universal Intel/Apple-silicon app, timestamps, notarizes, staples,
Gatekeeper-checks, and publishes a SHA-256 checksum with the archive.
The complete maintainer checklist is in [docs/RELEASING.md](docs/RELEASING.md).

## Contributing

Please read [CONTRIBUTING.md](CONTRIBUTING.md) before opening a pull request.
Use the issue templates for reproducible bugs and focused feature proposals,
and follow the [Code of Conduct](CODE_OF_CONDUCT.md).

MacFlow is available under the [MIT License](LICENSE). Release history is in
[CHANGELOG.md](CHANGELOG.md), and dependency attributions are in
[THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md).
