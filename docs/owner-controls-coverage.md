# Owner-control coverage investigation

This record accompanies the correction to the preserved owner-control work at
`687bb2087804b9a40f05ce2563928836d34e98d8`, whose base is
`1fc31ca124651994b0679de3e902b9d8a17f5721`. Foundry will assign the resulting
commit after reviewing these uncommitted changes. The reported blocker was a
coverage-run segfault in `campaign_resume_generated_client`.

## Controlled reproduction

The comparison used an archive of the base, without changing Git refs:

```bash
git archive 1fc31ca -o /tmp/foundry-base.tar
mkdir -p /tmp/foundry-base
tar -xf /tmp/foundry-base.tar -C /tmp/foundry-base
```

Both trees retain edition 2024, resolver 3, MSRV 1.88, toolchain 1.94.0,
Tokio, the same lockfile, feature semantics and coverage configuration.
The campaign-resume test is byte-identical across the two trees (SHA-256
`8ee63eaa2d0127c5c9b614fb46cbdb3d68965f8b9744913e09f7940d9e523d40`);
the campaign RPC implementation is also unchanged by the owner-control commit.

The host is Linux x86_64, kernel `6.8.0-142-generic`, with Tarpaulin 0.37.2.
Its initial `RUSTUP_TOOLCHAIN=stable` and Mise shims selected rustc 1.96.1,
even when setting `RUSTUP_TOOLCHAIN=1.94.0`. Prepending the rustup proxies to
PATH selected the actual repository pin, verified as
`rustc 1.94.0 (4a4ef493e 2026-03-02)`:

```bash
export PATH="$HOME/.cargo/bin:$PATH"
export RUSTUP_TOOLCHAIN=1.94.0
rustc --version
cargo tarpaulin --version
cargo tarpaulin -p foundryd --test campaign_resume_generated_client
```

Run that isolated command in each tree with separate target directories.
The diagnostic runs used `--target-dir` beneath
`/tmp/foundry-settlement-evidence/` to avoid collisions with the full gate.
Each run rebuilt the target using Tarpaulin's existing configuration and
default Linux ptrace backend. Isolation is diagnostic only; it does not
replace the required workspace gate.

| Tree | Compiler | Backend | Result |
|------|----------|---------|--------|
| Preserved work, initial run | 1.96.1 | ptrace | All 6 tests passed |
| Preserved work, repeat with shim override still active | 1.96.1 | ptrace | All 6 tests passed |
| Base, same shim environment | 1.96.1 | ptrace | All 6 tests passed |
| Preserved work, verified pin | 1.94.0 | ptrace | All 6 tests passed |
| Preserved work, verified-pin repeat | 1.94.0 | ptrace | All 6 tests passed |
| Base, verified pin | 1.94.0 | ptrace | All 6 tests passed |
| Preserved work, backend comparison | 1.94.0 | LLVM | All 6 tests passed |

The LLVM comparison used `--engine llvm` after installing the pinned
toolchain's `llvm-tools-preview` component. It did not change the repository
configuration. Tarpaulin documents both backends in its
[upstream README](https://github.com/xd009642/tarpaulin#readme).

## Evidence and correction

The reported segfault has not reproduced in these controlled isolated runs.
There is no established application, test-lifecycle or runner defect to fix.
The correction records reproducible diagnostics and adds the missing required
coverage command to the development instructions. Campaign-resume behaviour,
tests and runner configuration remain unchanged; no speculative crash cause
is asserted.

The preserved owner-control tests remain intact: generated clients talk to
the real `FoundryService`, and actual CLI tests exercise the executable. Their
exact target/sibling identity, operator context, preserved disposition,
Watch/log identity, prior log bytes, typed refusal, persistence-failure and
concurrent-settlement assertions have not been weakened.

## Full verification

On 2026-09-30 the exact required command passed with the verified 1.94.0
toolchain and unchanged `tarpaulin.toml`:

```bash
cargo tarpaulin --workspace --fail-under 61
```

Exit status was **0**, with **87.31% coverage (22,159 / 25,380 lines)**.
The full run executed 38 test executables: 2,877 tests passed, none failed,
and three existing ignored tests remained ignored. No test was excluded or
newly ignored, no crash was suppressed, and no gate command, required status,
workspace scope or threshold was changed. The generated campaign-resume
client's six tests all passed within this run, as did these real-boundary
owner-control tests in `queue_cli`:

- `owner_controls_cover_every_state_and_preserve_exact_identity_and_evidence`
- `owner_control_faults_leave_ledger_and_event_bytes_untouched`
- `concurrent_owner_controls_settle_once_and_keep_unrelated_writes`
- `actual_cli_owner_controls_forward_origin_and_never_write_client_files`

All other checks exited 0 on the same worktree and pinned compiler:

| Check | Result |
|-------|--------|
| `cargo fmt --all -- --check` | Passed |
| `cargo clippy --workspace --all-targets -- -D warnings` | Passed, including the required workspace lint scope |
| `cargo test --workspace` | 2,879 passed, 0 failed; includes doctests |
| `RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps` | Passed |
| `cargo deny check` | Advisories, bans, licences and sources passed |
| `cargo audit` | Passed; scanned 207 locked dependencies |
| `mdbook build book` | Passed; existing unclosed HTML-tag warnings remain |

Independent build checks used separate temporary target directories to avoid
colliding with Tarpaulin's clean/build cycle. The full coverage gate used the
worktree's normal target directory. Raw logs are in
`/tmp/foundry-settlement-evidence/`; these SHA-256 digests identify the
recorded coverage evidence:

| Log | SHA-256 |
|-----|---------|
| `preserved-194-ptrace.log` | `30b92d2eb5e0b5ce6cf2f997f68d5c8f68af55042e5a3c73811686f9f2b7439e` |
| `preserved-194-ptrace-repeat.log` | `ca66d8d52cf17441be0d151d53ac48868a90699ba07d266eb60b0f205f2e401c` |
| `base-194-ptrace.log` | `3e691d2ef85b233fc9369dc401c236ace983a0da51e8bf05e2ef4b1d98a4db22` |
| `full-ptrace.log` | `c6dbde378d6e8f69ca4692d4f470396b4bf5c3f8be3e3187da325b9acf898c5a` |

The initial segfault therefore remains **unreproduced**, including in the
successful full required gate. These results establish current acceptance
evidence, rather than a claim to have identified or repaired an intermittent
segfault. Application code, test lifecycle and coverage execution cannot be
assigned a cause without a failing reproduction.
