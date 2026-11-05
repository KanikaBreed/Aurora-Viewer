# Handoff: #2474 — Ignore other avatars' face lights

Branch: `feat/others-face-lights-2474`

The Graphics preferences now include “Lights worn by other avatars (face lights)”. It defaults on. Turning it off filters lights attached to other residents while retaining lights on your own avatar. The photo window's attached-light option remains independent and can still suppress worn lights while taking a photo.

Human in-world validation: wear an attachment with a light, stand near another resident wearing a face light, and compare the preference on and off. With it off, the other resident's light should stop illuminating the scene while your own remains visible. With it on, both should illuminate the scene. Also verify the photo window's worn-light switch works without changing the saved Graphics preference.

Checks: `cargo fmt --all --check` passed. The focused regression passed. Strict `cargo +1.99.1 clippy --workspace --all-targets --locked -- -D warnings` is blocked by the repository's existing Linux dead-code warnings (18 unrelated Windows-only items); rerunning with `-A dead_code` passed. `cargo test --workspace --no-fail-fast` ran 6,199 passing tests and 11 ignored; the three known failures remain: `water_mesh_depth_clamp_preserves_coverage_and_occlusion_on_gpu`, `pbr_terrain_samples_all_layers_maps_and_triplanar_normals_on_gpu`, and `what_inter_lacks_comes_from_the_same_faces_as_with_noto`. The human can run Clippy and all checks as well.
