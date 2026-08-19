#!/usr/bin/env bash
# Usage:
#   ./build.sh          Build, package, sign, and verify locally.
#   ./build.sh install  Also replace /Applications/MacFlow.app and launch it.
#   ./build.sh clean    Remove Cargo and packaged build artifacts.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
readonly SCRIPT_DIR
cd "$SCRIPT_DIR"

readonly APP_NAME="MacFlow"
readonly BUILD_DIR="$SCRIPT_DIR/build"
readonly APP_BUNDLE="$BUILD_DIR/$APP_NAME.app"
readonly BUNDLE_BINARY="$APP_BUNDLE/Contents/MacOS/$APP_NAME"
readonly INSTALL_PATH="/Applications/$APP_NAME.app"
readonly ACTION="${1:-build}"
readonly BUILD_ARCHS="${MACFLOW_BUILD_ARCHS:-native}"

if [[ "$ACTION" == "clean" ]]; then
    cargo clean
    if [[ -d "$BUILD_DIR" && "$BUILD_DIR" == "$SCRIPT_DIR/build" ]]; then
        rm -rf -- "$BUILD_DIR"
    fi
    echo "Cleaned Cargo and packaged build artifacts."
    exit 0
fi

if [[ "$ACTION" != "build" && "$ACTION" != "install" ]]; then
    echo "Usage: $0 [build|install|clean]" >&2
    exit 2
fi

echo "Building optimized Rust binary for macOS 14.0+..."
# Rust panic-location strings can otherwise include the builder's home path
# through dependency sources in Cargo's registry. Encode each flag separately
# so paths containing spaces remain valid and public binaries disclose neither
# the workspace location nor the local account name.
readonly FLAG_SEPARATOR=$'\x1f'
readonly CARGO_DATA_HOME="${CARGO_HOME:-${HOME:?}/.cargo}"
encoded_rustflags="${CARGO_ENCODED_RUSTFLAGS:-}"
for remap in \
    "--remap-path-prefix=$SCRIPT_DIR=/workspace" \
    "--remap-path-prefix=$CARGO_DATA_HOME=/cargo"; do
    if [[ -n "$encoded_rustflags" ]]; then
        encoded_rustflags+="$FLAG_SEPARATOR"
    fi
    encoded_rustflags+="$remap"
done
export CARGO_ENCODED_RUSTFLAGS="$encoded_rustflags"

case "$BUILD_ARCHS" in
    native)
        cargo build --release --locked
        readonly SOURCE_BINARY="$SCRIPT_DIR/target/release/macflow"
        ;;
    universal)
        if ! command -v rustup >/dev/null 2>&1; then
            echo "Error: universal builds require rustup to install both macOS targets." >&2
            exit 1
        fi
        for target in aarch64-apple-darwin x86_64-apple-darwin; do
            if ! rustup target list --installed | grep -Fxq "$target"; then
                echo "Error: missing Rust target '$target'." >&2
                echo "Install both targets with: rustup target add aarch64-apple-darwin x86_64-apple-darwin" >&2
                exit 1
            fi
            cargo build --release --locked --target "$target"
        done
        mkdir -p "$BUILD_DIR"
        readonly SOURCE_BINARY="$BUILD_DIR/$APP_NAME-universal"
        lipo -create \
            "$SCRIPT_DIR/target/aarch64-apple-darwin/release/macflow" \
            "$SCRIPT_DIR/target/x86_64-apple-darwin/release/macflow" \
            -output "$SOURCE_BINARY"
        ;;
    *)
        echo "Error: MACFLOW_BUILD_ARCHS must be 'native' or 'universal'." >&2
        exit 2
        ;;
esac

if [[ -d "$APP_BUNDLE" && "$APP_BUNDLE" == "$SCRIPT_DIR/build/MacFlow.app" ]]; then
    rm -rf -- "$APP_BUNDLE"
fi
mkdir -p "$APP_BUNDLE/Contents/MacOS" "$APP_BUNDLE/Contents/Resources"
install -m 755 "$SOURCE_BINARY" "$BUNDLE_BINARY"
install -m 644 "Resources/Info.plist" "$APP_BUNDLE/Contents/Info.plist"
install -m 644 "Resources/AppIcon.icns" "$APP_BUNDLE/Contents/Resources/AppIcon.icns"
install -m 644 "Resources/Assets.car" "$APP_BUNDLE/Contents/Resources/Assets.car"
install -m 644 "LICENSE" "$APP_BUNDLE/Contents/Resources/LICENSE.txt"
install -m 644 "PRIVACY.md" "$APP_BUNDLE/Contents/Resources/PRIVACY.md"
install -m 644 "THIRD_PARTY_NOTICES.md" \
    "$APP_BUNDLE/Contents/Resources/THIRD_PARTY_NOTICES.md"

plutil -lint "$APP_BUNDLE/Contents/Info.plist"
for plist_key in NSScreenCaptureUsageDescription NSHumanReadableCopyright; do
    if ! /usr/libexec/PlistBuddy -c "Print :$plist_key" \
        "$APP_BUNDLE/Contents/Info.plist" >/dev/null; then
        echo "Error: packaged Info.plist is missing $plist_key." >&2
        exit 1
    fi
done

signing_identity="${MACFLOW_SIGN_IDENTITY:-}"
if [[ -z "$signing_identity" ]]; then
    signing_identity=$(security find-identity -v -p codesigning 2>/dev/null \
        | sed -n 's/.*"\(Apple Development:[^"]*\)".*/\1/p' \
        | head -1 || true)
fi
if [[ -z "$signing_identity" ]]; then
    signing_identity="-"
    echo "Warning: no Apple Development identity found; using ad-hoc signing."
    echo "Accessibility/Screen Recording approvals may need to be granted again after rebuilds."
fi

echo "Signing packaged app."
signing_options=(--force --options runtime)
if [[ "$signing_identity" == Developer\ ID\ Application:* ]]; then
    signing_options+=(--timestamp)
else
    signing_options+=(--timestamp=none)
fi
codesign "${signing_options[@]}" --sign "$signing_identity" "$APP_BUNDLE"
codesign --verify --deep --strict --verbose=2 "$APP_BUNDLE"

min_versions=$(otool -l "$BUNDLE_BINARY" \
    | awk '$1 == "minos" { print $2 }' \
    | sort -u)
if [[ "$min_versions" != "14.0" ]]; then
    echo "Error: packaged binary has deployment target(s) '$min_versions', expected 14.0." >&2
    exit 1
fi
if grep -q 'CGWindowListCreateImage' < <(nm -u "$BUNDLE_BINARY"); then
    echo "Error: obsolete CGWindowListCreateImage is still linked." >&2
    exit 1
fi
if grep -Fq '/Users/' < <(strings -a "$BUNDLE_BINARY"); then
    echo "Error: packaged binary contains a local macOS home-directory path." >&2
    exit 1
fi
if [[ "$BUILD_ARCHS" == "universal" ]]; then
    binary_archs=$(lipo -archs "$BUNDLE_BINARY")
    if [[ "$binary_archs" != *arm64* || "$binary_archs" != *x86_64* ]]; then
        echo "Error: universal package is missing an architecture: $binary_archs" >&2
        exit 1
    fi
fi

echo "Verified $APP_BUNDLE ($(uname -m), macOS $min_versions+)."

if [[ "$ACTION" == "install" ]]; then
    pkill -x "$APP_NAME" 2>/dev/null || true
    for _ in {1..50}; do
        if ! pgrep -x "$APP_NAME" >/dev/null 2>&1; then
            break
        fi
        sleep 0.1
    done
    if pgrep -x "$APP_NAME" >/dev/null 2>&1; then
        echo "Error: an existing $APP_NAME process did not exit; installation was not changed." >&2
        exit 1
    fi
    readonly INSTALL_STAGE="/Applications/.MacFlow.app.install.$$"
    readonly INSTALL_BACKUP="/Applications/.MacFlow.app.backup.$$"
    if [[ -e "$INSTALL_STAGE" || -e "$INSTALL_BACKUP" ]]; then
        echo "Error: temporary installation path already exists." >&2
        exit 1
    fi
    cleanup_install_stage() {
        if [[ -e "$INSTALL_STAGE" && "$INSTALL_STAGE" == /Applications/.MacFlow.app.install.* ]]; then
            rm -rf -- "$INSTALL_STAGE"
        fi
    }
    trap cleanup_install_stage EXIT

    # Copy and verify the complete replacement before moving the installed app.
    ditto "$APP_BUNDLE" "$INSTALL_STAGE"
    codesign --verify --deep --strict "$INSTALL_STAGE"
    if [[ -d "$INSTALL_PATH" && "$INSTALL_PATH" == "/Applications/MacFlow.app" ]]; then
        mv "$INSTALL_PATH" "$INSTALL_BACKUP"
    fi
    if ! mv "$INSTALL_STAGE" "$INSTALL_PATH"; then
        if [[ -d "$INSTALL_BACKUP" ]]; then
            mv "$INSTALL_BACKUP" "$INSTALL_PATH"
        fi
        echo "Error: installation failed; the previous app was restored." >&2
        exit 1
    fi
    trap - EXIT
    if [[ -d "$INSTALL_BACKUP" && "$INSTALL_BACKUP" == /Applications/.MacFlow.app.backup.* ]]; then
        rm -rf -- "$INSTALL_BACKUP"
    fi
    open "$INSTALL_PATH"
    echo "Installed and launched $INSTALL_PATH"
else
    echo "Run './build.sh install' to explicitly install and launch it."
fi
