Here's a summary of what was done:

## Dependency Update Results

### ✅ Updated (2 dependencies)

| Dependency | Old | New | Notes |
|---|---|---|---|
| `strum` / `strum_macros` | `0.27` | `0.28` | MSRV-compatible, no breaking changes affecting this project |
| `sha2` (+ `digest`) | `0.10` | `0.11` | Core `Digest` trait API (`new()` / `update()` / `finalize()`) unchanged |

### ⏭️ Skipped (2 dependency pairs)

| Dependency | Current | Latest | Reason Skipped |
|---|---|---|---|
| `tonic` / `tonic-build` | `0.13` | `0.14` | **Multi-step migration required** — tonic 0.14 extracted prost into new `tonic-prost` and `tonic-prost-build` crates; import paths, `build.rs`, and `include_proto!` usage all need updating |
| `prost` / `prost-types` | `0.13` | `0.14` | **Must move together with tonic** — also has its own breaking changes (e.g. `Debug` no longer a supertrait of `Message`) |

### ✅ All Quality Gates Pass

- `cargo fmt --all -- --check` — clean
- `cargo clippy --workspace -- -D warnings` — clean  
- `cargo test --workspace` — 401 passed, 0 failed
- `cargo deny check` — clean (pre-existing duplicate warnings from tonic's transitive tree were already present)

---

The `tonic` + `prost` 0.14 migration is a meaningful breaking change that would be worth tackling as a dedicated task — want me to do that as a separate focused effort?