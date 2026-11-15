//! The work of the last frames before a GPU loss, one line per submitted
//! frame (GPU-loss test builds, #3139): how many copies and bytes the frame
//! streamed, how many buffer and texture writes and submits its journal
//! made, texture-table rebuilds, draws and triangles, and the streaming
//! events of the frame (#3012, #2968): pipelines made, texture pages made,
//! buffers (re)made with the largest, the largest single upload and its
//! kind, meshes added. The flight recorder's 10 Hz minimum / maximum cannot
//! show the one frame that hung; these lines can. Frozen at the first loss:
//! the recovery's frames never replace them.
//!
//! The events are counted where they happen, on any thread, with relaxed
//! atomics (no lock, no allocation); each line takes its share of the totals
//! since the line before. The labels of the pipelines made in the last
//! seconds are kept too (our own static names, filtered again), printed once
//! under the table.
//!
//! Numbers only in the lines: the public crash report prints them as they
//! are.
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// About four seconds at 60 frames per second.
const KEPT: usize = 256;

/// What the largest upload of a frame was.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum UploadKind {
    #[default]
    None,
    /// A streamed texture (all its levels, upload.rs `copy_to_layer`).
    Texture,
    /// A streamed mesh (vertices, skin and indices, arena.rs).
    Mesh,
    /// A queue write of the frame journal (records, palettes, a mesh or a
    /// texture written by the main thread, writes.rs).
    Write,
}

impl UploadKind {
    fn code(self) -> u8 {
        match self {
            UploadKind::None => 0,
            UploadKind::Texture => 1,
            UploadKind::Mesh => 2,
            UploadKind::Write => 3,
        }
    }

    fn from_code(code: u64) -> Option<Self> {
        Some(match code {
            0 => UploadKind::None,
            1 => UploadKind::Texture,
            2 => UploadKind::Mesh,
            3 => UploadKind::Write,
            _ => return None,
        })
    }

    /// The report's word.
    pub fn word(self) -> &'static str {
        match self {
            UploadKind::None => "-",
            UploadKind::Texture => "texture",
            UploadKind::Mesh => "mesh",
            UploadKind::Write => "write",
        }
    }
}

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
    /// Render and compute pipelines made since the line before
    /// (pipeline_cache.rs).
    pub pipelines: u32,
    /// Texture pages made since the line before (textures.rs).
    pub texture_pages: u32,
    /// Buffers made or made again larger through the growable path
    /// (gpu_alloc.rs `try_buffer`: arenas, record tables, culling buffers),
    /// and the size of the largest.
    pub buffers_made: u32,
    pub largest_buffer_bytes: u64,
    /// The largest single upload of the frame and its kind.
    pub largest_upload_bytes: u64,
    pub largest_upload_kind: UploadKind,
    /// Meshes added to the geometry arena since the line before.
    pub meshes_added: u32,
}

/// The event totals since the start, each line taking its share.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Totals {
    rebuilds: u64,
    pipelines: u64,
    pages: u64,
    buffers: u64,
    meshes: u64,
}

impl Totals {
    fn now() -> Self {
        Totals {
            rebuilds: REBUILDS.load(Ordering::Relaxed),
            pipelines: PIPELINES.load(Ordering::Relaxed),
            pages: PAGES.load(Ordering::Relaxed),
            buffers: BUFFERS.load(Ordering::Relaxed),
            meshes: MESHES.load(Ordering::Relaxed),
        }
    }

    /// The share of `self` since `last` (none for the first line: what came
    /// before it was the start). A recovered device starts counting its
    /// rebuilds again: no negative share.
    fn since(self, last: Option<Totals>) -> Totals {
        let Some(last) = last else {
            return Totals::default();
        };
        Totals {
            rebuilds: self.rebuilds.saturating_sub(last.rebuilds),
            pipelines: self.pipelines.saturating_sub(last.pipelines),
            pages: self.pages.saturating_sub(last.pages),
            buffers: self.buffers.saturating_sub(last.buffers),
            meshes: self.meshes.saturating_sub(last.meshes),
        }
    }
}

struct Log {
    lines: [(FrameLine, Option<Instant>); KEPT],
    head: usize,
    len: usize,
    last: Option<Totals>,
}

fn log() -> &'static Mutex<Log> {
    static LOG: OnceLock<Mutex<Log>> = OnceLock::new();
    LOG.get_or_init(|| {
        Mutex::new(Log {
            lines: [(FrameLine::default(), None); KEPT],
            head: 0,
            len: 0,
            last: None,
        })
    })
}

/// What the first loss kept.
struct Frozen {
    lines: Vec<FrameLine>,
    pipelines: Vec<RecentPipeline>,
}

static FROZEN: OnceLock<Frozen> = OnceLock::new();
/// Texture-table rebuilds since device creation, noted by the thread that
/// owns the texture table (main_thread.rs).
static REBUILDS: AtomicU64 = AtomicU64::new(0);
static PIPELINES: AtomicU64 = AtomicU64::new(0);
static PAGES: AtomicU64 = AtomicU64::new(0);
static BUFFERS: AtomicU64 = AtomicU64::new(0);
/// The largest buffer made since the line before (taken by each line).
static LARGEST_BUFFER: AtomicU64 = AtomicU64::new(0);
static MESHES: AtomicU64 = AtomicU64::new(0);

pub(crate) fn note_rebuilds(total: u64) {
    REBUILDS.store(total, Ordering::Relaxed);
}

/// A texture page was made (textures.rs, on the page thread or the frame's).
pub(crate) fn note_texture_page() {
    PAGES.fetch_add(1, Ordering::Relaxed);
}

/// A buffer of `bytes` was made through the growable path (gpu_alloc.rs).
pub(crate) fn note_buffer(bytes: u64) {
    BUFFERS.fetch_add(1, Ordering::Relaxed);
    LARGEST_BUFFER.fetch_max(bytes, Ordering::Relaxed);
}

/// A mesh was added to the geometry arena (arena.rs).
pub(crate) fn note_mesh() {
    MESHES.fetch_add(1, Ordering::Relaxed);
}

/// The pipelines made in the last [`PIPELINE_WINDOW`] before a loss are
/// listed; at most this many are remembered.
const PIPELINES_KEPT: usize = 32;
const PIPELINE_WINDOW: Duration = Duration::from_secs(4);
/// The longest label printed.
const LABEL_LEN: usize = 48;

/// A pipeline made recently: its label and how long before the loss.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecentPipeline {
    pub before_loss_ms: u32,
    pub label: String,
}

/// The labels of the last pipelines made, newest last.
#[derive(Default)]
struct PipelineRing {
    made: std::collections::VecDeque<(String, Instant)>,
}

impl PipelineRing {
    fn push(&mut self, label: String, at: Instant) {
        if self.made.len() == PIPELINES_KEPT {
            self.made.pop_front();
        }
        self.made.push_back((label, at));
    }

    /// Those made in the window before `now`, oldest first.
    fn recent(&self, now: Instant) -> Vec<RecentPipeline> {
        self.made
            .iter()
            .filter_map(|(label, at)| {
                let before = now.saturating_duration_since(*at);
                (before <= PIPELINE_WINDOW).then(|| RecentPipeline {
                    before_loss_ms: before.as_millis().min(u128::from(u32::MAX)) as u32,
                    label: label.clone(),
                })
            })
            .collect()
    }
}

fn pipeline_ring() -> &'static Mutex<PipelineRing> {
    static RING: OnceLock<Mutex<PipelineRing>> = OnceLock::new();
    RING.get_or_init(Default::default)
}

/// A pipeline label as the report may print it: letters, digits and a few
/// signs, at most [`LABEL_LEN`] of them; anything else becomes `_`.
pub fn clean_label(label: &str) -> String {
    let label = label.trim();
    if label.is_empty() {
        return "unnamed".to_owned();
    }
    label
        .chars()
        .take(LABEL_LEN)
        .map(|c| {
            if c.is_ascii_alphanumeric() || " _-().,:+#".contains(c) {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// A render or compute pipeline was made (pipeline_cache.rs). Rare (the
/// start, a new kind of content, a settings change): the label is kept.
pub(crate) fn note_pipeline(label: Option<&str>) {
    PIPELINES.fetch_add(1, Ordering::Relaxed);
    if FROZEN.get().is_some() {
        return;
    }
    let label = clean_label(label.unwrap_or(""));
    if let Ok(mut ring) = pipeline_ring().lock() {
        ring.push(label, Instant::now());
    }
}

/// A frame was submitted; the line keeps this frame's share of the
/// texture-table rebuilds and of the other events.
pub(crate) fn push(mut line: FrameLine) {
    let totals = Totals::now();
    let largest_buffer = LARGEST_BUFFER.swap(0, Ordering::Relaxed);
    if FROZEN.get().is_some() {
        return;
    }
    let Ok(mut log) = log().try_lock() else {
        return;
    };
    let share = totals.since(log.last);
    let small = |v: u64| v.min(u64::from(u32::MAX)) as u32;
    line.bind_group_rebuilds = small(share.rebuilds);
    line.pipelines = small(share.pipelines);
    line.texture_pages = small(share.pages);
    line.buffers_made = small(share.buffers);
    line.largest_buffer_bytes = if share.buffers > 0 { largest_buffer } else { 0 };
    line.meshes_added = small(share.meshes);
    log.last = Some(totals);
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
    // (a pipeline being noted right now is left out rather than waited for)
    let pipelines = pipeline_ring().try_lock().map(|ring| ring.recent(now)).unwrap_or_default();
    let _ = FROZEN.set(Frozen { lines, pipelines });
}

/// The lines kept at the first loss, oldest first.
pub fn frozen() -> Option<&'static [FrameLine]> {
    FROZEN.get().map(|f| f.lines.as_slice())
}

/// The pipelines made in the seconds before the first loss, oldest first.
pub fn frozen_pipelines() -> &'static [RecentPipeline] {
    FROZEN.get().map_or(&[], |f| f.pipelines.as_slice())
}

/// The fields of a line of an earlier test build (before the streaming
/// events): still read, the new fields left at zero.
const OLD_FIELDS: usize = 13;
const FIELDS: usize = 20;

impl FrameLine {
    /// One line of numbers (gpu-frames.txt).
    pub fn encode(&self) -> String {
        format!(
            "{} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {}",
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
            u8::from(self.gpu_cull),
            self.pipelines,
            self.texture_pages,
            self.buffers_made,
            self.largest_buffer_bytes,
            self.largest_upload_bytes,
            self.largest_upload_kind.code(),
            self.meshes_added,
        )
    }

    /// A line of [`Self::encode`] (or of an earlier build, without the
    /// streaming events); anything else (text, a wrong count of fields, a
    /// flag that is not 0 or 1, an unknown kind) is refused.
    pub fn decode(text: &str) -> Option<Self> {
        if text.len() > 384 {
            return None;
        }
        let n: Vec<u64> = text.split_whitespace().map(str::parse).collect::<Result<_, _>>().ok()?;
        if n.len() != OLD_FIELDS && n.len() != FIELDS {
            return None;
        }
        let small = |v: u64| u32::try_from(v).ok();
        if n[12] > 1 {
            return None;
        }
        let mut line = Self {
            frame: n[0],
            before_loss_ms: small(n[1])?,
            upload_copies: small(n[2])?,
            upload_bytes: n[3],
            write_buffers: small(n[4])?,
            write_textures: small(n[5])?,
            write_submits: small(n[6])?,
            write_bytes: n[7],
            bind_group_rebuilds: small(n[8])?,
            draws: small(n[9])?,
            shadow_draws: small(n[10])?,
            triangles: n[11],
            gpu_cull: n[12] == 1,
            ..Self::default()
        };
        if n.len() == FIELDS {
            line.pipelines = small(n[13])?;
            line.texture_pages = small(n[14])?;
            line.buffers_made = small(n[15])?;
            line.largest_buffer_bytes = n[16];
            line.largest_upload_bytes = n[17];
            line.largest_upload_kind = UploadKind::from_code(n[18])?;
            line.meshes_added = small(n[19])?;
        }
        Some(line)
    }
}

/// The prefix of a pipeline line in gpu-frames.txt.
const PIPELINE_PREFIX: &str = "pipeline ";

impl RecentPipeline {
    /// `pipeline <ms before the loss> <label>` (gpu-frames.txt, after the
    /// frame lines).
    pub fn encode(&self) -> String {
        format!("{PIPELINE_PREFIX}{} {}", self.before_loss_ms, clean_label(&self.label))
    }

    /// A line of [`Self::encode`], its label filtered again.
    pub fn decode(text: &str) -> Option<Self> {
        let rest = text.strip_prefix(PIPELINE_PREFIX)?;
        if rest.len() > 128 {
            return None;
        }
        let (ms, label) = rest.split_once(' ')?;
        Some(Self {
            before_loss_ms: ms.parse().ok()?,
            label: clean_label(label),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> FrameLine {
        FrameLine {
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
            pipelines: 3,
            texture_pages: 2,
            buffers_made: 1,
            largest_buffer_bytes: 256 << 20,
            largest_upload_bytes: 22 << 20,
            largest_upload_kind: UploadKind::Texture,
            meshes_added: 140,
        }
    }

    #[test]
    fn frame_lines_round_trip_and_refuse_text() {
        let line = sample();
        assert_eq!(FrameLine::decode(&line.encode()), Some(line));
        for kind in [UploadKind::None, UploadKind::Mesh, UploadKind::Write] {
            let line = FrameLine {
                largest_upload_kind: kind,
                ..sample()
            };
            assert_eq!(FrameLine::decode(&line.encode()), Some(line));
        }
        for bad in [
            "",
            "1 2 3",
            "a b c d e f g h i j k l m",
            "1 2 3 4 5 6 7 8 9 10 11 12 2",
            "1 2 3 4 5 6 7 8 9 10 11 12 1 14",
            // an unknown upload kind
            "1 2 3 4 5 6 7 8 9 10 11 12 1 0 0 0 0 0 4 0",
            "1 2 3 4 5 6 7 8 9 10 11 12 1 0 0 0 0 0 1 0 21",
            "pipeline 12 cull classify",
        ] {
            assert_eq!(FrameLine::decode(bad), None, "{bad}");
        }
    }

    /// The lines of build 3 (13 numbers) still read, without the events.
    #[test]
    fn an_earlier_builds_lines_still_read() {
        let old = "42847 1000 812 34603008 4100 12 2 9437184 1 3120 900 4200000 1";
        let line = FrameLine::decode(old).expect("an old line");
        assert_eq!(line.upload_copies, 812);
        assert!(line.gpu_cull);
        assert_eq!((line.pipelines, line.largest_upload_kind), (0, UploadKind::None));
    }

    #[test]
    fn each_line_takes_its_share_of_the_totals() {
        let at = |n: u64| Totals {
            rebuilds: n,
            pipelines: 2 * n,
            pages: n,
            buffers: 3 * n,
            meshes: 10 * n,
        };
        // the first line: what came before it was the start
        assert_eq!(at(5).since(None), Totals::default());
        let share = at(7).since(Some(at(5)));
        assert_eq!((share.rebuilds, share.pipelines, share.buffers, share.meshes), (2, 4, 6, 20));
        // a recovered device counts its rebuilds from 0 again
        let after = Totals { rebuilds: 1, ..at(7) };
        assert_eq!(after.since(Some(at(5))).rebuilds, 0);
    }

    #[test]
    fn recent_pipelines_are_those_of_the_last_seconds() {
        let now = Instant::now() + Duration::from_secs(60);
        let mut ring = PipelineRing::default();
        ring.push("old".into(), now - Duration::from_secs(10));
        for i in 0..PIPELINES_KEPT {
            ring.push(format!("p{i}"), now - Duration::from_millis(3000 - i as u64 * 10));
        }
        let recent = ring.recent(now);
        // the ring keeps the last ones; "old" was pushed out and too old
        assert_eq!(recent.len(), PIPELINES_KEPT);
        assert_eq!(recent[0].label, "p0");
        assert_eq!(recent[0].before_loss_ms, 3000);
        let mut ring = PipelineRing::default();
        ring.push("old".into(), now - Duration::from_secs(10));
        ring.push("cull classify".into(), now - Duration::from_millis(1500));
        assert_eq!(
            ring.recent(now),
            vec![RecentPipeline {
                before_loss_ms: 1500,
                label: "cull classify".into()
            }]
        );
    }

    #[test]
    fn pipeline_labels_are_filtered_and_round_trip() {
        assert_eq!(clean_label(""), "unnamed");
        assert_eq!(clean_label("contact shadows (photo)"), "contact shadows (photo)");
        assert_eq!(clean_label("C:\\Users\\x/y\nz"), "C:_Users_x_y_z");
        assert_eq!(clean_label(&"a".repeat(80)).len(), LABEL_LEN);
        let p = RecentPipeline {
            before_loss_ms: 812,
            label: "gtao main".into(),
        };
        assert_eq!(p.encode(), "pipeline 812 gtao main");
        assert_eq!(RecentPipeline::decode(&p.encode()), Some(p));
        assert_eq!(RecentPipeline::decode("pipeline x gtao"), None);
        assert_eq!(RecentPipeline::decode("1 2 3"), None);
        assert_eq!(
            RecentPipeline::decode("pipeline 5 a<b>/c").map(|p| p.label),
            Some("a_b__c".to_owned())
        );
    }
}
