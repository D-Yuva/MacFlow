## Summary

<!-- What changed, and what user problem does it solve? -->

## Verification

<!-- List automated and manual checks performed. -->

- [ ] `./scripts/verify.sh` passes.
- [ ] New behavior has regression coverage, or I explained why a test is not practical.
- [ ] I tested affected macOS interactions manually when applicable.

## Safety and performance

- [ ] The event-tap callback remains non-blocking and allocation-bounded.
- [ ] Capture, cache, queue, and retained-view bounds are preserved.
- [ ] Window switching still works without Screen Recording permission.
- [ ] Default, all-features, and no-default-features builds remain supported.
- [ ] I did not add secrets, personal data, private screenshots, or signing material.
- [ ] Any new dependency or private API use is explained below.

## Manual macOS matrix

<!-- Mark N/A where the change cannot affect an item. -->

- [ ] Multiple windows from the same app
- [ ] Multiple displays
- [ ] Fullscreen windows / Spaces
- [ ] Finder windows
- [ ] Accessibility permission missing and enabled
- [ ] Screen Recording permission missing and enabled
- [ ] Keyboard cancellation, quick switching, and chooser navigation
- [ ] N/A — no window-switching or platform behavior changed

## Additional notes

<!-- Dependencies, private APIs, screenshots with safe sample data, or follow-up work. -->
