set shell := ["bash", "-cu"]
set windows-shell := ["powershell.exe", "-NoLogo", "-NoProfile", "-Command"]

# Build the plugin binary in debug mode.
build:
    cargo build

# Build for release (what the GitHub Actions workflow ships).
release:
    cargo build --release

# Run unit tests. Excludes tests/live_db.rs (a live-database integration
# test requiring POSTGRES_PLUGIN_BIN and a running PostgreSQL instance —
# see the "Live PostgreSQL integration" CI job, or run it directly per
# tests/live_db.rs's own doc comment).
test:
    cargo test --lib --bins

# Launch the local REPL that simulates Tabularis JSON-RPC calls over stdio.
repl:
    cargo run --bin test_plugin

# Run clippy with warnings denied.
lint:
    cargo clippy --all-targets -- -D warnings

# Format the codebase.
fmt:
    cargo fmt --all

# Start a disposable PostgreSQL container for local testing
# (user: postgres, password: password, database: testdb).
demo-db:
    docker run -d --name tabularis-postgres-demo -p 5432:5432 -e POSTGRES_PASSWORD=password -e POSTGRES_DB=testdb postgres:16-alpine

demo-db-stop:
    docker rm -f tabularis-postgres-demo

# ---------------------------------------------------------------------------
# Platform-specific install recipes (plugin-dir conventions per OS).
# ---------------------------------------------------------------------------

[linux]
dev-install: build
    mkdir -p ~/.local/share/tabularis/plugins/postgresql
    cp target/debug/postgresql-plugin ~/.local/share/tabularis/plugins/postgresql/
    cp .tabularium ~/.local/share/tabularis/plugins/postgresql/
    @echo "Installed to ~/.local/share/tabularis/plugins/postgresql"
    @echo "Restart Tabularis (or toggle the plugin in Settings) to pick up changes."

[macos]
dev-install: build
    mkdir -p "$HOME/Library/Application Support/tabularis/plugins/postgresql"
    cp target/debug/postgresql-plugin "$HOME/Library/Application Support/tabularis/plugins/postgresql/"
    cp .tabularium "$HOME/Library/Application Support/tabularis/plugins/postgresql/"
    @echo "Installed to ~/Library/Application Support/tabularis/plugins/postgresql"
    @echo "Restart Tabularis (or toggle the plugin in Settings) to pick up changes."

# Each recipe line runs in a fresh shell, so this must be one logical command.
# Tabularis resolves its plugin dir via the `directories` crate
# (ProjectDirs "", "", "tabularis"), which on Windows is %APPDATA%\tabularis.
[windows]
dev-install: build
    $dest = Join-Path $env:APPDATA "tabularis\plugins\postgresql"; \
    New-Item -ItemType Directory -Force -Path $dest | Out-Null; \
    Copy-Item "target\debug\postgresql-plugin.exe" $dest; \
    Copy-Item ".tabularium" $dest; \
    Write-Host "Installed to $dest"; \
    Write-Host "Restart Tabularis (or toggle the plugin in Settings) to pick up changes."

[linux]
uninstall:
    rm -rf ~/.local/share/tabularis/plugins/postgresql

[macos]
uninstall:
    rm -rf "$HOME/Library/Application Support/tabularis/plugins/postgresql"

[windows]
uninstall:
    $dest = Join-Path $env:APPDATA "tabularis\plugins\postgresql"; \
    if (Test-Path $dest) { Remove-Item -Recurse -Force $dest }
