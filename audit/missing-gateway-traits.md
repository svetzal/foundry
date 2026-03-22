Now I have a thorough understanding of the project. Let me formulate my assessment.

```json
{ "severity": 3, "principle": "Functional core, imperative shell", "category": "Architecture / Testability" }
```

## Assessment

Foundry is a well-engineered project. The edition/MSRV/workspace-dependency/lint discipline is strong. The engine is cleanly separated via the `TaskBlock` trait. Error handling is consistent. The test count (~182) is healthy. There's a lot to admire here.

But the principle the codebase most violates is **"Functional core, imperative shell"** — specifically, the absence of gateway traits isolating I/O within the task blocks themselves.

---

### The Problem

Every task block that performs I/O calls `crate::shell::run()`, `crate::scanner::run_audit()`, or `std::path::Path::exists()` as **hard-wired concrete functions**. There is no trait boundary between the block's *decision logic* and its *side effects*.

For example, in `validate.rs`:

```rust
// Line 50 — direct call to a concrete function
crate::shell::run(path, "git", &["rev-parse", "--abbrev-ref", "HEAD"], None, None).await?;
```

In `remediate.rs`:

```rust
// Line 101 — again, hard-wired
crate::shell::run(Path::new(project_path), "hone", &["maintain", agent, project_path, "--json"], None, None).await;
```

In `scan.rs`:

```rust
// Line 60 — and again
crate::scanner::run_audit(path, &entry.stack).await?;
```

The engine level is well-designed — it holds `Vec<Box<dyn TaskBlock>>` and tests use `TestObserver`/`TestMutator` fakes that implement the trait with pure logic. That's textbook functional core. But the *real* blocks — the ones doing the actual work — have no such seam.

### How This Manifests in Tests

The existing block tests reveal the cost:

1. **`validate.rs`** — Tests call real `git` on the workspace root or on `tempfile` directories with `init_git_repo()`. These are integration tests masquerading as unit tests. The test at line 288 even discovers the current branch at runtime with `std::process::Command::new("git")` and bails early if in detached HEAD.

2. **`scan.rs`** — The `scans_known_project` test runs a real scanner against `/tmp` and asserts only `result.success`. The comment literally says *"Scanner tool likely not installed in test env — should handle gracefully."* This test verifies "it doesn't crash," not "it makes the right decisions."

3. **`remediate.rs`** — `emits_remediation_completed_when_project_found` runs against `/tmp` and implicitly depends on `hone` being absent. If `hone` *is* installed (likely on this developer's machine), it would **actually invoke `hone maintain claude /tmp --json`** — a real side effect in a unit test.

These tests can't verify edge cases like "what does the block do when git returns exit code 128?" or "what if the scanner finds 3 CVEs with specific severities?" without actually producing those conditions in the real environment.

---

### How to Correct It

Introduce a **gateway trait** that wraps shell command execution, and inject it into the blocks that need I/O:

```rust
// In foundry-core or foundryd — a thin trait at the I/O boundary
#[async_trait::async_trait]  // or use manual Pin<Box<...>> to stay async_trait-free
pub trait ShellGateway: Send + Sync {
    async fn run(
        &self,
        working_dir: &Path,
        command: &str,
        args: &[&str],
        env: Option<&[(String, String)]>,
        timeout: Option<Duration>,
    ) -> anyhow::Result<CommandResult>;
}
```

The **production implementation** is a trivial wrapper around the existing `shell::run()` — a gateway struct with no logic to test. The **test implementation** is a fake that returns pre-configured `CommandResult` values, letting you verify block decision logic in microseconds without touching the filesystem or spawning processes.

Then blocks become:

```rust
pub struct ValidateProject {
    registry: Arc<Registry>,
    shell: Arc<dyn ShellGateway>,  // injected
}
```

And tests can say:

```rust
let shell = Arc::new(FakeShell::returning(CommandResult {
    stdout: "main\n".into(),
    stderr: String::new(),
    exit_code: 0,
    success: true,
}));
let block = ValidateProject::new(registry, shell);
// Now test the block's DECISION logic, not git's behavior
```

---

### Secondary Finding: Lint Configuration Duplication

The identical `[lints.clippy]` and `[lints.rust]` blocks are repeated verbatim across all three `Cargo.toml` files. This is a textbook **"No knowledge duplication"** violation — any lint policy change requires three synchronized edits. The fix is straightforward:

```toml
# workspace Cargo.toml
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

```toml
# each crate's Cargo.toml
[lints]
workspace = true
```

This is severity 2 — easy to fix, low risk, but a clear duplication of a single decision across three files.

---

### Summary

| Concern | Principle | Severity |
|---|---|---|
| Task blocks hard-wire I/O with no gateway trait | Functional core, imperative shell | 3 |
| Lint config duplicated 3× | No knowledge duplication | 2 |

The gateway trait introduction is the highest-value improvement. It would make the block tests fast, deterministic, and capable of covering edge cases that currently can't be reached — all without changing any block's external behaviour.