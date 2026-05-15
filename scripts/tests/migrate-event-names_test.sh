#!/usr/bin/env bash
# Fixture test for scripts/migrate-event-names.sh.
#
# Verifies:
#   1. Old event names are rewritten to the v0.17.0 taxonomy.
#   2. Cycle vs. project split honours the `project` field.
#   3. Recomputed event ids match Event::compute_id (SHA-256 over
#      event_type || project || occurred_at_rfc3339 || compact_payload).
#   4. project_trace_ids payload references are rewritten to point at
#      the recomputed event ids.
#   5. Running the script twice is byte-identical (idempotent — md5).

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
MIGRATE_SCRIPT="$REPO_ROOT/scripts/migrate-event-names.sh"

TMP="$(mktemp -d -t foundry-migrate-test.XXXXXX)"
trap 'rm -rf "$TMP"' EXIT

EVENTS_DIR="$TMP/events"
TRACES_DIR="$TMP/traces/2026-05-15"
mkdir -p "$EVENTS_DIR" "$TRACES_DIR"

JSONL="$EVENTS_DIR/2026-05.jsonl"
TRACE="$TRACES_DIR/evt_aaaaaaaaaaaaaaaaaaaaaaaa.json"

# --- Fixture: 6 events (4 with renamed types + 2 with ref rewrites) ---
# Pre-compute the expected ids the script should produce by mirroring
# Event::compute_id exactly. We use python3 inline to build both the
# fixture file and the expected ids; the test then runs the script
# and asserts byte-for-byte equality.

python3 - "$JSONL" "$TRACE" <<'PY'
import hashlib
import json
import sys

jsonl_path, trace_path = sys.argv[1], sys.argv[2]


def canonical(ts: str) -> str:
    return ts[:-1] + "+00:00" if ts.endswith("Z") else ts


def compute_id(et: str, project: str, occurred_at: str, payload) -> str:
    h = hashlib.sha256()
    h.update(et.encode())
    h.update(project.encode())
    h.update(canonical(occurred_at).encode())
    h.update(json.dumps(payload, separators=(",", ":"), ensure_ascii=False).encode("utf-8"))
    return "evt_" + h.hexdigest()[:24]


# Source events use old (pre-0.17.0) names. The ids below are wrong on
# purpose (placeholders) — the script should overwrite them with the
# correctly-derived id.
events = [
    {
        # system + maintenance_run_started -> maintenance_cycle_started
        "id": "evt_oldsystemstart00000000",
        "event_type": "maintenance_run_started",
        "project": "system",
        "occurred_at": "2026-05-15T06:00:00.000000Z",
        "recorded_at": "2026-05-15T06:00:00.000000Z",
        "throttle": "full",
        "payload": {},
    },
    {
        # foundry + maintenance_run_started -> project_run_started
        "id": "evt_oldfoundrystart000000",
        "event_type": "maintenance_run_started",
        "project": "foundry",
        "occurred_at": "2026-05-15T06:00:01.000000Z",
        "recorded_at": "2026-05-15T06:00:01.000000Z",
        "throttle": "full",
        "payload": {},
    },
    {
        # iteration_requested -> project_iteration_requested
        "id": "evt_olditer00000000000000",
        "event_type": "iteration_requested",
        "project": "foundry",
        "occurred_at": "2026-05-15T06:00:02.000000Z",
        "recorded_at": "2026-05-15T06:00:02.000000Z",
        "throttle": "full",
        "payload": {"reason": "scheduled"},
    },
    {
        # greet_requested -> greeting_requested
        "id": "evt_oldgreet0000000000000",
        "event_type": "greet_requested",
        "project": "hello-world",
        "occurred_at": "2026-05-15T06:00:03.000000Z",
        "recorded_at": "2026-05-15T06:00:03.000000Z",
        "throttle": "full",
        "payload": {},
    },
    {
        # maintenance_requested -> project_maintenance_requested
        "id": "evt_oldmaint0000000000000",
        "event_type": "maintenance_requested",
        "project": "foundry",
        "occurred_at": "2026-05-15T06:00:04.000000Z",
        "recorded_at": "2026-05-15T06:00:04.000000Z",
        "throttle": "full",
        "payload": {},
    },
    {
        # system + maintenance_run_completed -> maintenance_cycle_completed
        # This one's payload references the project-level run start id
        # ("evt_oldfoundrystart000000") in project_trace_ids — the script
        # must rewrite that ref to the recomputed id.
        "id": "evt_oldsystemend0000000000",
        "event_type": "maintenance_run_completed",
        "project": "system",
        "occurred_at": "2026-05-15T06:30:00.000000Z",
        "recorded_at": "2026-05-15T06:30:00.000000Z",
        "throttle": "full",
        "payload": {
            "project_trace_ids": {
                "foundry": "evt_oldfoundrystart000000",
            },
            "root_event_id": "evt_oldsystemstart00000000",
            "success": True,
        },
    },
]

# Expected event_type after rename:
expected_renames = [
    "maintenance_cycle_started",
    "project_run_started",
    "project_iteration_requested",
    "greeting_requested",
    "project_maintenance_requested",
    "maintenance_cycle_completed",
]

# Compute expected NEW ids for events 0..4 (their payloads don't get
# rewritten — id is determined by new event_type + same payload).
expected_new_ids = []
for evt, new_et in list(zip(events, expected_renames))[:5]:
    expected_new_ids.append(
        compute_id(new_et, evt["project"], evt["occurred_at"], evt["payload"])
    )

# Build id_map for ref rewrites — keys are the PLACEHOLDER ids in the
# fixture (the migration sees them in payload refs and replaces with
# the recomputed event ids).
id_map = {
    "evt_oldsystemstart00000000": expected_new_ids[0],
    "evt_oldfoundrystart000000": expected_new_ids[1],
}

# For the 6th event (with refs), the payload mutates so id depends on
# the rewritten payload.
evt5 = events[5]
rewritten_payload = json.loads(json.dumps(evt5["payload"]))
rewritten_payload["project_trace_ids"]["foundry"] = id_map["evt_oldfoundrystart000000"]
rewritten_payload["root_event_id"] = id_map["evt_oldsystemstart00000000"]
expected_evt5_id = compute_id(
    "maintenance_cycle_completed",
    "system",
    evt5["occurred_at"],
    rewritten_payload,
)
expected_new_ids.append(expected_evt5_id)

with open(jsonl_path, "w", encoding="utf-8") as fh:
    for evt in events:
        fh.write(json.dumps(evt, separators=(",", ":"), ensure_ascii=False))
        fh.write("\n")

# Build a parallel trace fixture covering the same events.
trace_doc = {
    "events": events,
    "block_executions": [],
    "total_duration_ms": 1800000,
}
with open(trace_path, "w", encoding="utf-8") as fh:
    json.dump(trace_doc, fh, indent=2, ensure_ascii=False)
    fh.write("\n")

# Persist the expected outputs so the bash test can read them.
import os
with open(os.path.join(os.path.dirname(jsonl_path), "expected_renames.txt"), "w") as fh:
    fh.write("\n".join(expected_renames) + "\n")
with open(os.path.join(os.path.dirname(jsonl_path), "expected_ids.txt"), "w") as fh:
    fh.write("\n".join(expected_new_ids) + "\n")
PY

# --- Run the script ---
export FOUNDRY_EVENTS_DIR="$EVENTS_DIR"
export FOUNDRY_TRACES_DIR="$TMP/traces"

echo "[test] first migration pass" >&2
"$MIGRATE_SCRIPT" >/dev/null

# --- Assertion 1: no old names remain in either file ---
OLD_NAMES='"event_type":"maintenance_run_started"\|"event_type":"maintenance_run_completed"\|"event_type":"iteration_requested"\|"event_type":"maintenance_requested"\|"event_type":"greet_requested"'

OLD_NAMES_PRETTY='"event_type": "maintenance_run_started"\|"event_type": "maintenance_run_completed"\|"event_type": "iteration_requested"\|"event_type": "maintenance_requested"\|"event_type": "greet_requested"'

if grep -E "$OLD_NAMES" "$JSONL" >/dev/null 2>&1; then
  echo "FAIL: old event names still present in $JSONL" >&2
  grep -E "$OLD_NAMES" "$JSONL" >&2
  exit 1
fi

if grep -E "$OLD_NAMES_PRETTY" "$TRACE" >/dev/null 2>&1; then
  echo "FAIL: old event names still present in $TRACE" >&2
  grep -E "$OLD_NAMES_PRETTY" "$TRACE" >&2
  exit 1
fi

# --- Assertion 2: each expected new event type is present (jsonl) ---
while IFS= read -r expected_type; do
  [ -z "$expected_type" ] && continue
  if ! grep -F "\"event_type\":\"$expected_type\"" "$JSONL" >/dev/null; then
    echo "FAIL: expected event_type $expected_type not found in $JSONL" >&2
    exit 1
  fi
done < "$EVENTS_DIR/expected_renames.txt"

# --- Assertion 3: each expected new id is present (jsonl) ---
while IFS= read -r expected_id; do
  [ -z "$expected_id" ] && continue
  if ! grep -F "\"id\":\"$expected_id\"" "$JSONL" >/dev/null; then
    echo "FAIL: expected id $expected_id not found in $JSONL (id recomputation mismatch)" >&2
    exit 1
  fi
done < "$EVENTS_DIR/expected_ids.txt"

# --- Assertion 4: payload refs in 6th event were rewritten ---
# The 6th event's payload.project_trace_ids.foundry must now equal the
# recomputed project-level start id (line 2 of expected_ids.txt).
EXPECTED_PROJECT_RUN_ID="$(sed -n '2p' "$EVENTS_DIR/expected_ids.txt")"
EXPECTED_SYSTEM_START_ID="$(sed -n '1p' "$EVENTS_DIR/expected_ids.txt")"

if ! grep -F "\"foundry\":\"$EXPECTED_PROJECT_RUN_ID\"" "$JSONL" >/dev/null; then
  echo "FAIL: project_trace_ids.foundry ref was not rewritten to $EXPECTED_PROJECT_RUN_ID" >&2
  exit 1
fi
if ! grep -F "\"root_event_id\":\"$EXPECTED_SYSTEM_START_ID\"" "$JSONL" >/dev/null; then
  echo "FAIL: root_event_id ref was not rewritten to $EXPECTED_SYSTEM_START_ID" >&2
  exit 1
fi

# --- Assertion 5: idempotency — second run must not change either file ---
JSONL_MD5_BEFORE="$(md5 -q "$JSONL")"
TRACE_MD5_BEFORE="$(md5 -q "$TRACE")"

echo "[test] second migration pass (must be a no-op)" >&2
"$MIGRATE_SCRIPT" >/dev/null

JSONL_MD5_AFTER="$(md5 -q "$JSONL")"
TRACE_MD5_AFTER="$(md5 -q "$TRACE")"

if [ "$JSONL_MD5_BEFORE" != "$JSONL_MD5_AFTER" ]; then
  echo "FAIL: $JSONL changed between runs (not idempotent)" >&2
  echo "  before: $JSONL_MD5_BEFORE" >&2
  echo "  after : $JSONL_MD5_AFTER" >&2
  exit 1
fi
if [ "$TRACE_MD5_BEFORE" != "$TRACE_MD5_AFTER" ]; then
  echo "FAIL: $TRACE changed between runs (not idempotent)" >&2
  echo "  before: $TRACE_MD5_BEFORE" >&2
  echo "  after : $TRACE_MD5_AFTER" >&2
  exit 1
fi

# --- Assertion 6: --dry-run must not modify files ---
echo "[test] dry-run pass (must not touch files)" >&2
"$MIGRATE_SCRIPT" --dry-run >/dev/null
if [ "$(md5 -q "$JSONL")" != "$JSONL_MD5_AFTER" ]; then
  echo "FAIL: --dry-run modified $JSONL" >&2
  exit 1
fi
if [ "$(md5 -q "$TRACE")" != "$TRACE_MD5_AFTER" ]; then
  echo "FAIL: --dry-run modified $TRACE" >&2
  exit 1
fi

echo "PASS"
exit 0
