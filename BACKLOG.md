# Foundry improvement backlog

This backlog records observed Foundry behavior that should be improved but is
not being addressed in the current workflow. Entries require concrete runtime
evidence and a verifiable completion boundary. Remove an entry when it lands;
Git history is the archive.

## P1 — Show that an agent session is alive, and stop a restart from killing one unseen

**Observed:** 2026-09-30

A task on project parite ran one agent session for 57 minutes. Between
`agent_session_started` (13:06:34 UTC) and the end of the block, Foundry
emitted nothing, so `foundry status`, `foundry queue` and ops-visualizer all
showed the same line for the whole time and the owner could not tell a working
agent from a hung one. The same day the daemon was restarted twice while a
workflow was running (02:08 UTC by one session, 12:00:58 UTC by another); each
restart killed the running agent, and nothing warned the operator or the other
session.

Evidence: ops-01 event log for trace of `evt_f2e5d7d67bc09b7874b835d9`
(no events between 13:06:34 and 14:03); `systemctl --user show foundryd -p
ActiveEnterTimestamp` on 2026-09-30; the interrupted bedrock task
`evt_1c073a99f3ee4accdbe27d54`.

Completion evidence:

- While an agent session runs, Foundry records a heartbeat at a fixed interval
  (last output time and bytes written so far) that `foundry queue`,
  `foundry status` and the Watch stream expose, without flooding the durable
  event log.
- A work item's `queue show` names the provider, tier and session id of the
  session working it.
- `foundryd` refuses a graceful stop while items are running unless told to
  force, and a CLI command reports who is using the daemon (running items and
  their origins) before a restart.
- Tests cover the heartbeat cadence, the refusal, and the forced path settling
  running items as `failed` with the restart reason.

## P2 — The event writer can leave two events on one line

**Observed:** 2026-09-30 (written 2026-09-25)

Line 6245 of `~/.foundry/events/2026-09.jsonl` on mojility-ops-01 holds a
truncated `agent_session_ended` event (`evt_d42af67b66b914485fb96cc8`, cut off
inside its timestamp at `2026-09-25T15:44:40.780555034+0`) followed on the
same line by a complete `execution_requested` event
(`evt_e3d1502d12e698e13325e8c7`). Both are unreadable to a line-based JSON
reader. The daemon was killed mid-write on 2026-09-25 and the next process
appended without first terminating the partial line. ops-visualizer logs a
parse warning for it on every re-read.

Completion evidence:

- On open, the event writer checks whether the file ends with a newline and,
  if not, terminates the partial line before appending, so a later event is
  never glued to a fragment.
- A partial trailing line is reported once at start with its byte offset.
- A test writes a truncated last line, reopens the writer, appends an event,
  and asserts the new event parses on its own line.

## P1 — A formation agent that fails to spawn should pause the campaign, not escalate it

**Observed:** 2026-09-30

Campaign `foundry-work-ledger-v1` on mojility-ops-01 had five landed cycles
and a full budget left. Its sixth formation failed three times in fifteen
seconds with `failed to spawn claude` and the campaign was escalated with
reason "campaign decision agent did not answer in 3 attempts". A `resume`
and a second advance failed the same way. The actual cause was
`E2BIG`: the formation prompt is passed as one argv element and had grown
past Linux's 131,072-byte per-argument limit (the five successful prompts
were 111 KB to 122 KB; the two failures 134 KB and 133 KB). That fix is
dispatched separately. Two behaviours remain wrong:

- The `std::io::Error` from `Command::spawn` is not recorded. The event
  says `failed to spawn claude` and nothing else, so an argument-length
  error, a missing binary and a resource limit all look like a provider
  outage.
- A formation agent that never starts escalates the campaign, which needs an
  owner `resume`. The lifecycle documents provider unavailability as a
  `paused` state for task runs; the formation should take the same path.

Evidence: `campaign_advance_completed` and `campaign_escalated` at
2026-09-30T06:16:37Z and 07:56:22Z on ops-01, `agent_session_ended`
`unavailable` records at 06:16:22, 06:16:27, 06:16:37, 07:56:1x and
07:56:22, and the `prompt` field of each `campaign_advance_completed`.

Completion evidence:

- `agent_session_ended` for a spawn failure carries the OS error text (for
  example `Argument list too long (os error 7)`), and the escalation or
  pause reason repeats it.
- A formation decision agent that cannot be spawned, or returns no output,
  moves the campaign to `paused` with a typed provider-unavailable reason and
  a retry-after, does not consume a cycle, and is retried by the daemon
  without an owner `resume`. The ops digest carries the pause only if it
  persists past one retry window.
- Retries are spaced by a backoff, not fifteen seconds.
- Tests cover spawn failure with error text, empty output, and a successful
  retry.

## P0 — Classify temporary provider limits without opening a lifetime breaker

**Observed:** 2026-07-20

Claude's five-hour usage-window limit was reported and persisted as a monthly
spend limit. Foundry classified it as a terminal provider failure and opened an
in-memory circuit breaker for the lifetime of `foundryd`. The upstream window
later cleared, but retries were rejected from cached breaker state until the
daemon was restarted.

Evidence: Parite campaign escalation events `evt_085ce385f41439ccad262d7b`,
`evt_768c3c1fb4c44174983ee753`, and `evt_90e0d015e4025e9cd4724d5f`.

Completion evidence:

- Provider failure metadata distinguishes temporary usage-window exhaustion
  from terminal account/authentication failures without claiming a monthly
  limit when that cannot be established.
- Temporary limits open an expiring breaker or carry a retry-after boundary;
  they do not poison the provider for the daemon's lifetime.
- Deterministic tests cover window expiry, a successful post-expiry probe, and
  genuinely terminal authentication/account failures.

## P0 — Do not spend campaign cycles when no agent execution starts

**Observed:** 2026-07-20

The Parite probe-observability campaign consumed its eighth and final cycle when
Claude failed at invocation after roughly two seconds. No implementation work
or gate evaluation occurred, but the campaign advanced to `8/8`, leaving both a
provider escalation and an exhausted campaign budget.

Evidence: task event `evt_175de8fc578b50f8a6300d92` and campaign escalation
`evt_90e0d015e4025e9cd4724d5f`.

Completion evidence:

- Pre-execution provider, authentication, transport, and runner-start failures
  do not increment `cycles_completed` or reduce the authorized work budget.
- A retry after provider recovery resumes the same objective and preserved base
  without requiring an unrelated cycle extension.
- Tests distinguish a started-but-defective implementation cycle from a runner
  that never began executing the objective.

## P1 — Provide typed provider-breaker visibility and reset controls

**Observed:** 2026-07-20

The only way to clear Foundry's process-local provider breaker was to discover
its implementation detail and restart `foundryd`. Campaign and status commands
did not show breaker age, failure class, retry eligibility, or the recovery
action.

Completion evidence:

- A typed status surface reports each provider breaker's class, opened time,
  expiry or permanence, last failure, and whether a probe/reset is allowed.
- An explicit provider-scoped reset or probe operation replaces daemon restart
  as the normal recovery path and emits an auditable event.
- Resetting one provider cannot clear another provider's state or interrupt
  unrelated active workflows.

## P1 — Show the latest escalation reason and remaining evidence in campaign status

**Observed:** 2026-07-20

`foundry campaign show` reported `Status: escalated` but omitted the escalation
reason and the last incomplete review evidence. Diagnosis required querying raw
JSONL events and correlating task review, runner failure, and campaign terminal
records manually.

Completion evidence:

- Campaign detail exposes the latest typed escalation reason, originating event,
  last objective/verdict, preserved work reference, and unmet done evidence.
- CLI and typed gRPC output distinguish provider failure, owner-decision need,
  exhausted budget, and implementation remainder without parsing prose.
- Tests cover escalation after provider failure, skeptical-review remainder,
  and budget exhaustion.

## P1 — Distinguish campaign authorization from live agent activity

**Observed:** 2026-07-19 through 2026-07-20

An `active` campaign and a `campaign_advance_requested ... running` status did
not reliably answer whether an agent was executing, gates were running, review
was pending, or the campaign was merely authorized for another advance. This
required inspecting session output and raw event timestamps during the Parite
movie, TV, and probe-observability campaigns.

Completion evidence:

- Campaign status exposes a typed current stage such as forming, agent-running,
  gates-running, review-running, idle-authorized, completed, or escalated.
- It includes the current run/event identity and last heartbeat or transition
  time, allowing stale activity to be recognized without process inspection.
- CLI and gRPC tests prove accurate transitions across successful, preserved,
  provider-failed, and automatically advanced cycles.
