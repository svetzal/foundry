All 203 tests pass (24 foundry-core + 179 foundryd). Here's a summary of every change made:

---

## Part A — Lint Consolidation

The identical 8-line `[lints.clippy]` / `[lints.rust]` blocks that were duplicated across all three crates now live once in the workspace root `Cargo.toml` under `[workspace.lints.*]`. Each crate carries only `[lints] workspace = true`. A single place to maintain lint policy.

---

## Part B — Gateway Traits

**New file: `crates/foundryd/src/gateway.rs`**

- `ShellGateway` trait — wraps `crate::shell::run`, returning `Pin<Box<dyn Future + Send + '_>>` to match the project's existing async pattern without introducing `async_trait`
- `ProcessShellGateway` — zero-field production struct delegating to `shell::run`
- `ScannerGateway` trait and `ProcessScannerGateway` — same pattern wrapping `scanner::run_audit`
- `fakes` submodule (`#[cfg(test)]`):
  - `FakeShellGateway` with `always`, `sequence`, `success`, `failure` constructors plus an `invocations()` recorder
  - `FakeScannerGateway` with `clean`, `with_vulnerabilities`, `with_error`
- `shell.rs` and `scanner.rs` were **not touched**

**9 blocks migrated** (validate, scan, audit, remediate, hone_iterate, hone_maintain, git_ops, release, install):

Each block received an `Arc<dyn ShellGateway>` / `Arc<dyn ScannerGateway>` field injected via a `#[cfg(test)] with_shell` / `with_scanner` / `with_gateways` constructor. All `crate::shell::run(...)` and `crate::scanner::run_audit(...)` calls replaced with self calls through the gateway.

`WatchPipeline::query_latest_run` was also migrated — it was the one place bypassing `shell::run` by using `tokio::process::Command` directly; now routed through `ShellGateway` for consistency.

**New edge-case tests** that were previously impossible: detached HEAD recovery success/failure, git rev-parse failure, tag scan with vulnerabilities, scanner error handling, hone success/failure paths, commit message variants verified via fake invocation recorder, pipeline completed/failed scenarios, and more.

---

## Part C — Documentation

- `book/src/guide/writing-task-blocks.md` — new "Gateway Pattern" section with constructor example and "Testing with Fakes" code walkthrough
- `book/src/architecture/task-blocks.md` — new "Gateway Pattern" paragraph explaining the design rationale
- `book/src/architecture/crates.md` — `gateway.rs` added to the daemon support modules list