Now I have a thorough understanding of the entire codebase. Here is the complete, detailed, step-by-step plan:

---

## Execution Plan: Introduce Gateway Traits and Consolidate Lint Configuration

This plan addresses two issues identified in the assessment:
1. **Severity 3**: Task blocks hard-wire I/O with no gateway trait boundary (violates functional core, imperative shell)
2. **Severity 2**: Lint configuration duplicated across three `Cargo.toml` files (violates no knowledge duplication)

The plan is ordered to minimize risk: the lint consolidation (independent, low-risk) is done first, followed by the gateway introduction in carefully sequenced increments that keep the codebase green after each step.

---

### Part A: Consolidate Lint Configuration (Severity 2)

#### Step 1: Move lint configuration to workspace root

**File**: `/Users/svetzal/Work/Projects/Mojility/foundry/Cargo.toml`

Add the following sections at the end of the workspace root `Cargo.toml`:

```toml
[workspace.lints.clippy]
all = { level = "warn", priority = -1 }
pedantic = { level = "warn", priority = -1 }
module_name_repetitions = "allow"
must_use_candidate = "allow"
missing_errors_doc = "allow"
missing_panics_doc = "allow"

[workspace.lints.rust]
unsafe_code = "deny"
```

#### Step 2: Replace per-crate lint blocks with workspace inheritance

**File**: `/Users/svetzal/Work/Projects/Mojility/foundry/crates/foundry-core/Cargo.toml`

Remove the entire `[lints.clippy]` and `[lints.rust]` sections (lines 22–31) and replace with:

```toml
[lints]
workspace = true
```

**File**: `/Users/svetzal/Work/Projects/Mojility/foundry/crates/foundryd/Cargo.toml`

Remove the entire `[lints.clippy]` and `[lints.rust]` sections (lines 35–44) and replace with:

```toml
[lints]
workspace = true
```

**File**: `/Users/svetzal/Work/Projects/Mojility/foundry/crates/foundry-cli/Cargo.toml`

Remove the entire `[lints.clippy]` and `[lints.rust]` sections (lines 34–43) and replace with:

```toml
[lints]
workspace = true
```

#### Step 3: Verify lint consolidation

Run the quality gates to confirm the behaviour is identical:

```bash
cargo fmt --all -- --check
cargo clippy --workspace -- -D warnings
cargo test --workspace
```

All three must pass with zero differences in behaviour before proceeding.

---

### Part B: Introduce `ShellGateway` Trait (Severity 3)

This is the core architectural change. It is broken into small, safe increments that each leave the codebase in a green state.

#### Step 4: Define the `ShellGateway` trait and `CommandResult` in `foundryd`

**New file**: `/Users/svetzal/Work/Projects/Mojility/foundry/crates/foundryd/src/gateway.rs`

Create a new module `gateway` in `foundryd` containing:

1. **Re-export `CommandResult`** from `shell.rs` (or move the struct definition here — see reasoning below).

   Since `CommandResult` is currently defined in `shell.rs` and is used throughout the block implementations, the cleanest approach is to keep `CommandResult` defined in `shell.rs` and re-export it from `gateway.rs`. This avoids a disruptive move while the trait is being introduced.

2. **Define the `ShellGateway` trait**:

```rust
use std::path::Path;
use std::time::Duration;

use crate::shell::CommandResult;

/// Gateway trait that isolates shell command execution from block decision logic.
///
/// Production code uses `ProcessShellGateway` (a thin wrapper around `shell::run`).
/// Tests inject a `FakeShellGateway` to verify block decisions without spawning processes.
pub trait ShellGateway: Send + Sync {
    /// Run an external command asynchronously.
    ///
    /// Mirrors the signature of `shell::run` — see that function's documentation
    /// for argument semantics.
    fn run(
        &self,
        working_dir: &Path,
        command: &str,
        args: &[&str],
        env: Option<&[(String, String)]>,
        timeout: Option<Duration>,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = anyhow::Result<CommandResult>> + Send + '_>,
    >;
}
```

   Note: The trait uses `Pin<Box<dyn Future>>` for the return type to maintain object safety without requiring `async_trait`. This is consistent with the project's existing pattern (used in `TaskBlock::execute`). The project does not currently depend on `async_trait`, so we do not introduce it.

3. **Define `ProcessShellGateway`** — the production implementation:

```rust
/// Production gateway: delegates to `shell::run`.
///
/// This is a gateway struct — a thin wrapper around the real I/O function.
/// It has no logic to test.
pub struct ProcessShellGateway;

impl ShellGateway for ProcessShellGateway {
    fn run(
        &self,
        working_dir: &Path,
        command: &str,
        args: &[&str],
        env: Option<&[(String, String)]>,
        timeout: Option<Duration>,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = anyhow::Result<CommandResult>> + Send + '_>,
    > {
        Box::pin(crate::shell::run(working_dir, command, args, env, timeout))
    }
}
```

4. **Define `FakeShellGateway`** (behind `#[cfg(test)]`):

```rust
#[cfg(test)]
pub mod fake {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::Mutex;

    /// A fake shell gateway for testing block decision logic.
    ///
    /// Supports two modes:
    /// - **Single response**: returns the same `CommandResult` for every call
    /// - **Sequence**: returns results from a queue in order; panics if exhausted
    ///
    /// Also records all invocations for assertion purposes.
    pub struct FakeShellGateway {
        responses: Mutex<VecDeque<anyhow::Result<CommandResult>>>,
        default_response: Option<CommandResult>,
        pub invocations: Mutex<Vec<FakeInvocation>>,
    }

    #[derive(Debug, Clone)]
    pub struct FakeInvocation {
        pub command: String,
        pub args: Vec<String>,
        pub working_dir: std::path::PathBuf,
    }

    impl FakeShellGateway {
        /// Create a fake that always returns the given result.
        pub fn always(result: CommandResult) -> Self {
            Self {
                responses: Mutex::new(VecDeque::new()),
                default_response: Some(result),
                invocations: Mutex::new(Vec::new()),
            }
        }

        /// Create a fake with a sequence of results returned in order.
        pub fn sequence(results: Vec<anyhow::Result<CommandResult>>) -> Self {
            Self {
                responses: Mutex::new(results.into()),
                default_response: None,
                invocations: Mutex::new(Vec::new()),
            }
        }

        /// Create a fake that always returns a successful result with the given stdout.
        pub fn success(stdout: &str) -> Self {
            Self::always(CommandResult {
                stdout: stdout.to_string(),
                stderr: String::new(),
                exit_code: 0,
                success: true,
            })
        }

        /// Create a fake that always returns a failure result.
        pub fn failure(stderr: &str, exit_code: i32) -> Self {
            Self::always(CommandResult {
                stdout: String::new(),
                stderr: stderr.to_string(),
                exit_code,
                success: false,
            })
        }

        /// Create a fake that always returns an Err (simulating spawn failure).
        pub fn spawn_error(msg: &str) -> Self {
            let msg = msg.to_string();
            Self {
                responses: Mutex::new(vec![Err(anyhow::anyhow!("{}", msg))].into()),
                default_response: None,
                invocations: Mutex::new(Vec::new()),
            }
        }
    }

    impl ShellGateway for FakeShellGateway {
        fn run(
            &self,
            working_dir: &Path,
            command: &str,
            args: &[&str],
            _env: Option<&[(String, String)]>,
            _timeout: Option<Duration>,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = anyhow::Result<CommandResult>> + Send + '_>,
        > {
            self.invocations.lock().unwrap().push(FakeInvocation {
                command: command.to_string(),
                args: args.iter().map(|s| s.to_string()).collect(),
                working_dir: working_dir.to_path_buf(),
            });

            let result = {
                let mut responses = self.responses.lock().unwrap();
                if let Some(r) = responses.pop_front() {
                    r
                } else if let Some(ref default) = self.default_response {
                    Ok(default.clone())
                } else {
                    panic!("FakeShellGateway: no more responses in sequence and no default set")
                }
            };

            Box::pin(async move { result })
        }
    }
}
```

#### Step 5: Register the gateway module

**File**: `/Users/svetzal/Work/Projects/Mojility/foundry/crates/foundryd/src/main.rs`

Add `mod gateway;` to the module declarations (alongside `mod shell;`). This is declaration only — no block changes yet.

#### Step 6: Verify the gateway compiles

```bash
cargo build --workspace
cargo test --workspace
```

Nothing should change yet — the gateway exists but no block uses it.

---

### Part C: Migrate Blocks to Use the Gateway — One Block at a Time

Each step follows the same pattern:
1. Add `shell: Arc<dyn ShellGateway>` field to the block struct
2. Update `new()` to accept the gateway
3. Replace `crate::shell::run(...)` calls with `self.shell.run(...)` (or the gateway reference passed to helper functions)
4. Update `main.rs` registration to pass the production gateway
5. Rewrite tests to use `FakeShellGateway` for decision-logic tests; keep 1–2 real-I/O tests clearly marked as integration tests
6. Run quality gates

**Important ordering**: Start with blocks that have the simplest I/O patterns and fewest tests to rewrite. Progress to more complex blocks.

#### Step 7: Migrate `ValidateProject`

**Why first**: It has a clear separation between the decision logic (directory exists, branch matches, gates file present) and the I/O (git commands, filesystem checks). The `check_git_branch` helper already isolates two shell calls.

**Changes to `validate.rs`**:

1. Add `shell: Arc<dyn ShellGateway>` field to `ValidateProject`.
2. Update the constructor:
   ```rust
   pub fn new(registry: Arc<Registry>, shell: Arc<dyn ShellGateway>) -> Self {
       Self { registry, shell }
   }
   ```
3. Update `check_git_branch` to accept a `&dyn ShellGateway` parameter instead of calling `crate::shell::run` directly. Replace both `crate::shell::run(...)` calls with `shell.run(...)`.
4. In the `execute` method, replace the `path.exists()` check — this is a filesystem check, not a shell command. There are two options:
   - **Option A**: Leave `path.exists()` as-is (it's a synchronous, fast, OS-level check — not really "shell I/O"). This is the pragmatic choice. The gateway trait is for subprocess execution, not every filesystem syscall.
   - **Option B**: Introduce a `FilesystemGateway` trait with `exists()` and `read()` methods.

   **Recommendation**: Option A. The `path.exists()` and `path.join(...).exists()` calls are fast, synchronous, and deterministic given the filesystem state. Wrapping them in a trait would be over-engineering. The tests can continue to use `tempdir()` for these checks — that's a perfectly good fake for filesystem existence. Focus the gateway on subprocess execution which is the expensive, non-deterministic, slow operation.

5. Pass `&*self.shell` into `check_git_branch`.

**Changes to tests in `validate.rs`**:

- Update `make_registry`-based tests to pass `Arc::new(FakeShellGateway::...)` as the shell parameter.
- For `valid_project_on_correct_branch_emits_ok`: Rewrite to use `FakeShellGateway::success("main\n")` instead of discovering the real git branch at runtime. This eliminates the "if detached HEAD, bail" logic and the dependency on the real workspace being a git repo.
- For `wrong_branch_emits_error_status`: Use `FakeShellGateway::success("develop\n")` to simulate a branch mismatch.
- For `missing_gates_still_emits_ok` and `gates_file_present_sets_has_gates_true`: These still use `tempfile` for the directory existence check, but use `FakeShellGateway::success("main\n")` for the git branch check. Remove `init_git_repo` since we no longer need a real git repo.
- Add new tests that were previously impossible:
  - `detached_head_recovery_succeeds`: Fake returns `"HEAD\n"` first, then success for checkout
  - `detached_head_recovery_fails`: Fake returns `"HEAD\n"` first, then non-zero exit for checkout
  - `git_rev_parse_failure`: Fake returns non-zero exit code for rev-parse

**Changes to `main.rs`**:

```rust
let shell: Arc<dyn gateway::ShellGateway> = Arc::new(gateway::ProcessShellGateway);
// ...
engine.register(Box::new(blocks::ValidateProject::new(registry.clone(), shell.clone())));
```

**Verify**:
```bash
cargo fmt --all -- --check
cargo clippy --workspace -- -D warnings
cargo test --workspace
```

#### Step 8: Introduce `ScannerGateway` trait for `scanner::run_audit`

Before migrating `ScanDependencies`, `AuditReleaseTag`, and `AuditMainBranch`, we need a gateway for the scanner module too, since these blocks call `crate::scanner::run_audit()` rather than `crate::shell::run()` directly.

**Add to `gateway.rs`**:

```rust
use crate::scanner::AuditResult;
use foundry_core::registry::Stack;

/// Gateway trait for vulnerability scanning.
///
/// Wraps `scanner::run_audit` to allow test fakes that return
/// pre-configured audit results.
pub trait ScannerGateway: Send + Sync {
    fn run_audit(
        &self,
        path: &Path,
        stack: &Stack,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = anyhow::Result<AuditResult>> + Send + '_>,
    >;
}

/// Production scanner gateway: delegates to `scanner::run_audit`.
pub struct ProcessScannerGateway;

impl ScannerGateway for ProcessScannerGateway {
    fn run_audit(
        &self,
        path: &Path,
        stack: &Stack,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = anyhow::Result<AuditResult>> + Send + '_>,
    > {
        Box::pin(crate::scanner::run_audit(path, stack))
    }
}
```

**Add `FakeScannerGateway`** (behind `#[cfg(test)]` in `gateway.rs`):

```rust
pub struct FakeScannerGateway {
    result: Mutex<VecDeque<anyhow::Result<AuditResult>>>,
    default_result: Option<AuditResult>,
}

impl FakeScannerGateway {
    pub fn clean() -> Self {
        Self {
            result: Mutex::new(VecDeque::new()),
            default_result: Some(AuditResult::default()),
        }
    }

    pub fn with_vulnerabilities(vulns: Vec<crate::scanner::Vulnerability>) -> Self {
        Self {
            result: Mutex::new(VecDeque::new()),
            default_result: Some(AuditResult {
                vulnerabilities: vulns,
                error: None,
            }),
        }
    }

    pub fn with_error(msg: &str) -> Self {
        Self {
            result: Mutex::new(VecDeque::new()),
            default_result: Some(AuditResult {
                vulnerabilities: vec![],
                error: Some(msg.to_string()),
            }),
        }
    }

    pub fn sequence(results: Vec<anyhow::Result<AuditResult>>) -> Self {
        Self {
            result: Mutex::new(results.into()),
            default_result: None,
        }
    }
}
```

**Verify**: `cargo build --workspace && cargo test --workspace`

#### Step 9: Migrate `ScanDependencies`

**Changes to `scan.rs`**:

1. Add `scanner: Arc<dyn ScannerGateway>` field.
2. Update `new()` to accept the scanner gateway.
3. Replace `crate::scanner::run_audit(path, &entry.stack).await?` with `self.scanner.run_audit(path, &entry.stack).await?`.

**New tests**:
- `emits_vulnerability_events_for_each_cve`: Fake scanner returns 3 vulnerabilities → verify 3 `VulnerabilityDetected` events with correct payloads.
- `scanner_error_returns_success_with_empty_events`: Fake scanner returns `AuditResult` with error set → verify success=true, empty events, summary contains the error.
- `clean_project_returns_no_events`: Fake scanner returns empty vulnerabilities → verify success=true, no events.

The existing `scans_known_project` test (which runs against `/tmp` and hopes the scanner isn't installed) can be removed — it's now replaced by deterministic fakes.

**Update `main.rs`**: Pass `Arc::new(gateway::ProcessScannerGateway)` to `ScanDependencies::new(...)`.

**Verify**: Quality gates.

#### Step 10: Migrate `AuditReleaseTag`

**Changes to `audit.rs`** (`AuditReleaseTag`):

1. Add both `shell: Arc<dyn ShellGateway>` and `scanner: Arc<dyn ScannerGateway>` fields.
2. Update `new()` and `with_registry()` to accept gateways.
3. In `audit_after_push`: Replace `crate::scanner::run_audit(...)` with `self.scanner.run_audit(...)`.
4. In `audit_after_vulnerability_detected`: Replace `crate::shell::run(...)` calls with `self.shell.run(...)`.
5. In `perform_tag_checkout_and_scan` (the free function): Convert it to accept `&dyn ShellGateway` and `&dyn ScannerGateway` parameters, or make it a method on the struct. Replace all `crate::shell::run(...)` and `crate::scanner::run_audit(...)` calls.

**New tests**:
- `vulnerability_detected_with_release_tag_scans_and_restores_branch`: Fake shell returns git branch, tag list, successful checkout; fake scanner returns vulnerabilities. Verify correct events and that git checkout is called to restore the branch.
- `post_push_scanner_finds_vulnerabilities`: Fake scanner returns vulnerabilities → verify `vulnerable=true` in emitted event.
- `post_push_scanner_clean`: Fake scanner returns no vulnerabilities → verify `vulnerable=false`.

**Update `main.rs`**: Pass gateways to `AuditReleaseTag::with_registry(...)`.

**Verify**: Quality gates.

#### Step 11: Migrate `AuditMainBranch`

**Changes to `audit.rs`** (`AuditMainBranch`):

1. Add `scanner: Arc<dyn ScannerGateway>` field.
2. Update `new()` to accept the scanner gateway.
3. Replace `crate::scanner::run_audit(...)` with `self.scanner.run_audit(...)`.

**New tests**:
- `scanner_finds_matching_cve_sets_dirty_true`: Fake scanner returns vulnerability with matching CVE → verify `dirty=true`.
- `scanner_finds_different_cve_sets_dirty_false`: Fake scanner returns vulnerability with non-matching CVE → verify `dirty=false`.
- `scanner_unavailable_falls_back_to_payload`: Fake scanner returns error → verify payload values are used.

**Update `main.rs`**: Pass scanner gateway.

**Verify**: Quality gates.

#### Step 12: Migrate `RemediateVulnerability`

**Changes to `remediate.rs`**:

1. Add `shell: Arc<dyn ShellGateway>` field.
2. Update `new()` to accept the gateway.
3. Replace `crate::shell::run(...)` with `self.shell.run(...)`.

**New tests**:
- `successful_hone_emits_success_true`: Fake shell returns successful hone output → verify `success=true` in event payload.
- `hone_failure_emits_success_false`: Fake shell returns exit code 1 → verify `success=false`.
- `hone_spawn_failure_emits_success_false`: Fake shell returns `Err(...)` → verify graceful handling.
- `hone_json_summary_extracted`: Fake shell returns JSON stdout → verify summary is parsed correctly.

The existing `emits_remediation_completed_when_project_found` test (which runs `hone maintain claude /tmp --json` for real) is replaced by these deterministic fakes.

**Update `main.rs`**: Pass shell gateway.

**Verify**: Quality gates.

#### Step 13: Migrate `RunHoneIterate`

**Changes to `hone_iterate.rs`**:

1. Add `shell: Arc<dyn ShellGateway>` field.
2. Update `new()` to accept the gateway.
3. Replace `crate::shell::run(...)` with `self.shell.run(...)`.

**New tests**:
- `successful_iterate_emits_completed_with_success_true`: Fake shell returns success → verify event.
- `successful_iterate_with_maintain_emits_both_events`: Fake shell returns success + maintain=true → verify both `ProjectIterateCompleted` and `MaintenanceRequested` events.
- `failed_iterate_with_maintain_does_not_chain`: Fake shell returns failure + maintain=true → verify only `ProjectIterateCompleted` (no `MaintenanceRequested`).
- `timeout_forwarded_to_shell`: Use the fake's invocation recorder to verify the timeout from `entry.timeout()` is passed through.

The existing `emits_project_iterate_completed_on_success` test (which tries to run real `hone`) is replaced.

**Update `main.rs`**: Pass shell gateway.

**Verify**: Quality gates.

#### Step 14: Migrate `RunHoneMaintain`

**Changes to `hone_maintain.rs`**:

1. Add `shell: Arc<dyn ShellGateway>` field.
2. Update `new()` to accept the gateway.
3. Replace `crate::shell::run(...)` with `self.shell.run(...)`.

**New tests**:
- `successful_maintain_emits_completed`: Fake shell success → verify event.
- `failed_maintain_emits_completed_with_failure`: Fake shell failure → verify `success=false`.

The existing `emits_project_maintain_completed_on_success` test (which tries to run real `hone`) is replaced.

**Update `main.rs`**: Pass shell gateway.

**Verify**: Quality gates.

#### Step 15: Migrate `CommitAndPush`

**Changes to `git_ops.rs`**:

1. Add `shell: Arc<dyn ShellGateway>` field.
2. Update `new()` to accept the gateway.
3. Replace all `crate::shell::run(...)` calls with `self.shell.run(...)`.

**Test migration**: This block has the most extensive real-I/O tests (creating git repos, adding remotes, making commits). Rewrite them:

- `clean_tree_emits_no_events`: Fake shell returns empty stdout for `git status --porcelain` → verify no events.
- `dirty_tree_commits_and_pushes`: Fake shell returns non-empty status, then success for add, commit, push → verify both events emitted.
- `dirty_tree_commits_but_skips_push_when_disabled`: Same fake, but registry has push=false → verify only committed event.
- `git_commit_failure_returns_error`: Fake shell returns success for status and add, but failure for commit → verify error propagation.
- `commit_message_varies_by_trigger_type`: Three sub-tests using fake — verify commit message args contain "iterate", "maintenance", or "remediation" by inspecting the fake's invocations.

Remove the `init_git_repo` helper and all `TempDir`-based git manipulation from this test module.

**Update `main.rs`**: Pass shell gateway.

**Verify**: Quality gates.

#### Step 16: Migrate `CutRelease`

**Changes to `release.rs`**:

1. Add `shell: Arc<dyn ShellGateway>` field to `CutRelease`.
2. Update `new()` to accept the gateway.
3. `run_release` becomes a method or receives the gateway as a parameter.
4. Replace `crate::shell::run(...)` with the gateway call.
5. `WatchPipeline` also needs the gateway for `query_latest_run` (which uses `tokio::process::Command` directly — not even `shell::run`). Either:
   - Route it through the `ShellGateway` (preferred for consistency), or
   - Leave it as-is since `WatchPipeline` already stubs itself when repo is empty (current tests don't exercise real gh CLI calls).

   **Recommendation**: Route `query_latest_run` through the `ShellGateway` for consistency. The `gh` CLI invocation is currently the one place that bypasses `shell::run`, and normalizing it through the gateway makes the entire `blocks/` module consistently testable.

**New tests for `CutRelease`**:
- `successful_claude_cli_emits_release_completed`: Fake shell returns success with "Tagged as v1.2.4" stdout → verify event payload includes `new_tag`.
- `claude_cli_failure_emits_failure`: Fake shell returns exit code 1 → verify `success=false`.

**New tests for `WatchPipeline`** (if `query_latest_run` is migrated):
- `pipeline_completed_success`: Fake returns JSON with `status: "completed"`, `conclusion: "success"`.
- `pipeline_completed_failure`: Fake returns JSON with `status: "completed"`, `conclusion: "failure"`.
- Test would need special handling since `poll_pipeline` loops with sleep — consider adding a configurable poll limit for tests, or using the sequence fake with a "completed" response on the first call.

**Update `main.rs`**: Pass gateways.

**Verify**: Quality gates.

#### Step 17: Migrate `InstallLocally`

**Changes to `install.rs`**:

1. Add `shell: Arc<dyn ShellGateway>` field.
2. Update `new()` to accept the gateway.
3. Replace `crate::shell::run(...)` calls with `self.shell.run(...)`.

**New tests**:
- `command_install_success`: Fake shell returns success → verify event payload.
- `command_install_failure`: Fake shell returns failure → verify `success=false`.
- `brew_install_success`: Fake shell returns success → verify method="brew" in event payload.
- `brew_install_uses_correct_formula`: Inspect the fake's invocations to verify the formula name was passed as an argument to `brew upgrade`.

The existing `command_install_runs_shell_command` test (which runs real `true`) and `command_install_failure_emits_event_with_success_false` (which runs real `false`) are replaced by fakes.

**Update `main.rs`**: Pass shell gateway.

**Verify**: Quality gates.

---

### Part D: Clean Up and Finalize `main.rs` Registration

#### Step 18: Consolidate gateway instantiation in `main.rs`

After all blocks are migrated, `main.rs` should instantiate the production gateways once and pass them to all blocks:

```rust
let shell: Arc<dyn gateway::ShellGateway> = Arc::new(gateway::ProcessShellGateway);
let scanner: Arc<dyn gateway::ScannerGateway> = Arc::new(gateway::ProcessScannerGateway);

engine.register(Box::new(blocks::ValidateProject::new(registry.clone(), shell.clone())));
engine.register(Box::new(blocks::ScanDependencies::new(registry.clone(), scanner.clone())));
engine.register(Box::new(blocks::AuditReleaseTag::with_registry(registry.clone(), shell.clone(), scanner.clone())));
engine.register(Box::new(blocks::AuditMainBranch::new(registry.clone(), scanner.clone())));
engine.register(Box::new(blocks::RemediateVulnerability::new(registry.clone(), shell.clone())));
engine.register(Box::new(blocks::CommitAndPush::new(registry.clone(), shell.clone())));
engine.register(Box::new(blocks::CutRelease::new(registry.clone(), shell.clone())));
engine.register(Box::new(blocks::WatchPipeline::new(registry.clone(), shell.clone())));
engine.register(Box::new(blocks::InstallLocally::new(registry.clone(), shell.clone())));
engine.register(Box::new(blocks::RunHoneIterate::new(registry.clone(), shell.clone())));
engine.register(Box::new(blocks::RunHoneMaintain::new(registry.clone(), shell.clone())));
// These blocks don't use shell/scanner:
engine.register(Box::new(blocks::ComposeGreeting));
engine.register(Box::new(blocks::DeliverGreeting));
engine.register(Box::new(blocks::RouteProjectWorkflow));
```

Blocks that don't need gateways (`ComposeGreeting`, `DeliverGreeting`, `RouteProjectWorkflow`) remain unchanged — they are already pure logic.

#### Step 19: Final quality gate verification

Run the full quality gate suite:

```bash
cargo fmt --all -- --check
cargo clippy --workspace -- -D warnings
cargo test --workspace
```

All must pass. Verify that the test count is at least as high as before (should be higher, since we'll have added edge-case tests).

---

### Part E: Update Documentation

#### Step 20: Update `book/src/guide/writing-task-blocks.md`

Update the guide to show the gateway pattern. The example block should demonstrate:
- Accepting an `Arc<dyn ShellGateway>` in the constructor
- Using `self.shell.run(...)` instead of `crate::shell::run(...)`
- Testing with `FakeShellGateway`

Add a new section "Testing with Fakes" that shows how to construct a fake and verify block decisions.

#### Step 21: Update `book/src/architecture/task-blocks.md`

Add a paragraph under the task block library description explaining the gateway pattern:
- Blocks that perform I/O accept gateway trait objects via their constructor
- Production code injects `ProcessShellGateway` / `ProcessScannerGateway`
- Tests inject fakes that return pre-configured results
- This enables fast, deterministic testing of block decision logic

#### Step 22: Update `book/src/architecture/crates.md`

Under the "Daemon support modules" section, add a bullet for `gateway.rs`:
- `gateway.rs` — gateway traits (`ShellGateway`, `ScannerGateway`) that isolate I/O from block decision logic, plus production implementations and test fakes.

#### Step 23: Build documentation

```bash
mdbook build book/
```

Verify no broken links or compilation errors.

---

### Part F: Final Verification

#### Step 24: Run all quality gates one final time

```bash
cargo fmt --all -- --check
cargo clippy --workspace -- -D warnings
cargo test --workspace
mdbook build book/
```

#### Step 25: Review the diff holistically

Before committing, review the complete diff to verify:
- No block's external behaviour has changed
- No new dependencies have been added (the gateway uses only `std` types + `anyhow`)
- The `shell.rs` and `scanner.rs` modules are unchanged (they are still the real implementations; the gateway just wraps them)
- Every block that previously called `crate::shell::run(...)` now calls through the gateway
- Every test module that previously spawned real processes now uses fakes for the decision-logic tests
- `main.rs` passes production gateways to all blocks

---

### Summary of Changes by File

| File | Change |
|------|--------|
| `Cargo.toml` (workspace) | Add `[workspace.lints.clippy]` and `[workspace.lints.rust]` |
| `crates/foundry-core/Cargo.toml` | Replace lint sections with `[lints] workspace = true` |
| `crates/foundryd/Cargo.toml` | Replace lint sections with `[lints] workspace = true` |
| `crates/foundry-cli/Cargo.toml` | Replace lint sections with `[lints] workspace = true` |
| `crates/foundryd/src/gateway.rs` | **New**: `ShellGateway`, `ScannerGateway` traits + production/test impls |
| `crates/foundryd/src/main.rs` | Add `mod gateway;`, instantiate production gateways, pass to all blocks |
| `crates/foundryd/src/blocks/validate.rs` | Accept `ShellGateway`, rewrite tests with fakes |
| `crates/foundryd/src/blocks/scan.rs` | Accept `ScannerGateway`, rewrite tests with fakes |
| `crates/foundryd/src/blocks/audit.rs` | Accept both gateways, rewrite tests with fakes |
| `crates/foundryd/src/blocks/remediate.rs` | Accept `ShellGateway`, rewrite tests with fakes |
| `crates/foundryd/src/blocks/hone_iterate.rs` | Accept `ShellGateway`, rewrite tests with fakes |
| `crates/foundryd/src/blocks/hone_maintain.rs` | Accept `ShellGateway`, rewrite tests with fakes |
| `crates/foundryd/src/blocks/git_ops.rs` | Accept `ShellGateway`, rewrite tests with fakes |
| `crates/foundryd/src/blocks/release.rs` | Accept `ShellGateway`, rewrite tests with fakes |
| `crates/foundryd/src/blocks/install.rs` | Accept `ShellGateway`, rewrite tests with fakes |
| `book/src/guide/writing-task-blocks.md` | Add gateway pattern and testing with fakes |
| `book/src/architecture/task-blocks.md` | Add gateway pattern explanation |
| `book/src/architecture/crates.md` | Add `gateway.rs` description |

### What Does NOT Change

- `crates/foundryd/src/shell.rs` — untouched (the gateway wraps it)
- `crates/foundryd/src/scanner.rs` — untouched (the gateway wraps it)
- `crates/foundryd/src/engine.rs` — untouched (engine operates on `TaskBlock` trait, unchanged)
- `crates/foundry-core/` — no changes (the traits live in `foundryd`, not `foundry-core`, because they depend on `CommandResult` and `AuditResult` which are `foundryd` types)
- `crates/foundry-cli/` — untouched (CLI is a gRPC client, no block logic)
- `crates/foundryd/src/blocks/greet.rs` — untouched (no I/O)
- `crates/foundryd/src/blocks/route_project.rs` — untouched (no I/O)
- `crates/foundryd/src/blocks/hone_common.rs` — untouched (pure parsing logic)

### Risks and Mitigations

| Risk | Mitigation |
|------|-----------|
| Changing block constructors breaks engine tests | Engine tests use `TestObserver`/`TestMutator` which don't use gateways — unaffected |
| New gateway trait has wrong lifetime bounds | The `Pin<Box<dyn Future + Send + '_>>` pattern matches `TaskBlock::execute` exactly — proven to work |
| FakeShellGateway becomes brittle with sequence mode | Provide both `always()` (simple) and `sequence()` (multi-call) constructors; most tests use `always()` |
| Missing edge in a block's shell call graph | Each block migration step includes reading the full block source and mapping every `crate::shell::run` call |
| `WatchPipeline`'s `query_latest_run` bypasses `shell::run` | Explicitly called out in Step 16 with a recommendation to route it through the gateway |