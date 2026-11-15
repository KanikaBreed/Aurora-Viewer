//! The render and test switches (`AURORA_*` variables) a session ran with,
//! for the crash report's "Test switches" row: noted at the session's start
//! (`test-switches.txt`), read back when the report is built. Only the
//! variables of [`ALLOWED`] (and the `AURORA_TEST_*` family), each value cut
//! to a few plain characters; a value with anything else (a path, a space)
//! shows as `set`. Never the user's data: these variables only choose a
//! renderer path.

use std::path::Path;

pub(crate) const FILE: &str = "test-switches.txt";

/// The render switches of the GPU-loss and freeze test builds and of the
/// diagnostics: what changes how the card is driven.
const ALLOWED: &[&str] = &[
    "AURORA_AA",
    "AURORA_ALLOCATOR",
    "AURORA_AO",
    "AURORA_BLOOM",
    "AURORA_BLEND_ORDER",
    "AURORA_CONTACT_SHADOWS",
    "AURORA_CONTACT_VARIANT",
    "AURORA_CPU_CULL",
    "AURORA_DEMO_GPU_HANG",
    "AURORA_DEMO_GPU_LOST",
    "AURORA_DEMO_GPU_STALL",
    "AURORA_EYE_RELATIVE",
    "AURORA_FETCH_SLOTS",
    "AURORA_FPS_LIMIT",
    "AURORA_FRAME_PACING",
    "AURORA_FRAME_PRIORITY",
    "AURORA_GPU_BREADCRUMBS",
    "AURORA_GPU_FAULT_UNCONFIRMED",
    "AURORA_GPU_LABELS",
    "AURORA_GPU_OOM",
    "AURORA_GPU_OOM_FIRST_USE",
    "AURORA_GPU_OOM_START",
    "AURORA_GPU_OOM_TARGETS",
    "AURORA_GPU_RECOVERY",
    "AURORA_GPU_START_FAIL",
    "AURORA_GPU_VALIDATION",
    "AURORA_GRAPHICS_PRESET",
    "AURORA_HDR",
    "AURORA_LIGHT_TILES",
    "AURORA_LOCAL_LIGHTS",
    "AURORA_MAPPABLE_MB",
    "AURORA_MAX_TEXTURE_RES",
    "AURORA_MSAA_PREPASS",
    "AURORA_NO_OCCLUSION",
    "AURORA_NOVSYNC",
    "AURORA_OCCLUSION",
    "AURORA_OIT",
    "AURORA_PIPELINE_CACHE",
    "AURORA_PRESENT_MODE",
    "AURORA_RENDER_SCALE",
    "AURORA_RENDER_THREAD",
    "AURORA_SHADOWS",
    "AURORA_SSIL",
    "AURORA_TEXTURE_BC7",
    "AURORA_TEXTURE_BUDGET_MB",
    "AURORA_TEXTURE_SLOTS",
    "AURORA_VRAM_BUDGET_MB",
    "AURORA_VULKAN_DRIVERS",
    "AURORA_VULKAN_LAYERS",
    "AURORA_WRITE_STAGING",
];

/// The longest value printed.
const VALUE_LEN: usize = 16;
/// At most this many switches.
const MOST: usize = 24;

fn allowed(name: &str) -> bool {
    ALLOWED.contains(&name)
        || name
            .strip_prefix("AURORA_TEST_")
            .is_some_and(|rest| !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_'))
}

/// `value` as the report prints it: cut to [`VALUE_LEN`] characters, or
/// `set` when it holds anything but letters, digits and `.,:+-_=*`.
fn shown(value: &str) -> String {
    let value = value.trim();
    if value.is_empty() {
        return "(empty)".to_owned();
    }
    if !value.chars().all(|c| c.is_ascii_alphanumeric() || ".,:+-_=*".contains(c)) {
        return "set".to_owned();
    }
    if value.chars().count() > VALUE_LEN {
        format!("{}...", value.chars().take(VALUE_LEN).collect::<String>())
    } else {
        value.to_owned()
    }
}

/// The allowed switches among `vars`, as `NAME=value`, sorted by name.
pub(crate) fn from_vars(vars: impl IntoIterator<Item = (String, String)>) -> Vec<String> {
    let mut out: Vec<String> = vars
        .into_iter()
        .filter(|(name, _)| allowed(name))
        .map(|(name, value)| format!("{name}={}", shown(&value)))
        .collect();
    out.sort();
    out.dedup();
    out.truncate(MOST);
    out
}

/// At the session's start: the switches of this process.
pub(crate) fn note(dir: &Path) {
    let vars = std::env::vars_os()
        .filter_map(|(name, value)| Some((name.into_string().ok()?, value.into_string().unwrap_or_else(|_| "?".into()))));
    let _ = std::fs::write(dir.join(FILE), from_vars(vars).join("\n"));
}

/// The row: the noted switches, checked again line by line (a line that is
/// not an allowed `NAME=value` is left out), "none" when there are none.
pub(crate) fn row(text: &str) -> String {
    let switches = from_vars(text.lines().take(64).filter_map(|line| {
        let (name, value) = line.trim().split_once('=')?;
        Some((name.to_owned(), value.to_owned()))
    }));
    if switches.is_empty() {
        "none".to_owned()
    } else {
        switches.join(", ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars(list: &[(&str, &str)]) -> Vec<(String, String)> {
        list.iter().map(|(n, v)| (n.to_string(), v.to_string())).collect()
    }

    #[test]
    fn only_render_and_test_switches_with_plain_values() {
        let noted = from_vars(vars(&[
            ("AURORA_CPU_CULL", "1"),
            ("AURORA_TEST_SLOW_UPLOADS", "1"),
            ("AURORA_GPU_LABELS", " 1 "),
            ("AURORA_VULKAN_DRIVERS", "C:\\Users\\someone\\driver.json"),
            ("AURORA_VULKAN_LAYERS", "-VK_LAYER_OBS_HOOK,-VK_LAYER_RTSS"),
            ("AURORA_MAPPABLE_MB", "0"),
            // not a render switch, and not ours
            ("AURORA_LOG_NAME", "mine"),
            ("AURORA_DEMO", "1"),
            ("HOME", "/home/someone"),
            ("AURORA_TEST_", "1"),
            ("AURORA_TEST_lower", "1"),
            ("AURORA_PRESENT_MODE", "immediate mode"),
        ]));
        assert_eq!(
            noted,
            vec![
                "AURORA_CPU_CULL=1",
                "AURORA_GPU_LABELS=1",
                "AURORA_MAPPABLE_MB=0",
                "AURORA_PRESENT_MODE=set",
                "AURORA_TEST_SLOW_UPLOADS=1",
                "AURORA_VULKAN_DRIVERS=set",
                "AURORA_VULKAN_LAYERS=-VK_LAYER_OBS_HO...",
            ]
        );
    }

    #[test]
    fn the_row_reads_the_noted_file_again() {
        assert_eq!(row(""), "none");
        assert_eq!(
            row("AURORA_NO_OCCLUSION=1\nAURORA_TEST_FULLY_BOUND=1\n"),
            "AURORA_NO_OCCLUSION=1, AURORA_TEST_FULLY_BOUND=1"
        );
        // a line written by something else is checked again
        assert_eq!(row("HOME=/home/someone\nAURORA_CPU_CULL=/tmp/x\ngarbage"), "AURORA_CPU_CULL=set");
        let dir = std::env::temp_dir().join(format!("aurora-test-switches-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        note(&dir);
        assert!(dir.join(FILE).exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
