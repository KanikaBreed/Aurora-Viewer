//! The work of the last frames before a GPU loss, one line per submitted
//! frame (GPU-loss test builds, #3139): how many copies and bytes the frame
//! streamed, how many buffer and texture writes and submits its journal
//! made, texture-table rebuilds, draws and triangles. The flight recorder's
//! 10 Hz minimum / maximum cannot show the one frame that hung; these lines
//! can. Frozen at the first loss: the recovery's frames never replace them.
//!
//! Numbers only: the public crash report prints them as they are.
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

/// About four seconds at 60 frames per second.
const KEPT: usize = 256;

/// One submitted frame.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FrameLine {
    pub frame: u64,
    /// Milliseconds between this frame's submit and the loss (filled when
    /// frozen).
    pub before_loss_ms: u32,
    /// Streamed copies (buffer regions and texture levels) and their bytes.
    pub upload_copies: u32,
    pub upload_bytes: u64,
    /// The frame journal's buffer writes, texture writes and its own submits
    /// (arena growths, flushes), and the bytes it recorded.
    pub write_buffers: u32,
    pub write_textures: u32,
    pub write_submits: u32,
    pub write_bytes: u64,
    /// Texture-table bind-group rebuilds during this frame.
    pub bind_group_rebuilds: u32,
    pub draws: u32,
    pub shadow_draws: u32,
    pub triangles: u64,
    /// The frame's draw lists were built by the GPU (gpu_cull.rs).
    pub gpu_cull: bool,
}

struct Log {
    lines: [(FrameLine, Option<Instant>); KEPT],
    head: usize,
    len: usize,
    last_rebuilds: Option<u64>,
}

fn log() -> &'static Mutex<Log> {
    static LOG: OnceLock<Mutex<Log>> = OnceLock::new();
    LOG.get_or_init(|| {
        Mutex::new(Log {
            lines: [(FrameLine::default(), None); KEPT],
            head: 0,
            len: 0,
            last_rebuilds: None,
        })
    })
}

static FROZEN: OnceLock<Vec<FrameLine>> = OnceLock::new();
/// Texture-table rebuilds since device creation, noted by the thread that
/// owns the texture table (main_thread.rs).
static REBUILDS: AtomicU64 = AtomicU64::new(0);

pub(crate) fn note_rebuilds(total: u64) {
    REBUILDS.store(total, Ordering::Relaxed);
}

/// A frame was submitted; the line keeps this frame's share of the
/// texture-table rebuilds.
pub(crate) fn push(mut line: FrameLine) {
    let rebuilds_total = REBUILDS.load(Ordering::Relaxed);
    if FROZEN.get().is_some() {
        return;
    }
    let Ok(mut log) = log().try_lock() else {
        return;
    };
    // A recovered device starts counting again: no negative share.
    line.bind_group_rebuilds = log
        .last_rebuilds
        .map_or(0, |last| rebuilds_total.saturating_sub(last))
        .min(u64::from(u32::MAX)) as u32;
    log.last_rebuilds = Some(rebuilds_total);
    let at = (log.head + log.len) % KEPT;
    log.lines[at] = (line, Some(Instant::now()));
    if log.len < KEPT {
        log.len += 1;
    } else {
        log.head = (log.head + 1) % KEPT;
    }
}

/// The first GPU loss: the lines so far are kept for the report. Never
/// waits (it can run inside the driver's loss callback).
pub fn freeze() {
    if FROZEN.get().is_some() {
        return;
    }
    let Ok(log) = log().try_lock() else {
        return;
    };
    let now = Instant::now();
    let lines = (0..log.len)
        .filter_map(|i| {
            let (mut line, at) = log.lines[(log.head + i) % KEPT];
            let at = at?;
            line.before_loss_ms = now.saturating_duration_since(at).as_millis().min(u128::from(u32::MAX)) as u32;
            Some(line)
        })
        .collect();
    let _ = FROZEN.set(lines);
}

/// The lines kept at the first loss, oldest first.
pub fn frozen() -> Option<&'static [FrameLine]> {
    FROZEN.get().map(Vec::as_slice)
}

impl FrameLine {
    /// One line of numbers (gpu-frames.txt).
    pub fn encode(&self) -> String {
        format!(
            "{} {} {} {} {} {} {} {} {} {} {} {} {}",
            self.frame,
            self.before_loss_ms,
            self.upload_copies,
            self.upload_bytes,
            self.write_buffers,
            self.write_textures,
            self.write_submits,
            self.write_bytes,
            self.bind_group_rebuilds,
            self.draws,
            self.shadow_draws,
            self.triangles,
            u8::from(self.gpu_cull)
        )
    }

    /// A line of [`Self::encode`]; anything else (text, a wrong count of
    /// fields, a flag that is not 0 or 1) is refused.
    pub fn decode(text: &str) -> Option<Self> {
        if text.len() > 256 {
            return None;
        }
        let numbers: Vec<u64> = text.split_whitespace().map(str::parse).collect::<Result<_, _>>().ok()?;
        let [
            frame,
            before,
            copies,
            bytes,
            buffers,
            textures,
            submits,
            write_bytes,
            rebuilds,
            draws,
            shadow,
            triangles,
            cull,
        ] = numbers.as_slice()
        else {
            return None;
        };
        let small = |v: u64| u32::try_from(v).ok();
        if *cull > 1 {
            return None;
        }
        Some(Self {
            frame: *frame,
            before_loss_ms: small(*before)?,
            upload_copies: small(*copies)?,
            upload_bytes: *bytes,
            write_buffers: small(*buffers)?,
            write_textures: small(*textures)?,
            write_submits: small(*submits)?,
            write_bytes: *write_bytes,
            bind_group_rebuilds: small(*rebuilds)?,
            draws: small(*draws)?,
            shadow_draws: small(*shadow)?,
            triangles: *triangles,
            gpu_cull: *cull == 1,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_lines_round_trip_and_refuse_text() {
        let line = FrameLine {
            frame: 42847,
            before_loss_ms: 1000,
            upload_copies: 812,
            upload_bytes: 33 << 20,
            write_buffers: 4100,
            write_textures: 12,
            write_submits: 2,
            write_bytes: 9 << 20,
            bind_group_rebuilds: 1,
            draws: 3120,
            shadow_draws: 900,
            triangles: 4_200_000,
            gpu_cull: true,
        };
        assert_eq!(FrameLine::decode(&line.encode()), Some(line));
        for bad in [
            "",
            "1 2 3",
            "a b c d e f g h i j k l m",
            "1 2 3 4 5 6 7 8 9 10 11 12 2",
            "1 2 3 4 5 6 7 8 9 10 11 12 1 14",
        ] {
            assert_eq!(FrameLine::decode(bad), None, "{bad}");
        }
    }
}
