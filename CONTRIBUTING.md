# Contributing to MacFlow

Thanks for helping improve MacFlow. Bug reports, focused performance work,
accessibility improvements, documentation, and well-tested fixes are welcome.

By contributing, you agree that your contribution may be distributed under
the repository's [MIT License](LICENSE). Please also follow the
[Code of Conduct](CODE_OF_CONDUCT.md).

## Before you start

- Search existing issues and pull requests before opening a duplicate.
- Use an issue to discuss large behavior changes before investing in an
  implementation.
- Never include window titles, screenshots, signing certificates, Apple
  credentials, crash reports with personal paths, or other private data in an
  issue or commit. Redact diagnostics before posting them when necessary.
- Report security-sensitive findings using [SECURITY.md](SECURITY.md), not a
  public issue.

## Development setup

MacFlow requires macOS 14 or newer, the Xcode command line tools, and Rust
1.88 or newer with `rustfmt` and Clippy. `rust-toolchain.toml` selects the
declared minimum toolchain for rustup-based installations.

```sh
xcode-select --install
rustup toolchain install stable --component rustfmt,clippy
git clone https://github.com/D-Yuva/MacFlow.git
cd MacFlow
./build.sh
```

The app bundle is written to `build/MacFlow.app`. Building does not install
or launch it. Run `./build.sh install` only when you want to replace the copy
in `/Applications` and launch the new build.

An ad-hoc signature is enough for local development. For more stable macOS
permission grants, create an Apple Development certificate through Xcode as
described by `./setup_cert.sh`. Never commit certificates or passwords.

## Repository layout

- `src/core/` contains bounded state, ordering, layout, cache, and input logic.
- `src/platform/` contains the macOS AppKit, Accessibility, event-tap,
  ScreenCaptureKit, and window-focus integration.
- `Resources/` contains the packaged app metadata and assets.
- `scripts/verify.sh` runs the complete non-interactive verification matrix.

## Making a change

1. Create a short branch from `main`.
2. Keep changes focused and preserve the app's bounded-memory design.
3. Add regression tests for behavior changes and edge cases.
4. Run the full verifier before opening a pull request:

   ```sh
   ./scripts/verify.sh
   ```

5. Fill out the pull-request template, including manual macOS checks for UI,
   permissions, fullscreen Spaces, or global-shortcut changes.

For quick iteration, the individual checks are:

```sh
cargo fmt --all -- --check
cargo check --all-targets --locked
cargo test --all-targets --locked
cargo clippy --all-targets --locked -- -D warnings
```

The full verifier additionally runs all-features and no-default-features modes,
then builds and validates the signed app bundle.

## Project invariants

Changes should keep these properties intact:

- The event-tap callback does no UI, Accessibility, capture, or blocking work.
- Capture work remains bounded and optional; switching works without Screen
  Recording permission.
- Window identity and MRU order remain stable when several windows belong to
  the same app.
- Fullscreen and cross-Space behavior fails safely when private APIs are
  unavailable or disabled.
- Features must work with both default features and `--no-default-features`.
- The deployment target remains macOS 14 unless a version change is discussed
  and documented.

## Dependencies and private APIs

Keep dependencies minimal and explain any new dependency in the pull request.
Commit `Cargo.lock` changes together with dependency changes.

The default build contains a guarded, runtime-resolved SkyLight fallback for
exact cross-Space focus. Do not broaden private API use. The `app-store`
feature must continue to compile it out and preserve the public Accessibility
fallback.
