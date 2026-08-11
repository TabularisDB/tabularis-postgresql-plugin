# CI hardening — deferred items

**Status:** not implemented; documented here so the ideas aren't lost.

While setting a higher CI bar than the sibling plugin repos (see the
`ci.yml`/`release.yml` history around the manifest-validation, markdownlint,
release-binary-smoke-test, `cargo audit`, and live-database-integration-test
additions), two further items were identified and deliberately deferred
rather than implemented immediately:

## `dependency-review-action` on PRs

[`actions/dependency-review-action`](https://github.com/actions/dependency-review-action)
flags newly-introduced vulnerable or license-incompatible dependencies
**in the diff of a specific PR**, rather than scanning the whole dependency
tree. This is complementary to, not a replacement for, the `cargo audit`
job already added: `cargo audit` catches every known vulnerability in the
full tree (including ones that predate the PR and ones newly disclosed
against unchanged deps, via its weekly schedule run), while
`dependency-review-action` is specifically useful for catching "this PR
just added a bad dependency" as an inline PR check before merge.

Not implemented yet because `cargo audit` already covers the core
supply-chain-vulnerability need for a single-crate Rust repo this size;
the PR-diff-specific framing adds most value on repos with frequent
dependency churn or many contributors, which doesn't describe this repo's
current state.

## SBOM generation (`cargo-cyclonedx`)

Publishing a [CycloneDX](https://cyclonedx.org/) SBOM (Software Bill of
Materials) alongside each GitHub release, generated via
[`cargo-cyclonedx`](https://github.com/CycloneDX/cyclonedx-rust-cargo),
would let downstream consumers (or automated scanners) inspect this
plugin's exact dependency tree per release without needing to check out
the tagged commit and inspect `Cargo.lock` themselves.

Not implemented yet because it's genuinely ahead of the curve for this
repo's current maturity — no consumer has asked for it, no sibling plugin
in the org does this, and it doesn't close a gap the way the other four
additions do. Worth revisiting once this repo is signed off as the
primary plugin home (see `CLAUDE.md`'s "Status" section) and/or once
there's an actual downstream consumer or compliance requirement asking
for it.
