//! When a playing stream has gone too long without a data callback.
//!
//! cpal 0.18's WASAPI host waits forever in `wait_for_handle_signal`
//! (`WaitForMultipleObjectsEx` with `INFINITE`, `host/wasapi/stream.rs`
//! around line 584) when the endpoint disappears, so the error callback
//! never runs and the stream stays silent. The Legacy Viewer re-picks the
//! output when FMOD reports `FMOD_SYSTEM_CALLBACK_DEVICELISTCHANGED`
//! (`systemCallback` in `indra/llaudio/llaudioengine_fmodstudio.cpp`).
//!
//! The decision is pure so it can be tested without a device. The output
//! and capture threads share it; the PulseAudio default-device watch is a
//! separate path and is not involved.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// Silence after which a playing stream is dropped and opened again.
/// Long enough that a late callback (a busy machine, the 40 ms queue) is
/// not a stall, short enough that sound returns within a few seconds.
pub(crate) const CALLBACK_STALL: Duration = Duration::from_secs(2);

/// Longest silence required between reopens while the callback stays away.
pub(crate) const STALL_BACKOFF_MAX: Duration = Duration::from_secs(30);

/// Whether to drop the stream and open the current device again.
///
/// `since_callback` is how long the data callback has been absent, from the
/// last one, or from the moment the stream started when none has arrived.
/// `playing` is whether that callback is expected. `backoff` is the silence
/// required this time: [`CALLBACK_STALL`] at first, then longer after a
/// reopen that did not bring the callback back ([`next_stall_backoff`]).
pub(crate) fn stream_callback_stalled(since_callback: Duration, playing: bool, backoff: Duration) -> bool {
    playing && since_callback >= CALLBACK_STALL && since_callback >= backoff
}

/// The next backoff. `recovered` once a data callback has arrived; otherwise
/// the wait doubles, up to [`STALL_BACKOFF_MAX`], and never under the stall.
pub(crate) fn next_stall_backoff(backoff: Duration, recovered: bool) -> Duration {
    if recovered {
        CALLBACK_STALL
    } else {
        backoff.saturating_mul(2).clamp(CALLBACK_STALL, STALL_BACKOFF_MAX)
    }
}

/// A reopen the watchdog asked for.
pub(crate) struct Reopen {
    /// How long the callback had been absent.
    pub silent: Duration,
    /// Log this one. False while the same outage is retried.
    pub log: bool,
}

/// Backoff and the once-per-recovery log, across reopens of one thread.
pub(crate) struct StallWatch {
    backoff: Duration,
    logged: bool,
    /// The open in progress is a retry of a stall: keep `backoff` and `logged`.
    from_stall: bool,
    last_stamp: u64,
}

impl StallWatch {
    pub(crate) fn new() -> Self {
        Self {
            backoff: CALLBACK_STALL,
            logged: false,
            from_stall: false,
            last_stamp: 0,
        }
    }

    /// Silence required before the next reopen.
    pub(crate) fn backoff(&self) -> Duration {
        self.backoff
    }

    /// Call before each open. A retry of a stall keeps its backoff; any
    /// other open (the first, a device change, the error callback) starts
    /// the short one again.
    pub(crate) fn begin_open(&mut self) {
        if !self.from_stall {
            self.backoff = CALLBACK_STALL;
            self.logged = false;
        }
        self.last_stamp = 0;
    }

    /// The stream is open, so the preserved backoff now belongs to it.
    pub(crate) fn opened(&mut self) {
        self.from_stall = false;
    }

    /// `playing` is whether a callback is expected. `Some` asks to drop the
    /// stream and open the current device again.
    pub(crate) fn poll(&mut self, seen: &CallbackSeen, playing: bool) -> Option<Reopen> {
        let stamp = seen.stamp();
        let fresh = stamp != 0 && stamp != self.last_stamp;
        if stamp != self.last_stamp {
            self.last_stamp = stamp;
        }
        if fresh {
            self.logged = false;
            self.backoff = next_stall_backoff(self.backoff, true);
        }
        let silent = seen.since();
        if !stream_callback_stalled(silent, playing, self.backoff) {
            return None;
        }
        let log = !self.logged;
        self.logged = true;
        self.backoff = next_stall_backoff(self.backoff, false);
        self.from_stall = true;
        Some(Reopen { silent, log })
    }
}

/// Last data callback, shared by the callback and the thread that owns the
/// stream. The clock starts at [`Self::arm`], once the stream is playing,
/// so the time spent opening the device is not silence.
pub(crate) struct CallbackSeen {
    origin: Instant,
    /// Microseconds since `origin` when the clock was armed, or 0.
    armed_us: AtomicU64,
    /// Microseconds since `origin` of the last callback, or 0 if none yet.
    last_us: AtomicU64,
}

impl CallbackSeen {
    pub(crate) fn new() -> Self {
        Self {
            origin: Instant::now(),
            armed_us: AtomicU64::new(0),
            last_us: AtomicU64::new(0),
        }
    }

    fn micros(&self) -> u64 {
        u64::try_from(self.origin.elapsed().as_micros()).unwrap_or(u64::MAX).max(1)
    }

    /// Start measuring silence. Call once `play` has returned.
    pub(crate) fn arm(&self) {
        self.armed_us.store(self.micros(), Ordering::Release);
    }

    /// A data callback ran.
    pub(crate) fn mark(&self) {
        self.last_us.store(self.micros(), Ordering::Release);
    }

    /// Stamp of the last callback, 0 if none yet.
    pub(crate) fn stamp(&self) -> u64 {
        self.last_us.load(Ordering::Acquire)
    }

    /// Time since the last callback, or since [`Self::arm`] when none has
    /// arrived. Zero before the clock is armed.
    pub(crate) fn since(&self) -> Duration {
        let now = self.micros();
        let armed = self.armed_us.load(Ordering::Acquire);
        if armed == 0 {
            return Duration::ZERO;
        }
        let last = self.last_us.load(Ordering::Acquire);
        let from = if last == 0 { armed } else { last };
        Duration::from_micros(now.saturating_sub(from))
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{CALLBACK_STALL, next_stall_backoff, stream_callback_stalled};

    #[test]
    fn a_playing_stream_reopens_only_after_a_stall_and_its_backoff() {
        let stall = CALLBACK_STALL;
        // A callback that keeps arriving is not a stall, on any backoff.
        assert!(
            !stream_callback_stalled(Duration::from_millis(10), true, stall),
            "a live callback period must not reopen"
        );
        assert!(!stream_callback_stalled(Duration::from_millis(1_999), true, stall));
        assert!(!stream_callback_stalled(Duration::from_millis(20), true, Duration::from_secs(8)));

        // Playing, and silent for the stall, with the initial backoff: reopen.
        assert!(stream_callback_stalled(stall, true, stall));
        assert!(stream_callback_stalled(Duration::from_secs(5), true, stall));
        // The floor is the stall even when the backoff is shorter.
        assert!(!stream_callback_stalled(Duration::from_millis(500), true, Duration::ZERO));

        // Not playing: an idle stream is left alone, however long the gap.
        assert!(!stream_callback_stalled(stall, false, stall));
        assert!(!stream_callback_stalled(Duration::from_secs(30), false, stall));
        assert!(!stream_callback_stalled(Duration::from_secs(30), false, Duration::ZERO));

        // After a reopen that brought no callback, the longer backoff holds.
        let backoff = Duration::from_secs(4);
        assert!(!stream_callback_stalled(Duration::from_secs(2), true, backoff));
        assert!(!stream_callback_stalled(Duration::from_secs(3), true, backoff));
        assert!(stream_callback_stalled(backoff, true, backoff));
        assert!(stream_callback_stalled(Duration::from_secs(5), true, backoff));

        // Backoff doubles from the stall and stops at the cap; a callback resets it.
        assert_eq!(next_stall_backoff(stall, false), Duration::from_secs(4));
        assert_eq!(next_stall_backoff(Duration::from_secs(4), false), Duration::from_secs(8));
        assert_eq!(next_stall_backoff(Duration::from_secs(16), false), Duration::from_secs(30));
        assert_eq!(next_stall_backoff(Duration::from_secs(30), false), Duration::from_secs(30));
        assert_eq!(next_stall_backoff(Duration::from_secs(8), true), stall);
        assert_eq!(next_stall_backoff(Duration::ZERO, false), stall);
    }
}
