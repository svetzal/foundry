#!/usr/bin/env bash
# Migrate historical trace and event files from pre-v0.8.0 event names.
# Safe to run multiple times — already-migrated files are unchanged.

set -euo pipefail

TRACES_DIR="${FOUNDRY_TRACES_DIR:-$HOME/.foundry/traces}"
EVENTS_DIR="${FOUNDRY_EVENTS_DIR:-$HOME/.foundry/events}"

OLD_NAMES="auto_release_triggered|auto_release_completed|gates_resolved|project_iterate_completed|project_maintain_completed"

count=0

migrate_file() {
  local file="$1"
  if grep -qE "$OLD_NAMES" "$file" 2>/dev/null; then
    sed -i '' \
      -e 's/auto_release_triggered/release_requested/g' \
      -e 's/auto_release_completed/release_completed/g' \
      -e 's/gates_resolved/gate_resolution_completed/g' \
      -e 's/project_iterate_completed/project_iteration_completed/g' \
      -e 's/project_maintain_completed/project_maintenance_completed/g' \
      "$file"
    echo "  $file"
    count=$((count + 1))
  fi
}

echo "Migrating event names to v0.8.0 conventions..."

if [ -d "$TRACES_DIR" ]; then
  while IFS= read -r -d '' file; do
    migrate_file "$file"
  done < <(find "$TRACES_DIR" -name '*.json' -print0)
fi

if [ -d "$EVENTS_DIR" ]; then
  while IFS= read -r -d '' file; do
    migrate_file "$file"
  done < <(find "$EVENTS_DIR" -name '*.jsonl' -print0)
fi

echo "Migrated $count files."
