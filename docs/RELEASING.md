# Release process

Binary releases are intentionally fail-closed. Do not upload a locally built,
ad-hoc-signed application or a ZIP of a working directory.

## One-time repository setup

1. Enable private vulnerability reporting, Dependabot alerts and security
   updates, secret scanning, and push protection where the GitHub plan supports
   them.
2. Protect `main`: require the CI workflow, require pull requests, dismiss stale
   approvals, and block force pushes and deletion.
3. Create a protected GitHub environment named `release` with required
   maintainer approval.
4. Store these environment secrets:
   - `MACOS_CERTIFICATE_P12`
   - `MACOS_CERTIFICATE_PASSWORD`
   - `MACOS_KEYCHAIN_PASSWORD`
   - `NOTARY_APPLE_ID`
   - `NOTARY_TEAM_ID`
   - `NOTARY_APP_PASSWORD`

The certificate archive must contain a **Developer ID Application** identity.
An Apple Development or ad-hoc identity cannot be used for public distribution.
The certificate holder's signer name and Team ID are necessarily visible in a
signed download; use an organization account if individual identity disclosure
is unsuitable.

## Create a release

1. Update `Cargo.toml`, `Cargo.lock`, `Resources/Info.plist`, and
   `CHANGELOG.md` to the same version.
2. Run `./scripts/verify.sh` on macOS.
3. Merge through the protected `main` branch and wait for CI to pass.
4. Create a signed tag such as `v1.4.0` on that exact commit and push the tag.
5. Approve the protected Release workflow.
6. Verify the resulting GitHub release anonymously on an Intel Mac and an
   Apple-silicon Mac running a supported macOS version.

The workflow builds a universal binary, strips local build paths, checks the
minimum macOS version and obsolete APIs, imports signing material only for the
sign/notarize job, removes it before publication, staples the notarization
ticket, runs Gatekeeper assessment, and publishes the ZIP with a SHA-256 file.

## Verification commands

After download, a user can verify the checksum and Apple signature:

```sh
shasum -a 256 -c MacFlow-1.4.0-universal.zip.sha256
codesign --verify --deep --strict MacFlow.app
spctl --assess --type execute MacFlow.app
```

Never attach certificates, P12 files, notarization credentials, local logs, or
the ignored `target/`, `build/`, `.claude/`, or `.understand-anything/`
directories to an issue, artifact, or release.
