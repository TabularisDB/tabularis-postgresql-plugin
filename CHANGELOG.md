# Changelog

## [Unreleased]

### Added

- Repo scaffolding: `LICENSE` (Apache-2.0), `.gitignore`, `.editorconfig`,
  `CODEOWNERS`, `rust-toolchain.toml` (pinning `rustfmt`/`clippy`),
  `.github/dependabot.yml`, `justfile` (build/test/lint/fmt/dev-install/
  demo-db recipes, matching sibling plugin repos).
- `CI` workflow (`cargo build`/`test`/`clippy`/`fmt --check`) and `Release`
  workflow (cross-platform binary builds for Linux x86_64/aarch64, macOS
  x86_64/aarch64, Windows x86_64, published as GitHub release assets
  alongside `.tabularium`).
- `README.md` and `CLAUDE.md` describing the plugin's purpose, architecture,
  and current migration status.

The plugin source itself (`Cargo.toml`, `src/`, `.tabularium`) has not been
migrated from `TabularisDB/tabularis`'s `plugins/postgres-plugin/` yet — this
release holds only the repo-level basics staged ahead of that migration.
