# Phase 3 — Deprecate Built-in Driver (Deferred Decision)

**Goal:** Remove the built-in PostgreSQL driver from Tabularis core, making the
plugin the sole PostgreSQL driver. This is a strategic decision that requires
full team consensus and community readiness.

**Status:** Deferred until Phases 1 and 2 are complete and proven in production.

---

## When to Revisit This Decision

This phase should be discussed when ALL of the following are true:

- [ ] Plugin has been in stable release for at least 2 months
- [ ] No critical bug reports from plugin users
- [ ] Plugin test suite has 100% parity with built-in (Phase 1 proven)
- [ ] Plugin exceeds built-in in features (Phase 2 shipped)
- [ ] Community feedback is positive (no "I want the old driver back" sentiment)
- [ ] Performance benchmarks show no meaningful regression
- [ ] All supported platforms (macOS, Linux, Windows) confirmed working

---

## Decision Points

The team must agree on:

### 1. Plugin ID Strategy

| Option | Effort | User Impact |
| ------ | ------ | ----------- |
| Rename plugin to `"postgres"` (remove guard) | Low — one code change | Zero — saved connections work unchanged |
| Keep `"postgres-plugin"`, add migration UI | Medium — migration dialog + saved connection rewrite | Low — one-time dialog on update |
| Keep both (built-in frozen, plugin recommended) | Zero | Confusing — two PG drivers visible |

### 2. Bundling Strategy

| Option | Pros | Cons |
| ------ | ---- | ---- |
| Bundle plugin in app distribution | No install step, guaranteed availability | Larger app binary |
| Auto-install from registry on first launch | Smaller app, always latest version | Requires internet, extra startup time |
| Manual install (user must add via Settings) | Simplest for us | Worst UX, many users won't discover it |

### 3. Built-in Driver Removal Scope

What gets removed from `src-tauri/`:

- `drivers/postgres/` (2420+ lines — mod.rs, binding.rs, client.rs, explain.rs, helpers.rs, types.rs, extract/)
- PostgreSQL pool creation in `pool_manager.rs`
- `"postgres"` entry in `BUILTIN_DRIVER_IDS`
- PostgreSQL-specific code in `commands.rs` (SSH expansion for PG, postgres_dbname helper)

What stays:

- The `DatabaseDriver` trait (used by all drivers)
- RPC infrastructure (used by all plugins)
- Frontend driver capability handling (generic, works with any driver)

### 4. Rollback Plan

If something goes wrong after removing the built-in driver:

- **Short-term:** Users can install the last Tabularis version with built-in PG
- **Medium-term:** We can re-add the built-in driver in a patch release (code is in git history)
- **Plugin-side:** If the plugin has a bug, push a new plugin version (no app update needed)

---

## Implementation Steps (When Decided)

1. Remove `"postgres"` from `BUILTIN_DRIVER_IDS` array
2. If renaming plugin: update `.tabularium` manifest `id` to `"postgres"`
3. Remove `src-tauri/src/drivers/postgres/` directory
4. Remove PG pool logic from `pool_manager.rs`
5. Remove `postgres_dbname()` helper from `commands.rs`
6. Add migration logic: on first launch after update, if no `"postgres"` driver is
   registered, auto-install the plugin (or prompt user)
7. Update frontend: remove PG-specific fallback capabilities in `useDrivers.ts`
8. Update documentation: migration guide for users
9. Update CHANGELOG: announce the change prominently
10. Test: full regression suite against plugin-only configuration

---

## Checkpoint: CP-6

**When:** Team decides to proceed with deprecation.

**Stakeholders:** Full team consensus required — not a solo decision.

**Criteria for proceeding:**

- [ ] All items in "When to Revisit" section are satisfied
- [ ] Team has unanimously agreed on plugin ID strategy
- [ ] Team has agreed on bundling strategy
- [ ] Rollback plan is documented and tested
- [ ] Migration path verified with test accounts (saved connections survive)
- [ ] Community announcement drafted

---

## Security Consideration

Removing the built-in driver means all PostgreSQL connections flow through the
plugin process (a separate child process communicating via stdio). This changes
the security boundary:

| Concern | Built-in | Plugin |
| ------- | -------- | ------ |
| Credential handling | In-process, same memory space | Sent via JSON over stdio (local pipes) |
| Connection lifetime | Managed by Tabularis process | Managed by plugin process (kill_on_drop) |
| Crash isolation | PG driver crash = Tabularis crash | PG driver crash = error message (Tabularis survives) |
| Code audit surface | Part of main codebase | Separate binary (must be audited separately) |

The security posture is **slightly better** with the plugin (crash isolation)
but introduces a **new trust boundary** (the plugin binary must be verified
as legitimate at install time — already handled by registry SHA-256 verification).

---

## Timeline Estimate

This phase is purely a coordination and removal exercise. Technical effort is
minimal (< 1 week). The real timeline is governed by:

- Community confidence building (2+ months of stable plugin usage)
- Team scheduling for the migration release
- Documentation and announcement preparation

---

## Definition of Done

- [ ] Built-in PG driver code removed from Tabularis core
- [ ] Plugin is the sole PG driver, working identically
- [ ] Existing saved connections work without user action (or clear migration dialog)
- [ ] No user-facing regressions reported within 2 weeks of release
- [ ] CHANGELOG + migration guide published
- [ ] CP-6 sync completed with full team
