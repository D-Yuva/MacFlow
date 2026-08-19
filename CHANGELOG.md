# Changelog

All notable changes to MacFlow are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project follows [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## Unreleased

### Added

- Configurable quick-switch reveal timing, thumbnail policy, and chooser
  display placement.
- Grid navigation, pointer selection, double-click activation, app grouping,
  window-state badges, and accessibility improvements.
- Launch at Login, pause, diagnostics, and release-checking menu actions.
- CI verification across default, all-features, and no-default-features builds.
- Fail-closed signed and notarized GitHub release automation.

### Changed

- Reworked window ordering around stable per-window MRU identity and live focus
  tracking so windows from the same app remain distinct.
- Moved thumbnail refresh to a bounded, event-driven capture pipeline with
  stale-while-revalidate behavior and selectable capture scope.
- Improved exact-window switching across fullscreen Spaces and multiple
  displays while retaining safe public-API fallbacks.
- Reduced chooser latency, idle work, and transient thumbnail allocations.

### Fixed

- Incorrect or stale thumbnails after browser tab changes.
- Missing, reordered, or misfocused windows when several windows belong to the
  same application.
- Finder selections that could create or replace tabs instead of focusing the
  chosen window.
- Small-window preview alignment and partially visible rows during scrolling.

The first public release has not been tagged yet. This changelog intentionally
does not link to versions or release assets that do not exist.
