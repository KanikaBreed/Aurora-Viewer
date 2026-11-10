//! GPU-loss test build switches that slow the streaming down (#3139): the
//! RX 9070 XT reset its card after bursts of new content (login, a preset
//! jump, a teleport into a crowded region, and the full reload after each
//! recovery). These caps tell a burst-triggered hang from the rest.
//!
//! - `AURORA_TEST_SLOW_UPLOADS=1`: every frame at most [`TEXTURES`]
//!   textures, [`TEXTURE_BYTES`] of texels and [`GEOMETRIES`] geometries;
//! - `AURORA_TEST_GENTLE_RECOVERY=1`: the same caps for [`GENTLE_FOR`]
//!   after each GPU recovery only.
use std::sync::Mutex;
use std::time::{Duration, Instant};

pub const TEXTURES: usize = 4;
pub const TEXTURE_BYTES: u64 = 4 << 20;
pub const GEOMETRIES: usize = 8;
pub const GENTLE_FOR: Duration = Duration::from_secs(60);

static RECOVERED_AT: Mutex<Option<Instant>> = Mutex::new(None);

fn on(name: &str) -> bool {
    std::env::var(name).is_ok_and(|v| v.trim() == "1")
}

pub fn slow_uploads() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| on("AURORA_TEST_SLOW_UPLOADS"))
}

pub fn gentle_recovery() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| on("AURORA_TEST_GENTLE_RECOVERY"))
}

/// A GPU recovery finished: the scene streams again from nothing.
pub fn note_recovery() {
    if let Ok(mut at) = RECOVERED_AT.lock() {
        *at = Some(Instant::now());
    }
}

/// Whether this frame's streaming is capped.
pub fn capped() -> bool {
    slow_uploads() || (gentle_recovery() && within_gentle(RECOVERED_AT.lock().ok().and_then(|at| *at), Instant::now()))
}

fn within_gentle(recovered: Option<Instant>, now: Instant) -> bool {
    recovered.is_some_and(|at| now.saturating_duration_since(at) < GENTLE_FOR)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_gentle_window_lasts_a_minute_after_a_recovery_only() {
        let now = Instant::now();
        assert!(!within_gentle(None, now));
        assert!(within_gentle(Some(now), now + Duration::from_secs(59)));
        assert!(!within_gentle(Some(now), now + GENTLE_FOR));
    }
}
