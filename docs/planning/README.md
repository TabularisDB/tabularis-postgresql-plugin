# PostgreSQL Plugin Migration — Phase Docs Index

**Master Plan:** [postgres-plugin-migration.md](./postgres-plugin-migration.md)

## Phase Documents

| Phase | Document | Status |
| ----- | -------- | ------ |
| Prerequisites | [00-prerequisites.md](./00-prerequisites.md) | ✅ Complete (PR #576) |
| Phase 0 | [01-phase-0-baseline-tests.md](./01-phase-0-baseline-tests.md) | ✅ Complete |
| Phase 1 | [02-phase-1-plugin-build.md](./02-phase-1-plugin-build.md) | 🟡 In Progress |
| Phase 2 | [03-phase-2-issue-16.md](./03-phase-2-issue-16.md) | Planning |
| Phase 3 | [04-phase-3-deprecate-builtin.md](./04-phase-3-deprecate-builtin.md) | Planning |

## Test Architecture

| Layer | Count | Purpose |
| ----- | ----- | ------- |
| Parity tests | 80 | Byte-perfect comparison: plugin output == builtin output |
| Baseline tests | 72 | Safety net: builtin behavior hasn't regressed |
| Golden tests | 26 | Snapshot drift detection |

See [02-phase-1-plugin-build.md](./02-phase-1-plugin-build.md) for why 80
parity tests (not 102) is the correct number for the CP-4 gate.

## Checkpoints & Release Gates

| Checkpoint | When | Stakeholders | Ship? |
| ---------- | ---- | ------------ | ----- |
| CP-1 | After Prerequisites merged | Core team review | ✅ Done |
| CP-2 | After Phase 0 complete | Core team + QA | ✅ Done (@aesslinger proceeded) |
| CP-3 | Phase 1 metadata parity (13/80) | Core team sync | ✅ Done (@aesslinger proceeded) |
| CP-4 | Phase 1 at 80/80 parity tests green | Core team + QA | **Yes — beta release** |
| CP-5 | After Phase 2 features complete | Core team + community | **Yes — stable release** |
| CP-6 | Phase 3 decision | Full team consensus | Depends on decision |
