# Handoff: SLurl launches a second Aurora (#2402)

Branch: `fix/slurl-handoff-2402`

## Change

- Unexpected Windows named-pipe hand-off failures are written to stderr and appended to Aurora's log file before the normal logger starts. The diagnostic includes the Windows error code, pipe name and registered command.
- The pipe security descriptor grants the owner full access and sets medium mandatory integrity with no-write-up, allowing a browser-launched medium-integrity process to connect to an elevated Aurora process. Remote clients remain rejected.
- Startup repair now re-points a URL registration naming another `aurora-viewer.exe` to the current executable, even when that old executable still exists.
- Added offline tests for another Aurora install, the current install and a different viewer executable.

## Human validation in Windows

1. Start Aurora elevated, set it as the `secondlife:` handler, then open a `secondlife:` URL from a browser running at medium integrity. Confirm the existing elevated window receives the URL and a second viewer does not start.
2. Repeat with a current-user registration pointing to another existing Aurora installation. Start this installation and confirm registration repair points the scheme at the running executable.
3. Confirm ordinary unelevated launches and unrelated viewer registrations behave as before.
4. If hand-off fails, inspect the beginning/appended lines in the Aurora log for the Windows error code and registered command.

No grid connection or in-world testing is needed or performed. This Linux environment cannot perform the Windows elevation/browser checks; a Windows human validation remains required.

## Checks

Validation results:

- `cargo fmt --all --check` — passed.
- `cargo test -p aurora-viewer url_scheme::registration_tests --locked` — passed, 2 tests.
- `cargo test --workspace` — 352 passed, 2 failed, 32 ignored before the render crate stopped the workspace run. The failures are GPU/RADV-sensitive: `water_mesh_depth_clamp_preserves_coverage_and_occlusion_on_gpu` (depth delta) and `pbr_terrain_samples_all_layers_maps_and_triplanar_normals_on_gpu` (`0.39999998` versus `0.4`).
- `cargo clippy --workspace --all-targets --locked -- -D warnings` — blocked by existing `manual_range_contains` in `crates/aurora-assets/src/anim.rs:211`.
- `cargo check -p aurora-viewer --target x86_64-pc-windows-gnu --locked` — blocked in the `openjpeg-sys` build script because `x86_64-w64-mingw32-gcc` is not installed. The Windows-specific behavior therefore still needs a Windows build and human validation.

Commands run:

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace
```
