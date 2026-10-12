# Change Foundry while work continues

Leah maintains Foundry. She has changed how Foundry admits a queued continuation.
Other repositories are using the production daemon, so replacing its binary
also affects their running work.

Her goal is a working change and a production restart that does not interrupt
another project's agent session.

## Prove the change on the development host

Leah develops on the Mac. She installs the changed binaries and reloads the
development daemon through the repository's local procedure. Its registry
contains synthetic sandbox projects, including `sample-rs`.

She uses an explicit development address:

```bash
foundry --addr http://127.0.0.1:50051 validate sample-rs
foundry --addr http://127.0.0.1:50051 queue
```

She also runs the changed continuation path against sandbox work and checks its
result. Unit tests alone would not show whether the installed daemon admits
and settles that work correctly.

The repository gates pass. The sandbox records match the intended behaviour.
Leah records the source revision and evidence, then waits for the owner's
promotion approval.

## Make a quiet point after approval

After approval, Leah reads the production host's playbook and maintenance log.
She uses the manual release procedure in the repository's `AGENTS.md`. She
builds the tagged Linux artifact on the ops host and checks its release gates.

Outside the configured nightly window, she drains production:

```bash
ssh mojility-ops-01 foundry pacing drain --timeout 30m
```

Draining pauses every lane and waits for running work to settle. A timeout names
what still runs. Leah proceeds with replacement only after the drain reports
an idle daemon.

She installs the release artifact and restarts the daemon using the manual
procedure. The drain's pause survives the restart. She checks the installed
version and daemon state before allowing new work:

```bash
ssh mojility-ops-01 foundry --version
ssh mojility-ops-01 foundry status
ssh mojility-ops-01 foundry sentinel list
ssh mojility-ops-01 foundry pacing show
```

Once those checks pass, she resumes the lanes:

```bash
ssh mojility-ops-01 foundry pacing resume --lane all
ssh mojility-ops-01 foundry pacing show
ssh mojility-ops-01 foundry queue
```

## Leave a record of what is running

Leah checks release publication separately and records the installation and
verification in the host's maintenance log. The ending includes a known
installed version, a responsive daemon, restored pacing and durable evidence.

The commands here show the decision points. Use the repository's
[manual release procedure][manual-release] for the complete installation
instructions. Foundry itself is not released through
`foundry release foundry` or a task on its production daemon.

This follows Tracker journey `promote_foundry`. Read
[Getting started](../guide/getting-started.md) for the daemon setup and
[The work queue](../guide/work-queue.md) for drain semantics.

[manual-release]: https://github.com/svetzal/foundry/blob/main/AGENTS.md#ci--release
