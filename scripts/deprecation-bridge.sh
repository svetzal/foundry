#!/usr/bin/env bash
# deprecation-bridge.sh — Example bridge for migrating from maintain.sh to foundry
#
# USAGE: Copy this logic into your maintain.sh to delegate to Foundry when available.
#
# This script demonstrates the migration pattern:
#   1. Check if the foundry CLI is installed
#   2. Check if foundryd daemon is responsive via `foundry status`
#   3. Delegate to `foundry run` if available, else fall back to original logic

set -euo pipefail

FOUNDRY_CLI="${FOUNDRY_CLI:-foundry}"
DEPRECATION_MSG="[DEPRECATED] This script is being replaced by Foundry daemon. Using foundry run instead."

# Check if foundry CLI is installed
if command -v "$FOUNDRY_CLI" &>/dev/null; then
    # Check if foundryd daemon is reachable by running foundry status.
    # foundry status exits 0 when the daemon is up and responsive.
    if "$FOUNDRY_CLI" status &>/dev/null 2>&1; then
        echo "$DEPRECATION_MSG" >&2
        # exec replaces this process with foundry run, forwarding all arguments.
        exec "$FOUNDRY_CLI" run "$@"
        # If exec somehow returns, something went wrong.
        exit 1
    fi
fi

# Fallback: daemon not available — run original maintain.sh logic here.
echo "[INFO] Foundry daemon not available. Running in standalone mode." >&2

# --- Original maintain.sh implementation ---
# Replace this section with your existing maintain.sh logic.
# Example:
#   hone iterate
#   git add -A && git commit -m "chore: maintenance run" && git push
echo "[INFO] Running maintenance (stub — replace with real implementation)" >&2
