# Finish a change that stopped

Jon helps Maya maintain `parcel-cli`. A customer supplied a file with an empty
record between two valid records. The importer stopped at the empty record and
silently skipped everything after it.

Jon can name the expected behaviour and its acceptance cases. He submits one
task with an explicit provider:

```bash
foundry task parcel-cli \
  "Skip empty import records; prove records after them are still imported" \
  --agent codex --origin "Customer supplied an import with an empty record"
```

## Read the result before starting again

Foundry runs the task in an isolated worktree. The tests pass, but the separate
review finds that whitespace-only records still stop the import. In this
example, the verdict is `defect`. Foundry commits and preserves the work rather
than landing it on trunk.

Jon finds the obligation and reads its evidence:

```bash
foundry queue open
foundry queue show wi_0123456789abcdef01234567
```

He reads the task result and reviewer diagnosis through the trace and evidence
references. The preserved implementation may be useful. He does not need to
start from a blank checkout.

## Continue the preserved implementation

The original objective still describes the desired behaviour. Jon resumes its
work item:

```bash
foundry queue resume wi_0123456789abcdef01234567 \
  --origin "Continue the preserved import fix"
```

Foundry records a new linked task. The new task keeps the original objective
and starts from the preserved work. It waits for the normal pacing rules.
The original item stays open until the child actually lands.

If trunk changed, Foundry first brings the continuation onto current trunk.
If that rebase conflicts, it refuses the continuation before starting an agent.
The preserved reference remains intact. Jon reads that failure before choosing
how to reapply the work.

The `--origin` note explains why Jon resumed it. It does not replace the task's
objective. If he needs a different objective, he uses the
[continuation options](../guide/work-queue.md#resume-preserved-work) rather than
assuming a resume rewrites the brief.

## Check the ending

In this run, the continuation adds the whitespace case, passes the required
gates and receives a complete review. It lands on trunk.

Jon reads the child and parent records. The child has the landing commit and a
`resumes` link to the parent. The parent now records that the continuation
landed. He also runs the customer's representative import through the updated
tool before telling Maya the importer now handles those records.

The useful ending is a working import and a settled obligation. A preserved
branch by itself would leave both unresolved.

This follows Tracker journey `recover_unlanded_work`. Read
[Tasks and campaigns](../guide/campaigns.md) for verdicts and landing rules.
