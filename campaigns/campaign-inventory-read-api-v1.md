# Campaign Inventory Read API V1

## Intent

This is the Foundry half of the second campaign-management test bed. The first
test bed proved that ops-visualizer can recognize campaign lifecycle events,
but an event window cannot answer the durable operational question: which
campaigns exist now, and what state is each one in?

Foundry owns campaign storage and must remain the only reader and writer of
`~/.foundry/campaigns.json`. This slice exposes a typed, read-only daemon query
that future clients can use without coupling themselves to the store file or
its serialization format.

## Completion boundary

V1 is complete when foundryd exposes one typed `ListCampaigns` gRPC query that:

- loads the configured campaign store at request time;
- returns every campaign in deterministic name order;
- supports an optional exact project-name filter;
- reports the campaign name, project, mission, status, completed and landed
  cycle counts, maximum cycle budget, authorization identity, agent provider,
  and last run event identifier;
- returns an empty list for a missing or empty store;
- maps malformed or unreadable store failures to a clear gRPC error;
- has focused tests at the service boundary using temporary campaign stores.

The public proto and Foundry guide must document the read-only query. The
workspace's required quality gates must remain green.

## Scope guards

- Do not add campaign mutation RPCs.
- Do not expose filesystem paths, done-evidence internals, or escalation rules
  on the wire in this slice.
- Do not cache campaign records in the daemon; request-time loading keeps the
  file-backed source authoritative across CLI and formation writes.
- Do not change campaign lifecycle behavior, event payloads, or cycle
  accounting.
- Do not modify ops-visualizer in this campaign. Its consumer campaign begins
  only after this API is released into the live daemon.

If a safe read-only query requires changing campaign ownership or inventing a
second source of truth, escalate instead of broadening the slice.

## Growth path

1. Expose this durable read-only inventory from Foundry.
2. Consume it in ops-visualizer and reconcile it with recent lifecycle events.
3. Add mediated pause, resume, and advance operations only after the read path
   is stable in daily use.
