#!/usr/bin/env bash
# MacFlow deliberately does not create or trust a local root certificate.
# Use an Apple Development identity for stable local TCC permissions.
set -euo pipefail

if security find-identity -v -p codesigning 2>/dev/null \
    | grep -q '"Apple Development:'; then
    echo "An Apple Development code-signing identity is already available."
    security find-identity -v -p codesigning \
        | grep '"Apple Development:'
    exit 0
fi

cat <<'EOF'
No Apple Development signing identity was found.

Safe setup:
  1. Open Xcode > Settings > Accounts.
  2. Add your Apple ID and select your team.
  3. Choose Manage Certificates > + > Apple Development.
  4. Run ./build.sh again.

For a one-off local build, ./build.sh automatically uses ad-hoc signing. macOS
may ask you to re-grant Accessibility and Screen Recording after such rebuilds.
EOF
exit 1
