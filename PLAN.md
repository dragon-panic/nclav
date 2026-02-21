# Bootstrap Phase 2 — Implementation Plan

## What this work is

The current nclav implementation has two problems:

1. **Bootstrap is just "start a process"** — there is no persistent state store. When the server
   restarts, all enclave state is lost. Bootstrap should provision a real platform (the API server
   plus a persistent state store) that survives restarts. Locally this means a redb file on disk;
   on GCP it means Cloud Run + Cloud SQL.

2. **All enclaves share one driver** — the driver is chosen globally at bootstrap time and applied
   to every enclave. We need per-enclave cloud targeting: each enclave's `cloud:` field in YAML
   selects which driver provisions it. The YAML field is optional — when absent the enclave
   inherits the API's configured default cloud. This makes the same YAML portable across local
   dev (default: local) and production GCP (default: gcp) without modification.

The full design rationale is in `BOOTSTRAP.md`. This file tracks the implementation TODOs.

## Key design decisions
- **redb** (not SQLite) for local persistent store: pure Rust, ~200KB, no C toolchain, maps cleanly to nclav's KV+append-log patterns
- **`cloud:` optional in enclave YAML**: absent = use API default cloud; same YAML works local and GCP
- **DriverRegistry** replaces single `Arc<dyn Driver>`: dispatches per-enclave, LocalDriver always registered, `default_cloud` set at bootstrap
- **`resolved_cloud`** stored on `EnclaveState`: needed so teardown knows which driver to use after enclave is removed from YAML
- **Bootstrap = platform provisioning** (API + state store), separate from enclave provisioning
- **GCP platform bootstrap**: provisions `{prefix}-nclav` project with Cloud Run (API) + Cloud SQL (state store)
- **Import wiring**: use importer's driver for `provision_import`

## Docs updated (commit ba8bb2a)
- `BOOTSTRAP.md` — PRD for bootstrap design
- `README.md` — expanded bootstrap section, redb local modes
- `GCP.md` — GCP bootstrap platform section
- `PRD.md` — Bootstrap section now links to BOOTSTRAP.md

## Key interface changes needed

### reconciler
```rust
// BEFORE
pub async fn reconcile(req, store: Arc<dyn StateStore>, driver: Arc<dyn Driver>)
// AFTER
pub async fn reconcile(req, store: Arc<dyn StateStore>, registry: Arc<DriverRegistry>)
```
Inside loop: `let driver = registry.for_enclave(enc)?;`

### AppState (nclav-api)
```rust
// BEFORE
pub struct AppState { pub store: Arc<dyn StateStore>, pub driver: Arc<dyn Driver> }
// AFTER
pub struct AppState { pub store: Arc<dyn StateStore>, pub registry: Arc<DriverRegistry> }
```

### build_app
```rust
// BEFORE: build_app(store, driver)
// AFTER:  build_app(store, registry)
```

## Areas (in dependency order)

### Area 1: cloud: optional in domain + config ← START HERE
- `Enclave.cloud: CloudTarget` → `Option<CloudTarget>` (types.rs)
- `RawEnclave.cloud: String` → `Option<String>` (raw.rs)
- `convert_enclave`: map through `parse_cloud` only when Some (loader.rs)
- Heuristic in `collect_enclaves`: key on `id` presence not `cloud` (since cloud now optional)
- Tests: YAML with cloud: → Some(Gcp); YAML without cloud: → None

### Area 2: resolved_cloud on EnclaveState
- Add `resolved_cloud: Option<CloudTarget>` to `EnclaveState` (state.rs)
- `EnclaveState::new()` sets None; reconciler fills in before first upsert

### Area 3: DriverRegistry (new file: nclav-driver/src/registry.rs)
- `DriverRegistry { default_cloud: CloudTarget, drivers: HashMap<CloudTarget, Arc<dyn Driver>> }`
- Builder: `.register(cloud, driver)` — LocalDriver always registered
- `for_enclave(&self, enc) -> Result<Arc<dyn Driver>, DriverError>` — resolves enc.cloud.unwrap_or(default)
- `resolved_cloud(&self, enc) -> CloudTarget`
- `active_clouds(&self) -> Vec<CloudTarget>`
- `DriverError::DriverNotConfigured(CloudTarget)` new variant
- Export from lib.rs

### Area 4: RedbStore (new file: nclav-store/src/redb_store.rs)
- Add `redb = "2"` to nclav-store/Cargo.toml
- Tables: ENCLAVES (&str → &[u8]), EVENTS (u64 → &[u8]), META (&str → u64 for seq counter)
- `RedbStore::open(path: &Path) -> Result<Self, StoreError>` — creates parent dirs
- Implement StateStore: upsert/get/delete/list enclaves; append/list events
- Key test: persistence (open → write → drop → reopen → verify data survives)

### Area 5: Reconciler accepts DriverRegistry
- `reconcile(req, store, registry: Arc<DriverRegistry>)`
- Per-enclave: `let driver = registry.for_enclave(enc)?;` — per-enclave error, not global abort
- Set `enc_state.resolved_cloud = Some(registry.resolved_cloud(enc))` before upsert
- Teardown: use `enc_state.resolved_cloud` to pick driver
- Import wiring: use importer's driver
- Update existing tests to build DriverRegistry

### Area 6: API — wire registry through
- AppState: driver → registry
- build_app signature change
- handlers: pass registry to reconcile()
- /status: add default_cloud, active_drivers from registry

### Area 7: CLI — bootstrap modes
- Add --ephemeral flag, --store enum (redb/postgres), --store-url
- --ephemeral → InMemoryStore; default → RedbStore::open(~/.nclav/state.redb); postgres → stub error
- Build DriverRegistry in commands.rs (local: Local default; gcp: Gcp default + Local also registered)

### Area 8: Display
- output.rs: append [gcp]/[local] after enclave name in graph text
- status command: print default_cloud, active_drivers from /status JSON

### Area 9: GCP platform bootstrap (phase 2, larger)
- GcpDriver::bootstrap_platform() → PlatformInfo { api_url, project_id, region }
- Provisions {prefix}-nclav project: Cloud Run + Cloud SQL + nclav-runner SA
- bootstrap --cloud gcp → platform provision + print URL + exit (no local server)
- New CLI flags: --gcp-platform-project, --gcp-platform-region, --gcp-platform-parent

## Files to touch per area
1. nclav-domain/src/types.rs, nclav-config/src/raw.rs, nclav-config/src/loader.rs
2. nclav-store/src/state.rs
3. nclav-driver/src/registry.rs (new), nclav-driver/src/lib.rs, nclav-driver/src/error.rs
4. nclav-store/src/redb_store.rs (new), nclav-store/src/lib.rs, nclav-store/Cargo.toml
5. nclav-reconciler/src/reconcile.rs
6. nclav-api/src/state.rs, nclav-api/src/app.rs, nclav-api/src/handlers.rs
7. nclav-cli/src/cli.rs, nclav-cli/src/commands.rs, nclav-cli/src/main.rs
8. nclav-cli/src/output.rs
9. nclav-driver/src/gcp.rs (or gcp_bootstrap.rs)
