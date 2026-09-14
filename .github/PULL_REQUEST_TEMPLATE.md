## Summary

Briefly explain the goal of this pull request, the bug it fixes, or the feature it adds.

## Motivation & Context

Why is this change required? What problem does it solve? If it fixes an open issue, link to it: Fixes #

## Changes Made

- Detail the specific changes made in each crate or subsystem.

## Testing & Verification

- [ ] `cargo test --workspace` passes cleanly.
- [ ] `cargo fmt --all -- --check` passes with no discrepancies.
- [ ] `cargo clippy --workspace --all-targets -- -D warnings` reports no warnings.
- [ ] Added unit tests covering the new behavior / regression case.
- [ ] Differential tests against Node.js (`pwsh difftest/run.ps1`) pass (if applicable).
