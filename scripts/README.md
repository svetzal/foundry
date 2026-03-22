# scripts/

This directory contains helper scripts for integrating Foundry into existing workflows.

## deprecation-bridge.sh

`deprecation-bridge.sh` is an **example bridge script** that shows how to migrate an
existing `maintain.sh` (or similar shell automation) to delegate to the Foundry daemon
when it is available, while preserving the original fallback behaviour when it is not.

### How it works

1. Checks whether the `foundry` CLI is on `PATH` (or the path set by `$FOUNDRY_CLI`).
2. Calls `foundry status` to determine whether `foundryd` is responsive.
3. If the daemon is up, prints a deprecation notice to stderr and delegates via
   `exec foundry run "$@"` — the process is replaced, so all arguments and the exit
   code are forwarded transparently.
4. If the daemon is not available, execution falls through to the original shell logic
   with no change in behaviour.

### Integrating into your maintain.sh

Copy the delegation block from `deprecation-bridge.sh` to the **top** of your
`maintain.sh`, before any existing logic:

```bash
#!/usr/bin/env bash
set -euo pipefail

FOUNDRY_CLI="${FOUNDRY_CLI:-foundry}"

if command -v "$FOUNDRY_CLI" &>/dev/null; then
    if "$FOUNDRY_CLI" status &>/dev/null 2>&1; then
        echo "[DEPRECATED] Delegating to Foundry daemon." >&2
        exec "$FOUNDRY_CLI" run "$@"
        exit 1
    fi
fi

# --- rest of your existing maintain.sh ---
```

### Environment variables

| Variable       | Default    | Purpose                                      |
|----------------|------------|----------------------------------------------|
| `FOUNDRY_CLI`  | `foundry`  | Path or name of the `foundry` CLI binary     |

### Testing

To verify the bridge behaves correctly in both modes:

```bash
# Syntax check
bash -n scripts/deprecation-bridge.sh

# With foundryd running — should print the deprecation notice and delegate:
./scripts/deprecation-bridge.sh

# With foundryd stopped — should print the fallback info message:
./scripts/deprecation-bridge.sh
```
