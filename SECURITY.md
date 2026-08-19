# Security Policy

## Supported versions

Until the first binary release, security fixes are made on `main`. Afterwards,
fixes are provided for the latest published release. If a report affects an
older version, first confirm whether it is reproducible on the latest source or
release.

## Reporting a vulnerability

Please do not disclose a suspected vulnerability in a public issue, discussion,
pull request, or social post.

Use the repository's **Security** tab and select **Report a vulnerability** to
open a private GitHub security advisory. Include:

- the affected MacFlow and macOS versions;
- the impact and conditions required to reproduce it;
- minimal reproduction steps or a proof of concept;
- whether Accessibility or Screen Recording permission is involved; and
- any suggested remediation, if known.

Remove personal data before attaching logs or images. In particular, do not
submit real window titles, screen captures, Apple IDs, signing certificates,
passwords, tokens, or local file paths unless they are essential and safely
redacted.

If private vulnerability reporting is temporarily unavailable, open a public
issue containing no sensitive details and ask a maintainer to establish a
private reporting channel. Do not include the vulnerability itself.

## Security and privacy boundaries

MacFlow is a local macOS utility. It needs Accessibility permission to
observe and focus windows. Screen Recording permission is optional and is used
only to render window thumbnails. Thumbnail pixels stay in a bounded in-memory
cache and are not saved or uploaded by the application.

MacFlow has no telemetry or background update client. **Check for Updates**
opens the project's GitHub Releases page in the default browser. Preferences
are stored locally using macOS user defaults.

The default non-App-Store build includes a guarded, runtime-resolved SkyLight
fallback for exact cross-Space focus. It can be disabled with
`MACFLOW_DISABLE_PRIVATE_SPACES=1`, and the `app-store` feature removes it
at compile time.

Only install release archives published by this repository. An official binary
release must be Developer ID signed and Apple notarized; the release workflow
fails rather than publish an ad-hoc-signed archive. There is no official
prebuilt release until the Releases page contains one.

The local data model and permission behavior are documented in
[PRIVACY.md](PRIVACY.md).
