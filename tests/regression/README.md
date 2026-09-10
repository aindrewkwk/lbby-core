# Phase 3I — Real-world Regression Suite

## What's tested offline (no network required)

57 deterministic regression tests covering:

| Category | Tests | Scenarios |
|---|---|---|
| Happy paths | A, B, C | Manifest fallback, server-pack, Fabric |
| Safety invariants | D, E | UNKNOWN retention, ClientOnly quarantine |
| 3F-A dep repair | F, G, H | Happy, ambiguous, unsupported constraint |
| 3F-B runtime dep | I, J, K, L | Happy, wrong identity, ambiguity, bound |
| 3F-C Java repair | M, N, O | WrongJava repair, fails-again stop, JavaNotFound |
| Non-repairable | P, Q, R, S, T | OOM, loader mismatch, pin, family, conflicts |
| Orchestrator | U, V, W | Precedence, chained repairs, dep→loader |
| Transactions | X, Y, Z | Rollback, commit, custom level-name |
| Validation cleanup | — | Residue detection, cleanup failure |
| READY semantics | — | Only after success + cleanup |
| Retry budgets | — | Global ceiling, dep loop, Java loop |
| Server-pack | — | Runtime repair, dep limitation, loader advisor |
| Loader advisor | — | Fabric, Quilt, numeric comparison, pin precedence |
| Edge cases | — | Timeout, EULA, history completeness, auto-rollback |
| Transactions | — | Find stale, fresh install, backup cleanup |

## How to run

```bash
# All regression tests (offline, deterministic)
cargo test --test regression --features testing

# Run specific scenario
cargo test --test regression --features testing regression_scenario_a

# Also run existing unit tests
cargo test --lib
```

## What requires real infrastructure (smoke tests)

Existing ignored smoke tests in `tests/smoke_*.rs`:

```bash
# Requires Java 17+ and MC 1.21.4 server.jar at /tmp/smoke-test/
cargo test --test smoke_boot_validation -- --ignored
cargo test --test smoke_full_pipeline -- --ignored
```

## Required env vars for smoke tests

- `CF_API_KEY` — CurseForge API key (for smoke_d and smoke_e)
- Java 17+ installed
- MC 1.21.4 server.jar at `/tmp/smoke-test/server.jar`

## `testing` feature flag

The `testing` feature exposes `ValidationRepairOrchestrator::new_with_repair_overrides()` for integration tests. This injects deterministic repair results without network calls.

- No production behavior changes when `testing` is disabled
- Default builds (`cargo build`) do NOT enable `testing`
- Only regression tests compile with `--features testing`

## Test naming convention

All tests follow: `regression_scenario_{letter}_{description}`

Examples:
- `regression_scenario_a_manifest_fallback_success`
- `regression_scenario_i_runtime_dep_repair`
- `regression_scenario_v_chained_dep_then_java`

## Expected runtime

- Standard suite: <1 second (57 tests, all mocked)
- Unit tests: ~30 seconds (362 tests)
- Ignored smokes: 30-120 seconds each (real server boots)

## Cross-platform notes

- All tests use `PathBuf` and `std::path::Path` — no hardcoded `/`
- Temp directories via `tempfile::tempdir()` — auto-cleaned
- No Unix-specific commands, chmod, or shell strings
- No machine-installed Minecraft required for standard suite
