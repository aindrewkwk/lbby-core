# ARCHITECTURE FREEZE — Phase 3O (Installer/Recovery Track)

**Status:** FROZEN as of Phase 3O completion  
**Baseline commits:**
- `lbby-core`: `6d262ff` (main)
- `lbby-app`: `020bad1` (feature/import-server)
- `lbby-releases`: `0a5dd3a` (main)

---

## 1. Final Installer Pipeline

The canonical install pipeline is the ONLY allowed install path:

```
source acquisition (CurseForge / Modrinth / local ZIP / vanilla)
→ transactional staging (InstallTransaction::begin)
→ metadata normalization (mod JARs scanned, metadata extracted)
→ compatibility classification (ServerCompatibility enum)
→ dependency graph (DependencyGraph::build)
→ deterministic repair (MAX_REPAIR_ROUNDS = 1)
→ boot validation (BootValidator, MAX_TOTAL_BOOT_ATTEMPTS = 6)
→ runtime remediation (RuntimeRemediator, MAX_RUNTIME_REMEDIATION_ROUNDS = 2)
→ crash attribution (BootFailureAnalyzer)
→ UserActionRequired if destructive recovery needed
→ explicit user approval (never auto-approved)
→ quarantine (never delete)
→ rebuild graph
→ revalidate
→ commit (atomic staging→live swap)
→ production start (shared server_launch builder)
```

**No alternative undocumented install path is allowed.**

---

## 2. Final Validation Pipeline

```
BootValidator::validate()
→ check server.jar exists
→ check eula.txt acceptance
→ launch server process via shared launch builder
→ monitor stdout for readiness markers
→ check for crash reports
→ if crash: BootFailureAnalyzer::analyze()
→ attribution: extract suspected mods from crash report
→ if high-confidence attribution: UserActionRequired
→ if OOM detected: diagnostic only (no auto RAM mutation)
→ if loader mismatch: advisory only (no auto mutation)
→ retry up to MAX_TOTAL_BOOT_ATTEMPTS
```

---

## 3. Final Recovery Pipeline

```
UserActionRequired surfaced to UI
→ user reviews recovery details
→ user explicitly Approves or Rejects
→ if Approve:
  → target JAR quarantined (copied, not deleted)
  → dependency graph rebuilt
  → boot validation resumes
  → if successful: commit
→ if Reject:
  → transaction rollback
  → live server remains unchanged
  → pending recovery cleared
→ if Restore:
  → server must be stopped
  → SHA-256 verified
  → identity verified
  → duplicate-provider check
  → atomic temp copy → rename into live/mods
  → quarantine evidence retained
```

---

## 4. Persistence Model

### 4.1 Atomic Authoritative Write

All persistence uses `atomic_write_json()`:
- Write to `.tmp` file in same directory
- `fsync` the file
- Atomic rename to final path
- Crash-safe: either complete write exists or old version

### 4.2 Schema Versioning

All persisted structs implement `HasSchemaVersion`:
- `schema_version: u32` field, `#[serde(default)]` for legacy v0 compat
- `CURRENT_SCHEMA_VERSION = 1`
- Legacy v0 accepted (schema_version defaults to 0, migrated on read)
- Future schema > CURRENT → `UnsupportedSchema` (never overwritten)

### 4.3 Persisted Schemas

| Struct | Path | Schema Version |
|--------|------|---------------|
| `TransactionMeta` | `<staging>/transaction.json` | v1 |
| `PendingRecoveryMetadata` | `<staging>/pending_recovery.json` | v1 |
| `RetryStateSnapshot` | `<server>/.lbby-retry-state.json` | v1 |
| `QuarantineMetadata` | `<quarantine>/quarantine_metadata.json` | v1 |

### 4.4 Migration Policy

- Legacy v0 files accepted: `schema_version` defaults to 0 via `#[serde(default)]`
- On read, v0 may be migrated to CURRENT if appropriate
- Future schema > CURRENT: rejected with `UnsupportedSchema`
- No overwrite, no cleanup, no silent fallback to fresh state

---

## 5. Startup Reconciliation Model

On app startup, `run_startup_reconciliation()` runs:

```
discover all transactions in .lbby-staging/
→ classify each by phase:
  - Building → Abandoned (needs cleanup)
  - PendingUserAction → surfaces Recovery Required
  - Committing → safe finalize or rollback
  - Committed → cleanup staging
→ classify corrupt entries (parse failures)
→ classify unsupported schema entries (future versions)
→ report summary to UI
```

**Rules:**
- Discovery is READ-ONLY (no delete, no remove, no rewrite)
- No automatic resume of Building transactions
- No automatic rollback of PendingUserAction
- Corrupt entries reported, not acted upon
- UnsupportedSchema counted separately from corrupt

---

## 6. App / Core / Node / Cloud Boundaries

| Component | Owns | Does NOT Own |
|-----------|------|-------------|
| **lbby-core** | Restore/quarantine domain logic, filesystem safety, crash recovery primitives, dependency resolution, compatibility classification | Tenant auth, billing, cloud scheduling, node assignment, HTTP API policy |
| **lbby-app** | Local lifecycle adapter, Tauri commands, UI bridge, OperationGuard | Core domain logic, cloud orchestration |
| **lbby-node** | Future: trusted execution adapter, server lifecycle on managed nodes | — |
| **lbby-cloud** | Future: auth, orchestration, job ownership, distributed coordination | — |

---

## 7. Safety Invariants

1. **No destructive mod action without explicit user approval**
   - Exception: Explicit ClientOnly exclusion during pre-commit compatibility planning
   - UNKNOWN is never destructive

2. **No auto-approve, no auto-start, no auto-restore**

3. **Quarantine not delete** — JARs are copied to quarantine, never removed from live without explicit approval

4. **Restore requires explicit user action** — server must be stopped, SHA verified, identity verified

5. **Atomic authoritative persistence** — all writes use tmp+rename, crash-safe

6. **Discovery functions are READ-ONLY** — no mutation during startup reconciliation

7. **No bare ServerConfig** — all install results flow through `InstallOutcome` DTO bridge

8. **No filesystem paths exposed to frontend**

9. **Pause uses explicit rollback** — not Drop-dependent, deterministic safety

10. **Future schema → UnsupportedSchema** — never silently become fresh retry state

---

## 8. Retry Budgets (Frozen Constants)

| Constant | Value | Location |
|----------|-------|----------|
| `MAX_TOTAL_BOOT_ATTEMPTS` | 6 | `validation_orchestrator.rs:82`, `runtime_remediator.rs:27` |
| `MAX_RUNTIME_REMEDIATION_ROUNDS` | 2 | `runtime_remediator.rs:22` |
| `MAX_REPAIR_ROUNDS` | 1 | `dependency_resolver.rs:25` |
| `MAX_USER_RECOVERY_ACTIONS` | 2 | `recovery_actions.rs:34` |
| `CURRENT_SCHEMA_VERSION` | 1 | `atomic_persistence.rs` |

---

## 9. Known Limitations

### 9.1 Process-Local Restore Lock
Core/App restore lock is process-local. Future managed Cloud requires:
- Durable control-plane job ownership
- Node journal/idempotency
- Server-scoped operation coordination

### 9.2 No Distributed Locking
Not implemented in Phase 3O. Deferred to Cloud phase.

### 9.3 Cloud Recovery Does Not Exist
No claim of Cloud recovery functionality. Cloud is future work.

### 9.4 Platform Verification Status
- **Windows:** CI/build verified
- **Linux:** CI/build verified
- **macOS:** Local development only, NOT CI-verified

### 9.5 Release Source Branch
- lbby-app Phase 3O work is on `feature/import-server`
- Releases are created from exact git tags (lbby-releases workflow checks out tag)
- The branch is intended to merge to `main` before release
- lbby-releases workflow verifies tag integrity regardless of source branch

---

## 10. Deferred Work

1. **Distributed locking** — Cloud/node coordination
2. **Cloud recovery** — Multi-node server management
3. **Automated testing of real CurseForge/Modrinth installs** — Requires API keys and running Java environment
4. **macOS CI** — Not currently in CI matrix
5. **Real UserActionRequired E2E test** — Requires running Minecraft server with real mod that crashes

---

## 11. Prohibited Shortcuts

1. **No bypassing ValidationRepairOrchestrator** for mod installation
2. **No bypassing InstallTransaction** for staging/commit
3. **No bypassing OperationGuard** for concurrent operation prevention
4. **No bypassing InstallOutcome** for install result transport
5. **No bypassing recovery approval APIs** for recovery actions
6. **No bypassing startup reconciliation** for stale transaction cleanup
7. **No bypassing shared server_launch builder** for Java process launch
8. **No direct fs::remove_file on live mods** outside quarantine flow
9. **No direct live modpack extraction** without transaction staging
10. **No old compatibility bool code** — must use `ServerCompatibility` enum

---

## 12. Release Acceptance Matrix

| Gate | Required | Status |
|------|----------|--------|
| CurseForge Forge real smoke | YES | BLOCKED (no running Java env) |
| CurseForge Fabric real smoke | YES | BLOCKED (no running Java env) |
| Modrinth real smoke | YES | BLOCKED (no running Java env) |
| Local ZIP smoke | YES | PASS (unit tests) |
| UserActionRequired flow | YES | PASS (unit tests) |
| Approve flow | YES | PASS (unit tests) |
| Reject flow | YES | PASS (unit tests) |
| Restart PendingUserAction | YES | PASS (unit tests) |
| Crash during Building | YES | PASS |
| Crash during Committing | YES | PASS |
| Corrupt metadata | YES | PASS |
| Future schema | YES | PASS |
| Legacy upgrade | YES | PASS |
| Quarantine restore | YES | PASS (unit tests) |
| Restore failure matrix | YES | PASS (unit tests) |
| OperationGuard | YES | PASS (code verified) |
| Production start | YES | BLOCKED (no Java env) |
| Residue audit | YES | PASS |
| Network failure | YES | PASS |
| Java remediation | YES | BLOCKED (no Java download env) |
| OOM diagnostic | YES | PASS (code verified) |
| Loader mismatch | YES | PASS (code verified) |
| Dependency repair | YES | PASS (unit tests) |
| Ambiguity | YES | PASS |
| Duplicate provider | YES | PASS |
| UNKNOWN retention | YES | PASS |
| Explicit ClientOnly | YES | PASS |
| Dependency conflict | YES | PASS |
| Persistent state preservation | YES | PASS |
| Existing live safety | YES | PASS |
| Fresh install | YES | PASS |
| EN UI | YES | BLOCKED (no running app) |
| VI UI | YES | BLOCKED (no running app) |
| Accessibility | YES | BLOCKED (no running app) |
| Release workflow | YES | PASS |
| Core CI | YES | PASS |
| Windows build | YES | PASS |
| Linux build | YES | PASS |
| macOS status | — | NOT CI-VERIFIED |
| Bypass audit | YES | PASS |
| Architecture freeze doc | YES | PASS (this document) |

---

## 13. Final Pipeline Diagram

```
┌─────────────────────────────────────────────────────────┐
│                    INSTALL SOURCE                        │
│  CurseForge │ Modrinth │ Local ZIP │ Vanilla/Paper       │
└───────────────────────┬─────────────────────────────────┘
                        │
                        ▼
┌─────────────────────────────────────────────────────────┐
│              TRANSACTIONAL STAGING                       │
│  InstallTransaction::begin()                            │
│  Creates: .lbby-staging/<server>-<txn_id>/              │
│  Writes: transaction.json (atomic)                      │
│  Phase: Building                                        │
└───────────────────────┬─────────────────────────────────┘
                        │
                        ▼
┌─────────────────────────────────────────────────────────┐
│            METADATA NORMALIZATION                        │
│  Scan JARs → extract mod metadata                       │
│  Parse fabric.mod.json / META-INF/mods.toml             │
│  Extract mod_id, dependencies, environment               │
└───────────────────────┬─────────────────────────────────┘
                        │
                        ▼
┌─────────────────────────────────────────────────────────┐
│          COMPATIBILITY CLASSIFICATION                    │
│  ServerCompatibility enum:                              │
│    ServerOk │ ClientOnly │ Both │ Unknown               │
│  Confidence: Explicit │ None                            │
│  Only Explicit ClientOnly excluded automatically        │
└───────────────────────┬─────────────────────────────────┘
                        │
                        ▼
┌─────────────────────────────────────────────────────────┐
│              DEPENDENCY GRAPH                            │
│  DependencyGraph::build()                               │
│  Maps: mod_id → JAR → dependencies                      │
│  Detects: missing, conflicting, ambiguous               │
└───────────────────────┬─────────────────────────────────┘
                        │
                        ▼
┌─────────────────────────────────────────────────────────┐
│           DETERMINISTIC REPAIR                           │
│  MAX_REPAIR_ROUNDS = 1                                  │
│  Bounded candidate resolution                           │
│  Ambiguous → STOP (no auto-pick)                        │
└───────────────────────┬─────────────────────────────────┘
                        │
                        ▼
┌─────────────────────────────────────────────────────────┐
│             BOOT VALIDATION                              │
│  BootValidator (MAX_TOTAL_BOOT_ATTEMPTS = 6)            │
│  Shared server_launch builder                           │
│  Monitor stdout for readiness                           │
│  Check crash reports                                    │
└───────────────────────┬─────────────────────────────────┘
                        │
                        ▼
┌─────────────────────────────────────────────────────────┐
│          RUNTIME REMEDIATION                             │
│  MAX_RUNTIME_REMEDIATION_ROUNDS = 2                     │
│  OOM detection → diagnostic only                        │
│  Loader mismatch → advisory only                        │
└───────────────────────┬─────────────────────────────────┘
                        │
                        ▼
┌─────────────────────────────────────────────────────────┐
│           CRASH ATTRIBUTION                              │
│  BootFailureAnalyzer::analyze()                         │
│  Extract suspected mods from crash report               │
│  High-confidence → UserActionRequired                   │
└───────────────────────┬─────────────────────────────────┘
                        │
                        ▼
┌───────────────────────┴─────────────────────────────────┐
│         USER ACTION REQUIRED?                            │
│  YES → Surface to UI → Wait for explicit approval       │
│  NO  → Skip to commit                                   │
└───────────────────────┬─────────────────────────────────┘
                        │
              ┌─────────┴─────────┐
              │                   │
              ▼                   ▼
    ┌─────────────────┐  ┌─────────────────┐
    │    APPROVE       │  │    REJECT        │
    │ Quarantine JAR   │  │ Rollback txn     │
    │ Rebuild graph    │  │ Live unchanged   │
    │ Revalidate       │  │ Clear pending    │
    └────────┬────────┘  └─────────────────┘
             │
             ▼
┌─────────────────────────────────────────────────────────┐
│                   COMMIT                                 │
│  Atomic: staging → live swap                            │
│  Backup preserved in .lbby-staging/<server>-<id>-backup │
│  Phase: Committed                                       │
└───────────────────────┬─────────────────────────────────┘
                        │
                        ▼
┌─────────────────────────────────────────────────────────┐
│              PRODUCTION START                            │
│  Shared server_launch builder                           │
│  JAVA_HOME derivation                                   │
│  Canonical launch command                               │
│  Console readiness monitoring                           │
└─────────────────────────────────────────────────────────┘
```

---

## Document History

| Date | Event | Commit |
|------|-------|--------|
| Phase 3O | Architecture freeze | lbby-core@6d262ff, lbby-app@020bad1 |
