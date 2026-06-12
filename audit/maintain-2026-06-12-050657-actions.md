All dependencies are now at their latest compatible versions. Here's a summary of what happened:

**16 transitive dependencies bumped (all patch updates):**
- `prost` family: 0.14.3 → 0.14.4
- `regex` / `regex-syntax`: 1.12.3→1.12.4 / 0.8.10→0.8.11
- `uuid`: 1.23.2 → 1.23.3
- `http`: 1.4.1 → 1.4.2
- `memchr`: 2.8.1 → 2.8.2
- `smallvec`: 1.15.1 → 1.15.2
- `block-buffer`: 0.12.0 → 0.12.1
- `wasm-bindgen` family: 0.2.122 → 0.2.123
- `js-sys`: 0.3.99 → 0.3.100

**One package remains held back:**
- `matchit` stays at 0.8.4 — `axum v0.8.9` (the latest axum) hard-pins it with `=0.8.4`, so there's nothing to do until axum releases a version that relaxes that pin.

All six quality gates passed cleanly: fmt ✓, clippy ✓, tests ✓, docs ✓, coverage (78.06% > 61%) ✓, deny ✓.