#!/usr/bin/env bash
# Run MacFlow's complete non-interactive verification matrix and package the
# app. This script never installs or launches the bundle and does not trigger
# Accessibility or Screen Recording prompts.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
readonly SCRIPT_DIR
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
readonly REPO_ROOT

if [[ $# -ne 0 ]]; then
    echo "Usage: $0" >&2
    exit 2
fi

if [[ "$(uname -s)" != "Darwin" ]]; then
    echo "MacFlow verification requires macOS." >&2
    exit 1
fi

for tool in cargo codesign plutil otool nm; do
    if ! command -v "$tool" >/dev/null 2>&1; then
        echo "Required tool is unavailable: $tool" >&2
        exit 1
    fi
done

if ! cargo fmt --version >/dev/null 2>&1; then
    echo "The rustfmt component is required (rustup component add rustfmt)." >&2
    exit 1
fi
if ! cargo clippy --version >/dev/null 2>&1; then
    echo "The Clippy component is required (rustup component add clippy)." >&2
    exit 1
fi

cd "$REPO_ROOT"

echo "Checking Rust formatting..."
cargo fmt --all -- --check

run_feature_mode() {
    local label="$1"
    shift

    echo "Checking feature mode: $label"
    cargo check --all-targets --locked "$@"

    echo "Testing feature mode: $label"
    cargo test --all-targets --locked "$@"

    echo "Linting feature mode: $label"
    cargo clippy --all-targets --locked "$@" -- -D warnings
}

run_feature_mode "default"
run_feature_mode "all features" --all-features
run_feature_mode "no default features" --no-default-features

echo "Building and validating the release app bundle..."
./build.sh

echo "Verification passed for every feature mode and build/MacFlow.app."
