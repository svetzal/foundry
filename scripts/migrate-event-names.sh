#!/usr/bin/env bash
# Migrate historical event and trace files to the v0.17.0 event-type taxonomy.
#
# Rewrites pre-0.17.0 event names in:
#   * $FOUNDRY_EVENTS_DIR/*.jsonl      (default ~/.foundry/events)
#   * $FOUNDRY_TRACES_DIR/YYYY-MM-DD/*.json
#                                      (default ~/.foundry/traces)
#
# For every renamed event, Event::id is recomputed (SHA-256 over
# event_type || project || occurred_at_rfc3339 || compact_json_payload —
# first 12 bytes hex-encoded, "evt_" prefix) so the deterministic id
# continues to round-trip. Old-id → new-id pairs are tracked across the
# file so payload references (project_trace_ids values, root_event_id)
# get rewritten in a second pass.
#
# Files are written atomically via *.new + rename, so a kill mid-run
# leaves the original intact. Re-running the script is a no-op
# (idempotent — verified by md5 in the bundled test).
#
# Usage:
#   scripts/migrate-event-names.sh             # rewrite in place
#   scripts/migrate-event-names.sh --dry-run   # report only, no writes
#
# Rename rules (v0.17.0 cycle/project split):
#   maintenance_run_started   + project == system  -> maintenance_cycle_started
#   maintenance_run_started   + project != system  -> project_run_started
#   maintenance_run_completed + project == system  -> maintenance_cycle_completed
#   maintenance_run_completed + project != system  -> project_run_completed
#   iteration_requested                            -> project_iteration_requested
#   maintenance_requested                          -> project_maintenance_requested
#   greet_requested                                -> greeting_requested

set -euo pipefail

DRY_RUN="false"
if [ "${1-}" = "--dry-run" ]; then
  DRY_RUN="true"
fi

EVENTS_DIR="${FOUNDRY_EVENTS_DIR:-$HOME/.foundry/events}"
TRACES_DIR="${FOUNDRY_TRACES_DIR:-$HOME/.foundry/traces}"

if [ "$DRY_RUN" = "true" ]; then
  echo "[dry-run] no files will be modified" >&2
fi

echo "events dir: $EVENTS_DIR" >&2
echo "traces dir: $TRACES_DIR" >&2

process_file() {
  local file="$1"
  local mode="$2" # "jsonl" or "trace"
  python3 - "$file" "$DRY_RUN" "$mode" <<'PY'
import hashlib
import json
import os
import sys

path, dry_run_flag, mode = sys.argv[1], sys.argv[2], sys.argv[3]
dry_run = dry_run_flag == "true"


def canonical_rfc3339(ts: str) -> str:
    """Convert a stored RFC3339 timestamp to chrono's to_rfc3339() form.

    chrono `to_rfc3339()` emits a numeric offset (+00:00) rather than the
    'Z' military designator that serde defaults to when serializing a
    DateTime<Utc>. The hash input must match chrono's form, so we
    normalise here.
    """
    if ts.endswith("Z"):
        return ts[:-1] + "+00:00"
    return ts


def compute_id(event_type: str, project: str, occurred_at: str, payload) -> str:
    h = hashlib.sha256()
    h.update(event_type.encode())
    h.update(project.encode())
    h.update(canonical_rfc3339(occurred_at).encode())
    # serde_json::Value::to_string() emits compact JSON with no whitespace
    # and writes non-ASCII as UTF-8 (not \u escapes). Python's json.dumps
    # with ensure_ascii=False + tight separators mirrors that.
    payload_str = json.dumps(payload, separators=(",", ":"), ensure_ascii=False)
    h.update(payload_str.encode("utf-8"))
    return "evt_" + h.hexdigest()[:24]


def rename(event_type: str, project: str):
    if event_type == "maintenance_run_started":
        return "maintenance_cycle_started" if project == "system" else "project_run_started"
    if event_type == "maintenance_run_completed":
        return "maintenance_cycle_completed" if project == "system" else "project_run_completed"
    if event_type == "iteration_requested":
        return "project_iteration_requested"
    if event_type == "maintenance_requested":
        return "project_maintenance_requested"
    if event_type == "greet_requested":
        return "greeting_requested"
    return None


def rewrite_event(evt: dict, id_map: dict) -> bool:
    """Apply rename rule + id recomputation. Returns True if changed."""
    et = evt.get("event_type")
    project = evt.get("project", "")
    new_et = rename(et, project)
    if new_et is None:
        return False
    old_id = evt.get("id")
    evt["event_type"] = new_et
    new_id = compute_id(new_et, project, evt.get("occurred_at", ""), evt.get("payload", {}))
    evt["id"] = new_id
    if old_id and old_id != new_id:
        id_map[old_id] = new_id
    return True


def rewrite_references(evt: dict, id_map: dict) -> bool:
    """Rewrite payload id refs to point at recomputed ids.

    Known id-bearing payload fields:
      - payload.project_trace_ids  (map: project -> event-id)
      - payload.root_event_id      (single event-id string)
    """
    changed = False
    payload = evt.get("payload")
    if not isinstance(payload, dict):
        return False
    ptids = payload.get("project_trace_ids")
    if isinstance(ptids, dict):
        for k, v in list(ptids.items()):
            if isinstance(v, str) and v in id_map:
                ptids[k] = id_map[v]
                changed = True
    rev = payload.get("root_event_id")
    if isinstance(rev, str) and rev in id_map:
        payload["root_event_id"] = id_map[rev]
        changed = True
    return changed


def recompute_id_after_payload_change(evt: dict, id_map: dict) -> bool:
    """Recompute id (payload changed). Returns True if id changed."""
    old_id = evt.get("id")
    new_id = compute_id(
        evt.get("event_type", ""),
        evt.get("project", ""),
        evt.get("occurred_at", ""),
        evt.get("payload", {}),
    )
    if new_id != old_id:
        evt["id"] = new_id
        if old_id:
            id_map[old_id] = new_id
        return True
    return False


def run_passes(events_iter, id_map, rename_samples, ref_samples):
    """Run rename pass + ref-rewrite passes until fixed point.

    Yields (renamed_count, ref_rewrite_count).
    """
    renamed_count = 0
    ref_rewrite_count = 0

    # Pass 1: rename event_type and recompute id based on the (still old)
    # payload. Builds initial id_map.
    for evt in events_iter:
        if not isinstance(evt, dict):
            continue
        old_et = evt.get("event_type")
        old_id = evt.get("id")
        if rewrite_event(evt, id_map):
            renamed_count += 1
            if len(rename_samples) < 5:
                rename_samples.append(
                    f"{old_et} -> {evt['event_type']} (id {old_id} -> {evt['id']})"
                )

    # Subsequent passes: rewrite payload refs to point at the recomputed
    # upstream ids, then recompute the host event's id (payload changed!).
    # Iterate until no further changes — converges quickly because the
    # ref-graph is a DAG bounded by the number of renamed events.
    for _ in range(8):  # safety bound
        round_changed = 0
        for evt in events_iter:
            if not isinstance(evt, dict):
                continue
            if rewrite_references(evt, id_map):
                ref_rewrite_count += 1
                round_changed += 1
                if len(ref_samples) < 5:
                    ref_samples.append(
                        f"event {evt.get('id')} payload refs rewritten"
                    )
                recompute_id_after_payload_change(evt, id_map)
        if round_changed == 0:
            break

    return renamed_count, ref_rewrite_count


def emit_sample(samples, label, file):
    if samples:
        print(f"  [{file}] {label} sample (first 3):", file=sys.stderr)
        for s in samples[:3]:
            print(f"    {s}", file=sys.stderr)


renamed_count = 0
ref_rewrite_count = 0
id_map: dict[str, str] = {}
rename_samples = []
ref_samples = []

if mode == "jsonl":
    # Two-pass over a list of decoded events (small enough to hold in RAM —
    # foundry's event logs are bounded by month).
    events = []
    with open(path, "r", encoding="utf-8") as fh:
        for line_no, line in enumerate(fh, start=1):
            line = line.rstrip("\n")
            if not line.strip():
                events.append(None)
                continue
            try:
                events.append(json.loads(line))
            except json.JSONDecodeError as exc:
                print(f"  [{path}] line {line_no}: invalid JSON, leaving file untouched: {exc}", file=sys.stderr)
                sys.exit(0)

    non_null = [e for e in events if e is not None]
    renamed_count, ref_rewrite_count = run_passes(
        non_null, id_map, rename_samples, ref_samples
    )

    if renamed_count == 0 and ref_rewrite_count == 0:
        # nothing to do — leave file as-is
        sys.exit(0)

    print(f"  [{path}] renamed {renamed_count} events, rewrote refs in {ref_rewrite_count} events", file=sys.stderr)
    emit_sample(rename_samples, "rename", path)
    emit_sample(ref_samples, "ref-rewrite", path)

    if dry_run:
        sys.exit(0)

    tmp = path + ".new"
    with open(tmp, "w", encoding="utf-8") as fh:
        for evt in events:
            if evt is None:
                fh.write("\n")
            else:
                # serde_json's default to_string() emits compact JSON with
                # no spaces; the event log was written compactly too.
                fh.write(json.dumps(evt, separators=(",", ":"), ensure_ascii=False))
                fh.write("\n")
    os.replace(tmp, path)
elif mode == "trace":
    with open(path, "r", encoding="utf-8") as fh:
        try:
            doc = json.load(fh)
        except json.JSONDecodeError as exc:
            print(f"  [{path}] invalid JSON, skipping: {exc}", file=sys.stderr)
            sys.exit(0)

    events = doc.get("events", [])
    if not isinstance(events, list):
        sys.exit(0)

    renamed_count, ref_rewrite_count = run_passes(
        events, id_map, rename_samples, ref_samples
    )

    if renamed_count == 0 and ref_rewrite_count == 0:
        sys.exit(0)

    print(f"  [{path}] renamed {renamed_count} events, rewrote refs in {ref_rewrite_count} events", file=sys.stderr)
    emit_sample(rename_samples, "rename", path)
    emit_sample(ref_samples, "ref-rewrite", path)

    if dry_run:
        sys.exit(0)

    tmp = path + ".new"
    with open(tmp, "w", encoding="utf-8") as fh:
        # serde_json's pretty default is 2-space indent, but the trace
        # store writes via to_string_pretty which uses 2-space. We don't
        # need to perfectly match formatting — readers parse JSON. Keep
        # it pretty for human-readable trace files.
        json.dump(doc, fh, indent=2, ensure_ascii=False)
        fh.write("\n")
    os.replace(tmp, path)
else:
    print(f"unknown mode {mode}", file=sys.stderr)
    sys.exit(2)
PY
}

processed_events=0
if [ -d "$EVENTS_DIR" ]; then
  while IFS= read -r -d '' file; do
    process_file "$file" "jsonl"
    processed_events=$((processed_events + 1))
  done < <(find "$EVENTS_DIR" -type f -name '*.jsonl' -print0)
fi

processed_traces=0
if [ -d "$TRACES_DIR" ]; then
  while IFS= read -r -d '' file; do
    process_file "$file" "trace"
    processed_traces=$((processed_traces + 1))
  done < <(find "$TRACES_DIR" -type f -name '*.json' -print0)
fi

echo "scanned $processed_events event log(s) and $processed_traces trace file(s)" >&2
if [ "$DRY_RUN" = "true" ]; then
  echo "[dry-run] complete — no files were modified" >&2
else
  echo "migration complete" >&2
fi
