use alfred_workflow_rs::Item;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::time::SystemTime;

#[derive(Serialize)]
pub struct AlfredOutput {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rerun: Option<f32>,
    pub items: Vec<Item>,
}

/// Maximum number of content (full-text) matches to keep from a single search.
pub const MAX_CONTENT_MATCHES: usize = 50;

/// Number of changed files above which indexing is handed to the background worker
/// instead of being applied inline while the user waits.
pub const DIRTY_FILE_THRESHOLD: usize = 10;

/// Number of changed attachments above which indexing is handed to the background worker.
/// Kept separate from the overall threshold because every image (or scanned PDF page)
/// costs a Vision pass, which is far slower than parsing a note.
pub const DIRTY_IMAGE_THRESHOLD: usize = 1;

/// A worker's state file older than this (in seconds) is treated as abandoned, since
/// a healthy worker rewrites it constantly while it indexes.
/// Fallback age (in seconds) at which a state file is treated as abandoned. Only used
/// when the file carries no usable worker pid; normally liveness is checked directly.
pub const STALE_STATE_SECS: u64 = 600;

/// Fallback cost of extracting the text from one attachment on a single core, used to
/// estimate how long the remaining attachments will take before enough real timings
/// exist. A PDF with an embedded text layer is far cheaper than this, so the estimate
/// errs high.
pub const DEFAULT_IMAGE_OCR_SECS: f64 = 1.0;

/// Upper bound on concurrent OCR shard processes. Vision contends for the Neural Engine
/// and GPU, so throughput stops scaling well before the core count on larger chips.
pub const MAX_OCR_WORKERS: usize = 8;

/// A shard that has not answered for this long is assumed hung on the current file and
/// is killed; the file is recorded as having no text and a fresh shard takes over.
pub const OCR_FILE_TIMEOUT_SECS: u64 = 60;

/// How many consecutive failures to start a shard process are tolerated before a pool
/// thread gives up on isolation and extracts text in-process instead.
pub const MAX_SHARD_SPAWN_FAILURES: u32 = 3;

/// The attachment cache is rewritten to disk after this many new results, or after
/// `CHECKPOINT_SECS` have elapsed since the last write, whichever comes first.
/// Writing it per file made total I/O quadratic in the number of attachments.
pub const CHECKPOINT_FILES: usize = 50;
pub const CHECKPOINT_SECS: f64 = 5.0;

/// Minimum spacing between rewrites of the progress state file. Alfred polls it every
/// 0.2s, so writing faster than that is wasted work.
pub const STATE_WRITE_INTERVAL_SECS: f64 = 0.25;

/// Width of the sliding window used to measure indexing throughput for the ETA. A
/// window reacts to a run of slow scanned PDFs, where a lifetime average would not.
pub const THROUGHPUT_WINDOW_SECS: f64 = 30.0;

/// Number of completed files needed inside the window before its throughput is trusted
/// over the default per-file estimate.
pub const MIN_WINDOW_SAMPLES: usize = 8;

/// Fixed progress status strings. The status never names individual files: the UI only
/// shows a phase, a percentage, a file count and the time left.
pub const STATUS_SCANNING: &str = "Scanning vault";
pub const STATUS_NOTES: &str = "Indexing notes";
pub const STATUS_ATTACHMENTS: &str = "Indexing attachments";

#[derive(Serialize, Deserialize, Clone)]
pub struct FileResult {
    pub title: String,
    pub path: String,
    pub modified: SystemTime,
    pub tags: Vec<String>,
    /// Basenames of every file this note embeds (`![[image.png]]`, `![](image.png)` or
    /// `![[paper.pdf]]`), used to route attachment text hits back to the note that
    /// displays the attachment. Aliases and fragments are stripped, so
    /// `[[paper.pdf#page=3|See p3]]` is stored as `paper.pdf`.
    pub links: Vec<String>,
    /// Set only for notes found via content search; never persisted to the cache.
    #[serde(skip)]
    pub snippet: Option<String>,
}

/// Text extracted from an attachment, kept so the same file is never read twice while
/// its modification time stays the same. For images this is Vision output; for PDFs it
/// is the embedded text layer, or Vision output for pages that turn out to be scans.
#[derive(Serialize, Deserialize, Clone)]
pub struct OcrResult {
    pub modified: SystemTime,
    pub text: String,
}
pub type OcrCache = HashMap<String, OcrResult>;

/// A note whose body matched the query, plus the matching line to show the user.
#[derive(Clone)]
pub struct ContentMatch {
    pub path: String,
    pub snippet: String,
}

/// Bumped whenever the note parser changes what it extracts. A cache written by an
/// older parser is discarded on load, so every note is re-read instead of keeping
/// stale tags until its modification time happens to change.
pub const PARSER_VERSION: u32 = 2;

#[derive(Serialize, Deserialize)]
pub struct VaultCache {
    pub files: Vec<FileResult>,
    pub tag_recency: HashMap<String, SystemTime>,
    #[serde(default)]
    pub parser_version: u32,
}

/// Borrowed view of `VaultCache` with the same JSON shape, so the worker can write a
/// checkpoint without cloning every note entry first.
#[derive(Serialize)]
pub struct VaultCacheRef<'a> {
    pub files: &'a [FileResult],
    pub tag_recency: &'a HashMap<String, SystemTime>,
    pub parser_version: u32,
}

#[derive(Serialize, Deserialize)]
pub struct State {
    pub progress: u32,
    pub total: u32,
    pub status: String,
    /// Estimated seconds remaining, shown beside the percentage. Absent until enough
    /// files have been processed to make a guess.
    #[serde(default)]
    pub eta_secs: Option<u64>,
    /// Pid of the worker that owns this file, so main() can ask the OS whether the
    /// worker is still alive instead of guessing from the file's age.
    #[serde(default)]
    pub worker_pid: Option<u32>,
    /// Vaults being indexed by the worker that owns this file. A single worker indexes
    /// every vault that needs it as one batch with one combined total, and writes this
    /// same state to each of their state files, so the progress bar never finishes for
    /// one vault only to start over for the next.
    #[serde(default)]
    pub vaults: Vec<String>,
}

/// What a single walk of the vault found, compared against the two caches.
pub struct VaultScan {
    /// Every markdown note on disk, with its modification time.
    pub notes: HashMap<String, SystemTime>,
    /// Every image or PDF attachment on disk, with its modification time.
    pub images: HashMap<String, SystemTime>,
    /// Notes that are new or whose modification time no longer matches the cache.
    pub dirty_notes: Vec<String>,
    /// Attachments that are new or whose modification time no longer matches the cache.
    pub dirty_images: Vec<String>,
    /// True when a cached entry points at a file that is no longer on disk.
    pub has_deleted: bool,
}
