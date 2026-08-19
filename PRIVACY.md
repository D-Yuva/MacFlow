# Privacy

MacFlow is a local macOS utility. It has no telemetry, analytics, crash
reporting, advertising SDK, account system, or background network client.
MacFlow does not upload or sell data.

## Data processed locally

- The global shortcut listener receives keyboard modifier changes and key-down
  events. It recognizes only the configured shortcut and navigation keys;
  unrelated events are passed through and are not logged or stored.
- Accessibility APIs provide process IDs, window IDs, titles, positions, sizes,
  and state needed to list and focus windows. This metadata is held only in
  ephemeral in-memory state and is not written to disk.
- If Screen Recording is enabled, ScreenCaptureKit provides images for chooser
  thumbnails. Pixels remain in an 8 MiB in-memory cache and are discarded as
  windows change or the app exits. They are never saved or uploaded.
- Preferences such as the shortcut, reveal delay, display placement, and
  thumbnail policy are stored in macOS user defaults.

## Permissions

- **Accessibility** is required for the global shortcut and exact-window
  discovery and focus.
- **Screen Recording** is optional. Window switching works without it by using
  application icons. MacFlow requests this permission only after the user
  chooses **Enable Screen Recording…** from the menu-bar item.

Permission status is checked locally so the menu can show setup guidance. A
permission can be revoked at any time in macOS System Settings.

## User-initiated external actions

- **Check for Updates** opens the project's GitHub Releases page in the default
  browser. MacFlow itself does not contact GitHub or download updates.
- **Copy Diagnostics** writes a diagnostics report to the clipboard only when
  selected. The report includes app/system versions, settings, permission
  states, timings, and bounded-cache counts. It excludes window titles and
  thumbnail pixels. Review clipboard contents before sharing them.

GitHub applies its own privacy terms when someone visits the repository, opens
an issue, or downloads a release. Those services are outside the application.

## Build and contributor data

The public source tree does not include signing certificates, credentials,
local analysis state, build outputs, or absolute developer paths. Official
source archives and releases must be produced from a clean Git checkout; do
not redistribute a ZIP of a contributor's working directory.

Privacy regressions can be reported through the process in [SECURITY.md](SECURITY.md).
