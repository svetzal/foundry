All dependencies are now updated and all quality gates pass. Here's what changed:

## What Was Updated

### gRPC Stack (major version bumps — confirmed no breaking changes at call sites)

| Package | From | To |
|---|---|---|
| `tonic` | 0.13.1 | 0.14.5 |
| `tonic-build` | 0.13.1 | 0.14.5 |
| `prost` | 0.13.5 | 0.14.3 |
| `prost-types` | 0.13.5 | 0.14.3 |

**Migration note:** Tonic 0.14 extracted prost integration into two new companion crates. This required:
- Adding `tonic-prost` as a runtime dependency (the generated proto code now references `tonic_prost::ProstCodec`)
- Replacing `tonic-build` with `tonic-prost-build` in build dependencies
- Updating both `build.rs` files to call `tonic_prost_build::compile_protos` instead of `tonic_build::compile_protos`

The public APIs (`tonic::Request/Response/Status`, `#[tonic::async_trait]`, `tonic::include_proto!`) are unchanged.

### Everything else — already current
All other dependencies (`tokio`, `clap`, `serde`, `anyhow`, `thiserror`, `tracing`, `chrono`, `clap`, `comfy-table`, etc.) were already at their latest compatible versions within their semver ranges — `cargo update` resolved `tokio` to `1.52.1` automatically via lockfile without needing a `Cargo.toml` change.

### `deny.toml` — one license added
Added `"Zlib"` to the license allowlist. `foldhash` (a transitive dep pulled in via `prost-build 0.14`) carries the Zlib license — it's OSI-approved and permissive.

## Quality Gate Results

| Gate | Result |
|---|---|
| `cargo fmt --all -- --check` | ✅ Passed |
| `cargo clippy --workspace -- -D warnings` | ✅ Passed (zero warnings) |
| `cargo test --workspace` | ✅ Passed — 401 tests, 0 failures |
| `cargo deny check` | ✅ Passed |