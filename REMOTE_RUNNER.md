# Remote runners

A plan to let other machines register with `foundryd` as runners, and to let
the daemon hand them jobs that run in a fresh clone of the project at a named
commit. The daemon keeps its head (events, chains, pacing, campaigns, review,
landing). Runners become its hands. The daemon's own host becomes one runner
among several, and the registered working tree stops being Foundry's workspace.

Status: draft for Stacey's review, 2026-10-17. Nothing here is built.

## 0. Why

Three facts from the last week of `mojentic-sw` on ops-01:

1. **The work cannot run where the daemon is.** The Darwin `URLSession` timeout
   verification needs macOS. Every managed host is Linux. The campaign
   `mojentic-3-0-sw` burned cycles until an executor wrote a blocker asking
   "Can you provide a macOS runner for the Swift timeout diagnosis?", with the
   options "Register a macOS runner" or "Drop Darwin timeouts". Stacey's
   decision on 2026-10-17 was to hold the member and move the Darwin work to an
   interactive session on the Mac. That is the right call today and the wrong
   shape for the system: a platform gap became an owner interrupt.
2. **The gate environment and the development environment are the same tree.**
   `mojentic-sw-transient-recovery-v2` escalated on 2026-10-10 because the
   task stage forbids ref mutation while the project's own conformance doc
   demands `pull --rebase` before coding, and because `swiftformat` was absent
   on the host that ran the gates. Reviews on 2026-10-11 found required gates
   timing out with a file-descriptor limit of 1,024 where stored passing runs
   had 8,192. Those are properties of one long-lived checkout on one host, not
   of the code.
3. **GitHub Actions is the only place the Swift work is qualified**, and the
   registry note for `mojentic-sw` says so: "macOS/iOS validation is covered by
   CI." Hosted macOS minutes are the expensive kind, we already run out of
   runner capacity, and the Mac sits here with Swift 6.4 on it.

The general statement: Foundry assumes one host, one checkout per project, and
that every stack it manages can be built and tested there. The registry has
no notion of platform, pacing's "host at capacity" means "this daemon", and
`ProjectEntry.path` is an absolute path on the daemon's disk that every block
reads. The assumption held while the fleet was Rust, Python, TypeScript and
Elixir. Swift broke it, Kotlin with Android tooling will break it again, and
anything that needs a GPU (Odin) is already outside it.

Doing it the GitHub Actions way, with a fresh clone per job on a machine that
advertises what it can run, fixes the three facts at once and removes the
"dirty source tree" class of failures with them. It also brings two costs
Stacey named, which this plan takes on rather than hides: every clone must be
current when work starts, and multi-cycle work that lands on trunk between
cycles needs a way to ship dark. Section 3.7 and section 5 cover those.

## 1. What exists today

Anchors are in `crates/` unless noted.

### 1.1 Where work runs

- `ProjectEntry.path` (`foundry-sdk/src/registry.rs:103`) is "absolute path to
  the project on the local filesystem". Every block resolves the project to
  `Path::new(&entry.path)`: `resolve_gates.rs:167`, `run_preflight_gates.rs:122`,
  `execution.rs:225`, `git_ops.rs:109`, `release/mod.rs`.
- Nightly maintain and iterate run **in the registered checkout**. The module
  doc says it outright: "works directly in the registered checkout
  (`entry.path`), not in a disposable worktree" (`checkout_sync.rs:3`).
  `sync_checkout` refuses a dirty tree, fetches, and fast-forwards; it never
  stashes or rebases (`checkout_sync.rs:130-194`).
- Tasks and campaign cycles get a **git worktree** under `~/.foundry/worktrees`,
  created from `FETCH_HEAD` and sharing the registered repo's object store
  (`task_workspace.rs:187-243`). It is isolation of the index, not of the host.
- Agents run with `remote.origin.pushurl=foundry://agent-push-disabled`
  (`push_guard.rs:55`), and `branch_guard.rs` moves the configured branch
  forward only if the result is a fast-forward. Landing authority is the
  daemon's, and the plan keeps it there.
- The one real `git clone` in production is the initiative integration gate
  (`steer_initiative/integration_runner.py`): it clones `--no-hardlinks
  --no-checkout` into a temp dir, detaches at the expected sha, removes
  `origin`, runs the command under `foundry capture`, and returns JSON. It can
  run on a named host over `ssh -o BatchMode=yes`
  (`steer_initiative/integration.rs:59-130`). That is a remote runner in
  miniature: named host, fresh clone, pinned sha, evidence returned. It has no
  registration, no labels, no scheduling, and it needs a second `foundryd` on
  the far host for registry and pacing reads.

### 1.2 The seams

- `ShellGateway::run(working_dir, command, args, env, timeout) -> CommandResult
  {stdout, stderr, exit_code, success}` (`foundry-sdk/src/gateway/shell.rs:23`).
  No stdin, no streaming, output held in memory.
- `AgentGateway::invoke(&AgentRequest) -> AgentResponse`
  (`gateway/agent.rs:699`). `AgentRequest` carries `working_dir: PathBuf`,
  `agent_file: Option<PathBuf>`, provider, tier, effort, access, env, timeout,
  trace id. `AgentFailureMetadata.source_log_path` is a host-local path.
- `AgentStreamRunner::run(StreamRun{working_dir, command, args, env, stdin,
  timeout, log_path})` (`foundry-blocks/src/agent_stream.rs:55`) tees stdout to
  a session log and returns lines after exit. Nothing reads them live.
- `TaskBlock::execute` is the block seam SDK.md plans to put on the wire as
  "Phase 2 block workers": a worker registers `RegisterBlock{name, sinks_on}`,
  the daemon sends `BlockDispatch`, the worker returns `BlockResult`
  (`SDK.md:226-246`).
- About twenty block files construct their own `ProcessShellGateway` through
  `task_block_new!` rather than taking an injected one; `scanner.rs`,
  `work_reconcile.rs:464` and `work_supersession.rs:14` call `shell::run`
  directly; blocks read `.hone-gates.json`, `AGENTS.md`, disk space and agent
  files straight from the filesystem. Any seam below the job level leaks.

### 1.3 Scheduling

- `DurableWorkAdmission` records every engineering root as a `WorkItem` with a
  `kind` and a `lane` (`foundryd/src/work_admission.rs:285-310`). Lanes are
  `Interactive`, `Campaign`, `Maintenance`.
- Pacing checks, in order: one mutating item per repository, `max_running` per
  host (default 2, ops-01 runs 4), `not_before`, `depends_on`, lane paused
  (`foundry-sdk/src/pacing.rs:1-36`). "Host" is this daemon.
- `PacingScheduler::tick` moves items to running and calls `spawn_workflow`
  (`foundryd/src/pacing.rs:89-336`). Nothing in `WorkItem`, the registry or the
  engine names a host, platform or architecture. The only OS checks are
  `InstallLocally` skipping brew off macOS and the per-host
  `FOUNDRY_GATE_TIMEOUT_SCALE`.

### 1.4 Gates and evidence

- `.hone-gates.json` is `{gates:[{name, command, required, timeout?,
  fix_command?}]}` (`gates.rs:8-44`), read from the checkout. `run_gates` runs
  each through `sh -c` with `CARGO_INCREMENTAL=0` and a 600 s default scaled by
  the host (`gate_runner.rs:86-243`).
- Since 0.53.0 a failed gate keeps a 200-line split tail in the payload and
  full stdout and stderr under `~/.foundry/evidence/gate-logs/<run>/`, capped at
  8 MiB per stream, pruned at 14 days or 1 GiB (`gate_runner/logs.rs`). The
  absolute paths are written into `GateResult.logs`. On a remote host those
  paths point at nothing the daemon can read.

### 1.5 Control plane

- One gRPC service, plaintext, no auth, `127.0.0.1:50051` by default,
  `0.0.0.0:50051` on ops-01 behind the LAN firewall
  (`book/src/guide/trusted-lan-control-plane.md`). `Watch` is the only
  streaming RPC and it is server-side only, from an in-memory broadcast with no
  replay (`service/eventing_ops.rs:595-625`).
- `IDEMPOTENCY.md` designs an `effect_key` ledger whose enforcement is gated on
  "the first redelivery source: remote-worker dispatch retry (Phase 2)"
  (`IDEMPOTENCY.md:345-356`). This plan is that source.
- `OBSERVABILITY.md` phase 1 wants a 15-second heartbeat from running agent
  work and a live activity record. A runner streaming job output is the
  natural producer of both.

### 1.6 What `mojentic-sw` needs, concretely

Six gates, all through `./scripts/with-gate-environment`: `swift format lint`,
`swiftlint --strict`, `swift build -c release`, `swift test --parallel`,
`swift test --traits full`, and DocC generation. Linux can run the Swift
toolchain for most of them. Darwin-only behaviour (`URLSession` timeouts) and
Apple platform validation need macOS, and the Mac has Swift 6.4 on arm64.
ops-01 is x86_64 Linux with `max_running: 4`. The project is checked out on
both machines at the mirrored path
`~/Work/Projects/Personal/mojentic-unify/mojentic-sw`.

## 2. The target

A `foundry-runner` process on any machine we own connects to the daemon,
says what it is (`os=macos arch=arm64 toolchain.swift=6.4`) and how many jobs
it will take at once, and waits. The daemon turns each piece of executable work
(a gate run, an agent execution with its verify gates, a release build) into a
**job**: project, clone source, exact commit, the steps to run, the evidence to
bring back. It picks a runner whose labels satisfy the job's `runs_on`, streams
the job over, and receives log lines while it runs, then the result and any
commits as a git bundle. The daemon lands the bundle on trunk the way it lands
a worktree today: fast-forward only, re-verified if trunk moved.

The runner never pushes. It needs read access to the repository and nothing
else. The clone is created for the job and deleted after it, unless the job
asked for it to be preserved for a human to pick up (the Darwin case).

The daemon's host runs the same `foundry-runner` code in-process as the
default runner, labelled from its own `uname`. There is one execution path,
not a local one and a remote one. Projects with no `runs_on` run anywhere;
`mojentic-sw` gets `runs_on: [os=macos]` for its Darwin gates and nothing else
changes for it.

Done looks like this for the motivating case: `foundry registry edit
mojentic-sw --runs-on os=macos`, a `foundry-runner` launchd agent on the Mac,
and the next `mojentic-3-0-sw` cycle runs its gates on the Mac with the full
logs visible from ops-01, without an owner question.

What this plan does not do:

- It does not make Foundry a CI service for the public. Trusted LAN, trusted
  runners, same posture as the control plane today.
- It does not move landing, review or campaign reasoning off the daemon.
- It does not decide how in-flight features ship dark (section 5). It states
  what the runner model requires of that decision.
- It does not replace GitHub Actions. It stops us needing hosted macOS minutes
  to find out whether Swift work is sound, and it lets release artifacts be
  built on the host that will run them, which the build-host rule in
  Operations already asks for.

## 3. Design

### 3.1 Vocabulary

- **Runner.** A process that executes jobs in fresh clones on one machine. It
  has a name, labels and a capacity.
- **Label.** A `key=value` fact about a runner: `os`, `arch`,
  `toolchain.<name>`, `gpu`, and free-form tags.
- **`runs_on`.** A set of label constraints a job must satisfy. All must match.
- **Job.** One unit of execution handed to a runner: clone source, commit,
  steps, evidence request, timeout.
- **Step.** A shell command or an agent invocation inside a job, run in the
  clone.
- **Bundle.** A `git bundle` of the refs the job produced, returned to the
  daemon.
- **Local runner.** The runner compiled into `foundryd`, labelled from the
  daemon host.

### 3.2 The job is the unit, not the block or the process

Three seams were available (section 1.2). The job level is chosen because:

- Block-level dispatch (SDK.md Phase 2) would move a `TaskBlock` to the
  runner, and blocks read the registry, the gates file, agent files and
  evidence paths on the daemon's disk. Every block would need its inputs
  marshalled; twenty of them build their own shell gateway. The leak is the
  whole block layer.
- Process-level dispatch (a remote `ShellGateway`) keeps `working_dir` as the
  contract, and the working directory is the thing that no longer exists on
  the daemon. Each gate would be its own round trip with no shared clone.
- A job carries everything the runner needs and nothing the daemon owns. The
  daemon stays the only reader of the registry and the only writer of events.
  The runner's state is a mirror cache and a job directory. It can be killed
  and reinstalled without losing anything but a running job, which redelivers.

SDK.md Phase 2 is not displaced. Block workers remain the way to run a
*different block* elsewhere. Runners are the way to run *this project's
commands* elsewhere. The two share the registration and heartbeat shape
(section 3.4) so Phase 2 can reuse it.

### 3.3 Job specification

Defined in `foundry-sdk/src/job.rs`, mirrored in `proto/foundry.proto`,
schema in `contracts/job-v1.json`.

```json
{
  "id": "job_7f3a…",
  "work_item": "wi_27e073dcfb788671f58c9969",
  "trace": { "trace_id": "…", "span_id": "…" },
  "project": "mojentic-sw",
  "source": {
    "url": "https://github.com/svetzal/mojentic-sw.git",
    "ref": "436c334d63c5477dbf9dda59ba1b66b58319124a",
    "branch": "main",
    "continuation": null
  },
  "runs_on": { "os": "macos" },
  "workspace": { "preserve": "on_failure" },
  "env": { "FOUNDRY_TRACE_ID": "…" },
  "steps": [
    { "kind": "gates", "names": ["test", "test-full-traits", "docs"] },
    { "kind": "agent", "provider": "codex", "tier": "balanced",
      "access": "full", "prompt_ref": "blob:sha256-…", "timeout_secs": 3600 },
    { "kind": "gates", "names": "*" }
  ],
  "evidence": {
    "bundle_refs": ["HEAD"], "gate_logs": "failed", "transcript": true
  },
  "timeout_secs": 7200
}
```

Notes on the shape:

- `source.ref` is always a commit, never a branch name. The daemon resolves it
  at dispatch from its mirror (section 3.6). `execution_requested` already
  carries `base_ref` as a sha, so this is a promotion of an existing field.
- `continuation` is the existing `bundle:<path>` idea made explicit: a bundle
  the daemon sends with the job, which the runner fetches into the clone and
  rebases onto `source.ref`. It is how a preserved cycle resumes on any runner.
- A `gates` step names gates from the clone's own `.hone-gates.json` at
  `source.ref`. The runner reads the file; the daemon does not need a checkout
  to know the gate names because it can `git show <ref>:.hone-gates.json` from
  the mirror. `"*"` is all gates.
- An `agent` step is today's `AgentRequest` with `working_dir` removed, the
  agent file sent as content (`prompt_ref` and `agent_file_ref` point at blobs
  shipped with the job), and provider configuration taken from the runner
  host's own `~/.foundry/agents.json`. Credentials for the provider live on the
  runner host in its secret store, as they do today on ops-01.
- `evidence.bundle_refs` asks for a bundle of the clone's `HEAD` relative to
  `source.ref`. An empty bundle (no commits) is a valid result.
- `workspace.preserve` is `never`, `on_failure` or `always`. A preserved clone
  stays under the runner's job directory with its path reported in the result,
  which is what the Darwin decision needed: a tree on the Mac at the preserved
  commit, ready for an interactive session.

### 3.4 Protocol

One new bidirectional streaming RPC on the existing service, dialled **from
the runner to the daemon**. The Mac moves between networks and sits behind
NAT and the Teleport or WireGuard tunnel; ops-01 is reachable from everywhere
we work. Outbound-only from the runner also means a runner host opens no
port.

```proto
rpc RunnerSession(stream RunnerMessage) returns (stream DaemonMessage);

message RunnerMessage {
  oneof body {
    Register register = 1;      // name, labels, capacity, version, token
    Heartbeat heartbeat = 2;    // running job ids, load
    JobAccepted accepted = 3;   // job id, clone started
    JobLog log = 4;             // job id, step, stream, line, at
    JobStepResult step = 5;     // job id, step index, GateResult or outcome
    JobArtifact artifact = 6;   // job id, kind, chunk
    JobResult result = 7;       // job id, success, summary, preserved_path?
    JobRefused refused = 8;     // job id, reason
  }
}

message DaemonMessage {
  oneof body {
    Registered registered = 1;  // runner id, daemon version, heartbeat secs
    Dispatch dispatch = 2;      // Job plus blobs (prompt, agent file, bundle)
    Cancel cancel = 3;          // job id, reason
    Drain drain = 4;            // finish running jobs, accept no more
  }
}

Artifact kinds are `bundle`, `gate_log` and `transcript`. Refusal reasons are
`labels`, `at_capacity`, `clone_failed` and `continuation_conflict`.
```

Rules:

- **Registration token.** `foundry runner token new <name>` on the daemon
  mints a token and stores its hash in `~/.foundry/runners.json`. The runner
  reads its copy from the host's secret store (Keychain on the Mac, systemd
  credential on Linux) and sends it in `Register`. No token in a file, a flag
  or an environment variable, per the Operations credentials rule. The token
  authorises *being a runner*; which jobs it receives is decided by labels.
  This is not TLS and does not pretend to be. It stops a stray process on the
  LAN from accepting jobs, which plaintext gRPC alone does not.
- **Heartbeat** every 15 s (the OBSERVABILITY.md interval). Three missed beats
  mark the runner `offline`; its running jobs become `lost` and redeliver
  (section 3.8). Heartbeats do not enter the durable event log.
- **Log lines** stream as they happen. The daemon writes them to the job's
  evidence directory under `~/.foundry/evidence/jobs/<job>/` and feeds the
  activity record. This is the "show that an agent session is alive" backlog
  item, satisfied for runner-executed work.
- **Artifacts** are chunked at 1 MiB. A bundle larger than a configurable cap
  (default 256 MiB) fails the job with a typed reason rather than streaming
  forever.
- **Cancel** kills the step's process group and deletes the clone unless
  `preserve` says otherwise. `Drain` is how `foundry runner drain <name>`
  and the Mac's sleep hook stop a runner cleanly.

### 3.5 Runner behaviour

`foundry-runner` is a new binary crate, `crates/foundry-runner`, depending on
`foundry-sdk` only. Its state lives under `~/.foundry/runner/`:

```text
~/.foundry/runner/
  mirrors/<owner>/<repo>.git     bare mirror, fetched before every job
  jobs/<job id>/                 the clone; removed after unless preserved
  logs/                          runner's own log
```

Per job:

1. `git -C mirrors/<repo>.git fetch --prune origin`. If the mirror is absent,
   `git clone --mirror`. Failure refuses the job with `clone_failed`.
2. `git clone --reference mirrors/<repo>.git <url> jobs/<id>` then
   `checkout --detach <ref>`. Objects come from the local mirror through the
   alternates file, so a clone of a large repository costs seconds. Fresh
   working tree, fresh index, no stale state possible.
3. Apply `continuation` if present: fetch the bundle, `rebase --onto <ref>`.
   A conflict refuses the job with `continuation_conflict` and the daemon
   decides, as `prepare_task_workspace` does today.
4. Set `remote.origin.pushurl=foundry://runner-push-disabled` and the job's
   env. Run the steps in order in the clone. Gate steps reuse `gate_runner`
   with its stream capture; an agent step reuses the provider adapters from
   `foundry-blocks` moved to a crate both can depend on (section 4, phase 2).
5. Stream logs during, step results after each step.
6. `git bundle create` of `source.ref..HEAD` and ship it, with gate logs and
   the transcript, then the result.
7. Delete the clone, or report its path if preserved.

The runner also enforces its capacity: a dispatch beyond capacity is refused
with `at_capacity` and the daemon requeues. The Mac's runner defaults to
capacity 1 because the same machine hosts Stacey's interactive sessions.

The daemon's **local runner** is the same crate linked into `foundryd`,
driving an in-memory channel instead of gRPC. Its labels come from
`std::env::consts::{OS, ARCH}` plus detected toolchains. It has capacity equal
to pacing's `max_running`. Dirty-tree mode, the current behaviour, is kept
during migration as `workspace.mode = "registered_path"`, which the local
runner alone accepts (section 4).

### 3.6 Daemon changes

**Registry.** `ProjectEntry` gains `runs_on: BTreeMap<String, String>`
(default empty: any runner) and `source_url: Option<String>` (default derived
from `repo` as `https://github.com/<repo>.git`). `path` stays and changes
meaning over the phases: in phase 1 it is still where the local runner works;
from phase 3 it is informational, the human's checkout, which Foundry no
longer touches. `.hone-gates.json` may give a gate its own `runs_on`, so one
project can have Linux gates and macOS gates:

```json
{ "name": "test-darwin", "command": "swift test --traits darwin",
  "required": true, "runs_on": { "os": "macos" } }
```

**Mirrors.** The daemon keeps its own bare mirrors under `~/.foundry/mirrors/`
for resolving `source.ref`, reading `.hone-gates.json` at a commit, applying
bundles, and landing. This replaces reading those from `entry.path` and is
what lets `path` become informational.

**Runner registry.** `~/.foundry/runners.json` holds the known runners: name,
token hash, labels last seen, capacity, state (`online`, `draining`,
`offline`), last heartbeat, running jobs. `foundry runner list|show|drain|
remove|token` operate on it. Connected sessions live in memory in
`foundryd/src/runners/`.

**Scheduling.** `WorkItem` gains `runs_on` (copied from the project and the
job) and `runner: Option<String>` once started. Pacing adds two checks after
the existing five, a sixth and a seventh:

- A runner exists whose labels satisfy `runs_on`. If none is registered at
  all, the item waits with the reason `no runner for os=macos`, visible in
  `foundry queue` and the ops digest. It never falls back to a non-matching
  runner.
- That runner has capacity. "Host at capacity n/m" becomes "runner NAME at
  capacity n/m".

"One mutating item per repository" is unchanged and is enforced by the
daemon, not the runner, so two runners never work the same trunk at once.

**Blocks.** `ExecuteMaintain`, `ExecutePlan` and `RunVerifyGates` (and the
release blocks in phase 3) stop running commands and instead build a `Job`,
submit it to the runner service, and await the result. The daemon-side
`RunJob` call is one async function, so the block code shrinks. The chain of
events is unchanged; the payloads gain `job_id`, `runner` and evidence paths
that now point into `~/.foundry/evidence/jobs/`. `GateResult.logs` keeps its
shape and starts pointing at daemon-local copies.

**Landing.** After a successful job, `FinalizeTask` (or the maintain chain's
`CommitAndPush`) fetches the bundle into the mirror, checks it is a
fast-forward of the current trunk, and pushes. If trunk moved during the job,
the daemon rebases the bundle's commits onto the new trunk in a temporary
worktree of the mirror and dispatches a **verify job** with the same `runs_on`
before pushing. This is `verify_rebased_commits` (`git_ops.rs:299-323`) made
runner-aware. A rebase conflict preserves the bundle and asks, as today.

**Events.** Following the naming conventions:

| Event | Kind | Payload |
| --- | --- | --- |
| `runner_registered` | domain fact | name, labels, capacity, version |
| `runner_departed` | domain fact | name, reason, jobs lost |
| `runner_job_requested` | command | the job spec minus blobs |
| `runner_job_started` | lifecycle | job id, runner, clone commit |
| `runner_job_completed` | lifecycle | job id, runner, success, step results |

A departure reason is `drain`, `heartbeat_lost` or `disconnect`. A completed
job also records its bundle sha, preserved path and evidence directory.
Heartbeats and log lines are not events; they go to the activity record.

### 3.7 Keeping clones current

Stacey's first consequence: more clones means more rebasing. The design makes
most of it disappear and names what is left.

- **Runner clones are never stale** because they do not persist. Each job
  fetches the mirror and clones at the commit the daemon chose at dispatch.
  There is no second-host checkout that "lags", which the maintenance guide
  warns about today.
- **The daemon's mirrors** fetch before every dispatch and before every
  landing. They are bare; nothing can be dirty.
- **Trunk moving during a job** is the real residual, and it grows with job
  length and runner count. It is handled at landing by rebase plus a verify
  job (section 3.6). Pacing's one-mutating-item-per-repository rule keeps
  Foundry from racing itself; the moves come from Stacey's interactive
  commits. A `verify` job after rebase is cheap for gates, expensive if the
  agent must be re-run, which it is not: the rebased commits are re-verified,
  not re-authored.
- **Human checkouts on the Mac and ops-01** are the one thing this plan takes
  Foundry *out of*. From phase 3 the registered `path` is never written by
  Foundry, so the "dirty tree refused" and "diverged" failures stop. Keeping
  those checkouts current remains the Operations git-hygiene rule (fetch and
  rebase before work), which a `SessionStart` hook already does for the
  Operations repo and could do for project repos.

### 3.8 Failure and redelivery

- **Runner lost mid-job.** Jobs on an offline runner become `lost`. A lost job
  is redelivered once to any eligible runner, from the same `source.ref` and
  continuation, in a fresh clone. Because the clone is new, a half-run job
  left nothing behind that can be double-applied; the only mutator with
  side effects outside the clone is the daemon's landing step, which is
  where `IDEMPOTENCY.md` Slice B's `effect_key` ledger activates. This plan is
  that document's "first redelivery source" and sequences its enforcement.
- **Agent provider failure** on a runner is reported with today's
  `AgentFailureMetadata` classification and takes the existing paused or
  escalated paths. The breaker becomes per provider *per runner*, since a
  Claude window limit on the Mac says nothing about Codex on ops-01.
- **No eligible runner** holds the item with a reason, and the ops digest
  carries it after one digest window. A campaign member whose work needs a
  platform nobody provides waits rather than burning cycles writing blockers.
- **Clone or continuation failure** refuses the job before any step runs, and
  the item is not counted as a spent cycle, which closes part of the backlog's
  "do not spend campaign cycles when no agent execution starts".
- **Daemon restart** with jobs running on remote runners: runners keep
  running and buffer logs; on reconnect they replay buffered lines and
  results. Jobs on the local runner die with the daemon, as today, and
  redeliver on start.

### 3.9 Security posture

Same trust boundary as today, stated:

- The daemon is the trust root. A runner executes whatever a registered
  daemon sends, in a clone, as the runner's user. Only the daemon address and
  a token in the host's secret store are configured on the runner.
- Runners dial out; no inbound port on the Mac. Traffic is plaintext gRPC on
  the LAN or inside the tunnel, as the control plane is. Adding TLS to the
  daemon later covers runners for free.
- Runners need read access to repositories. For private repositories that is
  a read-only deploy key or the host's existing `gh` login; never a push
  credential, because runners never push.
- Agent provider credentials sit on the runner host in its secret store and
  are never sent over the stream.
- A runner's labels are self-reported. A wrong label makes a job fail on the
  wrong platform, which the gate results show; it is not a privilege.

## 4. Phases

Each phase lands on `main` by ordinary commits, is iterated on the Mac
against the development daemon and the sandbox project, and is promoted by
the release procedure in `AGENTS.md`. Done evidence is what a reviewer checks.
Phases 0 and 1 are small and deliver the motivating case; 2 through 4 finish
the shift.

### Phase 0: contract and the runner binary

- `foundry-sdk/src/job.rs` and `runner.rs` with the types in section 3.3,
  `contracts/job-v1.json`, serde round-trip tests.
- `proto/foundry.proto`: `RunnerSession` and its messages.
- `crates/foundry-runner`: register, heartbeat, mirror, clone at ref, run
  `gates` steps through the existing `gate_runner`, stream logs, ship gate
  logs and a bundle, delete or preserve. No agent steps yet.
- `foundryd/src/runners/`: accept sessions, verify tokens, hold the live set,
  `RunJob` as a daemon-side async call. `foundry runner token|list|show|
  drain|remove`.
- Keychain and systemd-credential readers for the token, in `foundry-sdk`
  behind a small trait with a fake.
- A launchd plist and a systemd unit for `foundry-runner`.

Done: on the Mac, `foundry-runner serve --daemon http://127.0.0.1:50051`
registers with the development daemon; `foundry runner list` shows it with
`os=macos arch=arm64`; `foundry gates run sample-rs --on os=macos` (a new
operator command that submits a one-off gates job) runs the sandbox gates in a
fresh clone and prints the result with the daemon-local log paths. A runner
killed mid-gate shows `offline` within 45 s and the job `lost`.

### Phase 1: scheduling and the first production use

- `runs_on` on `ProjectEntry` and per gate in `.hone-gates.json`; `source_url`.
- `WorkItem.runs_on` and `runner`; pacing checks 6 and 7; queue and digest
  rendering of `no runner for …` and `runner <name> at capacity`.
- Daemon mirrors under `~/.foundry/mirrors/` for ref resolution and gates-file
  reads.
- `RunVerifyGates` builds a job for gates whose `runs_on` is not satisfied by
  the local runner and runs the rest locally in the worktree as today. This is
  the minimal change that gets Darwin gates onto the Mac while execution still
  happens on ops-01.
- Events `runner_registered`, `runner_departed`, `runner_job_*`.
- Per-runner provider breakers.

Done: `mojentic-sw` has a `test-darwin` gate with `runs_on: {os: macos}`; a
`mojentic-3-0-sw` cycle on ops-01 runs its Linux gates there and its Darwin
gate on the Mac; the gate result in the event carries `runner: max-headroom`
and a log path on ops-01 that holds the Mac's output. With the Mac runner
stopped, the item waits with `no runner for os=macos` and the digest says so.

### Phase 2: agent execution on runners

- Move the provider adapters (`gateway/engine.rs`, the claude and codex
  adapters, `agent_stream.rs`, `token_usage.rs`) from `foundry-blocks` into a
  crate both `foundryd` and `foundry-runner` depend on. Mechanical.
- `agent` steps in the runner: prompt and agent file as blobs, provider config
  from the runner host's `agents.json`, transcript shipped as an artifact.
- `ExecutePlan` and `ExecuteMaintain` build jobs with `agent` plus `gates`
  steps and submit them. The local runner takes them when `runs_on` is empty,
  so ops-01 behaviour is unchanged except that the work now happens in a clone
  under `~/.foundry/runner/jobs/` instead of a worktree.
- Landing from a bundle, with the rebase-and-verify job path.
- `IDEMPOTENCY.md` Slice B: `effect_key` enforcement on landing.
- Redelivery of lost jobs; daemon-restart reconnect and replay.

Done: a task dispatched to `mojentic-sw` with `runs_on: {os: macos}` runs its
agent on the Mac, its bundle lands on `main` from ops-01, and the chain's
events are the same types as before with `job_id` and `runner` added. Killing
the Mac runner during the agent step redelivers once and the landing ledger
shows one effect, not two.

### Phase 3: nightly, release, and the end of the dirty tree

- Nightly maintain runs as jobs. `checkout_sync`, `branch_guard`, `run_guard`
  and `push_guard` retire, because there is no shared tree to guard. Their
  invariants (no agent push, fast-forward only) are enforced by the runner's
  pushurl and the daemon's landing.
- Release: `ExecuteRelease` splits into the tag-and-push step (daemon, from
  the mirror) and build jobs per `release.targets` in the registry
  (`[{runs_on: {os: linux, arch: x86_64}}, {runs_on: {os: macos, arch:
  arm64}}]`), each returning the artifact as a job artifact. `InstallLocally`
  becomes an install job on the runner whose host should receive the binary,
  which is how `parite` gets its amd64 build from ops-01 without the Mac being
  involved.
- `workspace.mode = "registered_path"` is removed. `ProjectEntry.path` is
  documented as informational. The registry note for `mojentic-sw` about CI
  covering macOS is deleted.
- The initiative integration gate's `host` field is reimplemented as a job
  with `runs_on: {host: <name>}`, and `integration_runner.py` is deleted.
  The far host no longer needs a `foundryd`.

Done: a full nightly on ops-01 touches no registered checkout (verified by
`git status` and `reflog` on each before and after); `foundry release
epilogue-tracker` produces Linux and macOS artifacts from two runners and the
release event names both; `integration_runner.py` and its SSH path are gone.

### Phase 4: platform matrices

- A gates step may fan out: the daemon turns one `gates` step whose gates have
  differing `runs_on` into one job per platform and gathers with the existing
  scatter and gather machinery, so a single `GateVerificationCompleted` carries
  results from several runners.
- Toolchain labels are detected, not typed: the runner probes `swift`,
  `cargo`, `node`, `python3`, `elixir`, `java` and reports versions as
  `toolchain.<name>=<version>`, and a project may constrain
  `toolchain.swift>=6.4`.
- `foundry runner` shows utilisation; the ops digest reports idle platforms
  and waiting items per platform.

Done: `mojentic-sw`'s gates run on Linux and macOS in one cycle with one
verification event, and the digest shows the Mac runner's share of the week.

## 5. Shipping dark: what the runner model asks of it

Stacey's second consequence is a separate conversation, and this section only
states what the runner model requires of its outcome, so that conversation
starts from the right constraints.

Today a multi-cycle campaign can carry unfinished work between cycles as a
preserved bundle (`~/.foundry/preserved/`) or a `foundry-task/*` branch. That
is a branch by another name, and it is exactly what the trunk-based rule says
to avoid at the organisational level. With runners, the pressure increases:
every job starts from trunk, the fast-forward landing means trunk is the only
place work accumulates, and a cycle that lands part of a feature has landed it
for every consumer that pulls `main`, including production deploys for
`bedrock` and `ops-visualizer`, which deploy on push.

So the runner model wants this answer: **a cycle's output is always safe to
land on trunk, and features reach users through a toggle, not through
holding commits back.** The pieces that would have to exist:

- A convention per stack for a release toggle (a config flag read at start,
  an environment switch, a feature module not yet wired), named in the
  project's `AGENTS.md` so an executor knows how to ship dark.
- An acceptance boundary in campaigns that distinguishes "landed dark" from
  "released", so `done_evidence` can require the toggle to be on in
  production before the campaign completes.
- Toggle hygiene as maintenance: a nightly check that lists toggles older than
  N days and asks whether they are on everywhere and can be removed. Foundry's
  scout and maintain chains are the natural home.
- Dependency releases across the Mojentic ports (the version-synchronisation
  rule) are the hardest case, because a library cannot toggle its public API.
  There the answer is probably additive APIs with deprecation windows, which is
  a release-policy question, not a Foundry one.

What the runner model does *not* need is a long-lived integration branch per
feature. Preserved bundles stay as a recovery mechanism for a cycle that
could not land, not as a way to accumulate a feature. That matches the
Operations rule as written.

## 6. Risks

- **Mac as a runner is a shared machine.** Capacity 1, a sleep hook that
  drains, and a `foundry runner pause max-headroom` for when Stacey is
  presenting. A job that runs `swift test` while an interactive session also
  builds will be slow; the gate timeout scale per runner (today's
  `FOUNDRY_GATE_TIMEOUT_SCALE`, moved to a runner label) absorbs some of it.
  TCC on macOS will prompt for Full Disk Access once for `foundry-runner`;
  `install.sh`'s stable signing identifier trick applies.
- **Clone cost for large repositories** is bounded by the mirror, but the
  first job on a new runner pays a full clone. Acceptable; print it.
- **Bundle landing diverges from worktree landing** in subtle ways (hooks,
  LFS, submodules). None of our registered projects use LFS or submodules
  today; the job spec has room for `source.submodules` later.
- **Two crates sharing the agent adapters** is a refactor with a wide blast
  radius in `foundry-blocks`. It is phase 2, after phases 0 and 1 prove the
  protocol with gates alone, so it is never on the critical path to the
  motivating case.
- **Plaintext plus token is not authentication of the daemon.** A runner that
  dials a wrong address will hand its token to whoever answers. The address
  is set once in the service unit from a hostname we control on a LAN we
  control. TLS for the control plane is the real fix and is out of scope
  here; this plan does not make it worse.
- **Lost-job redelivery can double-spend provider budget.** A job lost after
  an hour of agent work redelivers from scratch. Once only, and the activity
  record shows both attempts under one work item, so the cost is visible.
- **Scope creep toward a CI product.** Every GitHub Actions feature (caches,
  services, containers, matrices beyond platform) will look reasonable. The
  charter test is whether Foundry's own chains need it. Phase 4 is the ceiling
  in this plan.

## 7. Decisions

For Stacey. The plan assumes the first option of each unless she says
otherwise.

1. **Result transport.** Bundles over the stream, landed by the daemon
   (keeps runners push-less and landing authority in one place), or runners
   pushing to a `foundry/job/<id>` ref on origin (simpler transport, more
   branches on GitHub, runner needs write credentials).
2. **Does the Mac run agents, or only gates and builds?** Phase 2 assumes yes,
   with the Mac's existing Claude and Codex logins. "Gates and builds only" is
   a smaller, cheaper plan that still clears the motivating case, and leaves
   Darwin-specific implementation work interactive, as the 2026-10-17 decision
   did.
3. **One execution path from phase 2**, with the daemon host's own work going
   through the local runner in a clone under `~/.foundry/runner/jobs/`, or keep
   the worktree path for the daemon host and use jobs only for remote
   platforms. One path is more change up front and far less to reason about
   after.
4. **Retire dirty-tree mode in phase 3** so the registered `path` is never
   written by Foundry again, or keep it as an opt-in per project. Retiring it
   is the point of the shift; keeping it preserves a failure class.
5. **Connection direction and token store** as in 3.4 and 3.9: runner dials
   the daemon, token in Keychain or systemd credentials. The alternative is
   the daemon reaching runners over SSH as the integration gate does today,
   which needs no new daemon-side listener but keeps a Python runner, needs a
   shell account per runner, and cannot stream.
6. **Where preserved clones live and how long.** `~/.foundry/runner/jobs/<id>`
   on the runner host, kept until `foundry runner clean` or 14 days, reported
   in the job result so an interactive session can `cd` to it. Or copy the
   preserved bundle back to the daemon and delete the clone, which is tidier
   and loses the ready-to-work tree.

## 8. References

- `SDK.md` "Phase 2 — gRPC block workers"; `IDEMPOTENCY.md` Slice B;
  `OBSERVABILITY.md` section 3.1 and phase 1; `BACKLOG.md` first and fifth
  entries; `CHARTER.md`.
- `book/src/guide/initiatives.md` integration gates with a `host`;
  `book/src/guide/trusted-lan-control-plane.md`;
  `book/src/guide/maintenance-workflow.md` on the registered checkout.
- `crates/foundry-blocks/src/blocks/{checkout_sync,task_workspace,push_guard,
  branch_guard,git_ops}.rs`, `gate_runner.rs`, `agent_stream.rs`,
  `steer_initiative/integration.rs` and `integration_runner.py`.
- `crates/foundry-sdk/src/{registry,pacing,work_item,gates}.rs`,
  `gateway/{shell,agent}.rs`.
- Operations `AGENTS.md`: trunk-based development, build-host repository
  mirroring, credentials never in plaintext, dependencies by released version.
- Production evidence: campaign `mojentic-sw-transient-recovery-v2`
  escalations 2026-10-10 to 2026-10-13; `mojentic-3-0-sw` Darwin blocker and
  the `initiative_decided` record of 2026-10-17 on ops-01.
