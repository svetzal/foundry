# Foundry improvement backlog

This backlog records observed Foundry behavior that should be improved but is
not being addressed in the current workflow. Entries require concrete runtime
evidence and a verifiable completion boundary. Remove an entry when it lands;
Git history is the archive.

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
