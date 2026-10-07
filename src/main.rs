use alfred_workflow_rs::Item;
use grep_regex::{RegexMatcher, RegexMatcherBuilder};
use grep_searcher::sinks::UTF8;
use grep_searcher::Searcher;
use ignore::{
    DirEntry, Error as IgnoreError, ParallelVisitor, ParallelVisitorBuilder, WalkBuilder, WalkState,
};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::env;
use std::ffi::{CStr, CString};
use std::fs;
use std::io::{BufRead, BufReader};
use std::os::raw::c_char;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Instant, SystemTime};

fn url_encode(input: &str) -> String {
    let mut encoded = String::new();
    for byte in input.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                encoded.push(byte as char);
            }
            _ => {
                encoded.push_str(&format!("%{:02X}", byte));
            }
        }
    }
    encoded
}

#[derive(Serialize)]
struct AlfredOutput {
    #[serde(skip_serializing_if = "Option::is_none")]
    rerun: Option<f32>,
    items: Vec<Item>,
}

/// Maximum number of content (full-text) matches to keep from a single search.
const MAX_CONTENT_MATCHES: usize = 50;

/// Number of changed files above which indexing is handed to the background worker
/// instead of being applied inline while the user waits.
const DIRTY_FILE_THRESHOLD: usize = 10;

/// Number of changed images above which indexing is handed to the background worker.
/// Kept separate from the overall threshold because every image costs a Vision OCR
/// call, which is far slower than parsing a note.
const DIRTY_IMAGE_THRESHOLD: usize = 1;

/// A worker's state file older than this (in seconds) is treated as abandoned, since
/// a healthy worker rewrites it constantly while it indexes.
const STALE_STATE_SECS: u64 = 30;

/// Fallback cost of OCR-ing one image, used to estimate how long the remaining images
/// will take before any have actually been processed.
const DEFAULT_IMAGE_OCR_SECS: f64 = 0.25;

#[derive(Serialize, Deserialize, Clone)]
struct FileResult {
    title: String,
    path: String,
    modified: SystemTime,
    tags: Vec<String>,
    /// Basenames of every file this note embeds (`![[image.png]]` or `![](image.png)`),
    /// used to route OCR hits back to the note that shows the image.
    links: Vec<String>,
    /// Set only for notes found via content search; never persisted to the cache.
    #[serde(skip)]
    snippet: Option<String>,
}

/// Text recognized from an image, kept so the same image is never OCR'd twice
/// while its modification time stays the same.
#[derive(Serialize, Deserialize, Clone)]
struct OcrResult {
    modified: SystemTime,
    text: String,
}
type OcrCache = HashMap<String, OcrResult>;

/// A note whose body matched the query, plus the matching line to show the user.
#[derive(Clone)]
struct ContentMatch {
    path: String,
    snippet: String,
}

#[derive(Serialize, Deserialize)]
struct VaultCache {
    files: Vec<FileResult>,
    tag_recency: HashMap<String, SystemTime>,
}

#[derive(Serialize, Deserialize)]
struct State {
    progress: u32,
    total: u32,
    status: String,
    /// Estimated seconds remaining, shown beside the percentage. Absent until enough
    /// files have been processed to make a guess.
    #[serde(default)]
    eta_secs: Option<u64>,
}

extern "C" {
    fn perform_ocr(path: *const c_char) -> *mut c_char;
    fn free_ocr_string(ptr: *mut c_char);
}

/// Recognizes the text in an image through the native Vision framework.
///
/// Returns an empty string when the image cannot be read, so a failed recognition
/// is cached as "no text" rather than retried on every pass.
fn recognize_text(path: &str) -> String {
    if let Ok(c_path) = CString::new(path) {
        unsafe {
            let ptr = perform_ocr(c_path.as_ptr());
            if !ptr.is_null() {
                let text = CStr::from_ptr(ptr).to_string_lossy().into_owned();
                free_ocr_string(ptr);
                return text;
            }
        }
    }
    String::new()
}

/// Escapes regex metacharacters so a query can be matched as a literal string.
fn escape_regex(input: &str) -> String {
    let mut escaped = String::with_capacity(input.len());
    for ch in input.chars() {
        if "\\^$.|?*+()[]{}".contains(ch) {
            escaped.push('\\');
        }
        escaped.push(ch);
    }
    escaped
}

/// Trims a matched line down to something readable in an Alfred subtitle.
fn build_snippet(line: &str) -> String {
    const MAX_SNIPPET_CHARS: usize = 120;
    let trimmed = line.trim();
    let mut snippet: String = trimmed.chars().take(MAX_SNIPPET_CHARS).collect();
    if trimmed.chars().count() > MAX_SNIPPET_CHARS {
        snippet.push('…');
    }
    snippet
}

/// Builds one case-insensitive matcher per search term.
///
/// Each term is compiled as a regex first so patterns like `foo.*bar` work, but
/// most terms are plain words that may contain characters which are invalid regex
/// (e.g. `c++`), so those fall back to a literal, escaped search. A file only
/// counts as a content match when every term matches somewhere inside it.
fn build_term_matchers(terms: &[&str]) -> Vec<RegexMatcher> {
    terms
        .iter()
        .filter(|t| !t.trim().is_empty())
        .filter_map(|t| {
            let mut builder = RegexMatcherBuilder::new();
            builder.case_insensitive(true);
            builder.build(t).or_else(|_| builder.build(&escape_regex(t))).ok()
        })
        .collect()
}

/// Builds the per-thread visitors used by `WalkBuilder::build_parallel`.
struct ContentVisitorBuilder {
    matchers: Vec<RegexMatcher>,
    matches: Arc<Mutex<Vec<ContentMatch>>>,
}

impl<'s> ParallelVisitorBuilder<'s> for ContentVisitorBuilder {
    fn build(&mut self) -> Box<dyn ParallelVisitor + 's> {
        // `RegexMatcher` is cheap to clone and shares nothing mutable, so each
        // walking thread gets its own copy to avoid contention.
        Box::new(ContentVisitor {
            matchers: self.matchers.clone(),
            matches: Arc::clone(&self.matches),
            searcher: Searcher::new(),
        })
    }
}

/// Searches one file at a time, keeping the first matching line as the snippet.
struct ContentVisitor {
    matchers: Vec<RegexMatcher>,
    matches: Arc<Mutex<Vec<ContentMatch>>>,
    searcher: Searcher,
}

impl ParallelVisitor for ContentVisitor {
    fn visit(&mut self, entry: Result<DirEntry, IgnoreError>) -> WalkState {
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => return WalkState::Continue,
        };

        // Only regular files can be searched
        if !entry.file_type().map_or(false, |ft| ft.is_file()) {
            return WalkState::Continue;
        }

        // The cache only indexes markdown, so content search stays consistent with it
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("md") {
            return WalkState::Continue;
        }

        // Once a full page of hits is collected there is no point reading more files
        if let Ok(guard) = self.matches.lock() {
            if guard.len() >= MAX_CONTENT_MATCHES {
                return WalkState::Continue;
            }
        }

        // The file counts as a hit only when every term matches somewhere inside
        // it, so "foo bar" still matches a note whose words never share a line
        let mut snippet: Option<String> = None;
        let mut matched_every_term = true;

        for matcher in &self.matchers {
            let mut matched_line: Option<String> = None;
            let sink = UTF8(|_line_number: u64, line: &str| -> std::io::Result<bool> {
                let trimmed = line.trim();
                if !trimmed.is_empty() {
                    matched_line = Some(build_snippet(trimmed));
                }
                // Returning false stops the search after the first matching line
                Ok(false)
            });

            if self.searcher.search_path(matcher, path, sink).is_err() {
                matched_every_term = false;
                break;
            }

            match matched_line {
                // The first term's matching line is the one shown in Alfred
                Some(line) => {
                    if snippet.is_none() {
                        snippet = Some(line);
                    }
                }
                None => {
                    matched_every_term = false;
                    break;
                }
            }
        }

        if matched_every_term {
            if let Some(snippet) = snippet {
                if let Ok(mut guard) = self.matches.lock() {
                    if guard.len() < MAX_CONTENT_MATCHES {
                        guard.push(ContentMatch {
                            path: path.to_string_lossy().into_owned(),
                            snippet,
                        });
                    }
                }
            }
        }

        WalkState::Continue
    }
}

fn expand_tilde(path: &str) -> String {
    if path.starts_with("~/") {
        if let Ok(home) = env::var("HOME") {
            return path.replacen("~/", &format!("{}/", home), 1);
        }
    }
    path.to_string()
}

fn get_vault_map() -> HashMap<String, String> {
    let vault_map_env = env::var("vault_map").unwrap_or_else(|_| "".to_string());
    let mut vault_map = HashMap::new();
    for line in vault_map_env.lines() {
        if let Some((key, path)) = line.split_once(':') {
            let clean_key = key.trim().to_string();
            let clean_path = path.trim().to_string();
            if !clean_key.is_empty() && !clean_path.is_empty() {
                vault_map.insert(clean_key, clean_path);
            }
        }
    }
    vault_map
}

fn get_workflow_cache_dir() -> PathBuf {
    let dir = env::var("alfred_workflow_cache")
        .unwrap_or_else(|_| env::temp_dir().to_string_lossy().into_owned());
    let path = PathBuf::from(dir);
    fs::create_dir_all(&path).ok();
    path
}

/// True when the file is an image format the OCR CLI can read.
fn is_supported_image(path: &Path) -> bool {
    match path.extension().and_then(|e| e.to_str()) {
        Some(ext) => matches!(ext.to_lowercase().as_str(), "png" | "jpg" | "jpeg" | "webp"),
        None => false,
    }
}

/// What a single walk of the vault found, compared against the two caches.
struct VaultScan {
    /// Every markdown note on disk, with its modification time.
    notes: HashMap<String, SystemTime>,
    /// Every OCR-able image on disk, with its modification time.
    images: HashMap<String, SystemTime>,
    /// Notes that are new or whose modification time no longer matches the cache.
    dirty_notes: Vec<String>,
    /// Images that are new or whose modification time no longer matches the OCR cache.
    dirty_images: Vec<String>,
    /// True when a cached entry points at a file that is no longer on disk.
    has_deleted: bool,
}

/// Walks the vault once (skipping hidden dot-directories) and reports which notes and
/// images differ from what the caches already hold, so only those need re-reading.
fn scan_vault(dir: &Path, cached_files: &[FileResult], ocr_cache: &OcrCache) -> VaultScan {
    let mut scan = VaultScan {
        notes: HashMap::new(),
        images: HashMap::new(),
        dirty_notes: Vec::new(),
        dirty_images: Vec::new(),
        has_deleted: false,
    };

    collect_vault_files(dir, &mut scan.notes, &mut scan.images);

    // Borrowed paths and copied timestamps, so no FileResult, String or Vec is cloned
    // just to compare modification times
    let mut cached_mtimes: HashMap<&str, SystemTime> = HashMap::with_capacity(cached_files.len());
    for res in cached_files {
        cached_mtimes.insert(res.path.as_str(), res.modified);
    }

    for (path, modified) in &scan.notes {
        let is_dirty = match cached_mtimes.get(path.as_str()) {
            Some(cached) => *cached != *modified,
            None => true,
        };
        if is_dirty {
            scan.dirty_notes.push(path.clone());
        }
    }

    for (path, modified) in &scan.images {
        let is_dirty = match ocr_cache.get(path) {
            Some(cached) => cached.modified != *modified,
            None => true,
        };
        if is_dirty {
            scan.dirty_images.push(path.clone());
        }
    }

    // Anything the caches remember but the walk never saw has been deleted
    scan.has_deleted = cached_files.iter().any(|res| !scan.notes.contains_key(&res.path))
        || ocr_cache.keys().any(|path| !scan.images.contains_key(path));

    scan
}

/// Recursively records every markdown note and supported image below `dir`, skipping
/// hidden directories the way the rest of the workflow does.
fn collect_vault_files(
    dir: &Path,
    notes: &mut HashMap<String, SystemTime>,
    images: &mut HashMap<String, SystemTime>,
) {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(_) => return,
    };

    for entry in entries.flatten() {
        // file_type() comes from the directory entry itself, so it needs no extra stat
        // and never follows symlinks, which is what stops a symlinked directory from
        // walking the vault forever
        let file_type = match entry.file_type() {
            Ok(file_type) => file_type,
            Err(_) => continue,
        };

        if file_type.is_dir() {
            if let Some(name) = entry.file_name().to_str() {
                if !name.starts_with('.') {
                    collect_vault_files(&entry.path(), notes, images);
                }
            }
            continue;
        }

        // Anything that is not a plain file (socket, fifo, symlink) is skipped
        if !file_type.is_file() {
            continue;
        }

        let path = entry.path();
        let is_note = path
            .extension()
            .and_then(|ext| ext.to_str())
            .map(|ext| ext.eq_ignore_ascii_case("md"))
            .unwrap_or(false);

        if is_note {
            // entry.metadata() stats through the directory entry descriptor, which on
            // macOS avoids re-resolving the full path for every file in the vault
            let modified = entry
                .metadata()
                .and_then(|meta| meta.modified())
                .unwrap_or(SystemTime::UNIX_EPOCH);
            notes.insert(path.to_string_lossy().into_owned(), modified);
        } else if is_supported_image(&path) {
            let modified = entry
                .metadata()
                .and_then(|meta| meta.modified())
                .unwrap_or(SystemTime::UNIX_EPOCH);
            images.insert(path.to_string_lossy().into_owned(), modified);
        }
    }
}

fn clean_tag(raw_tag: &str) -> Option<String> {
    let t = raw_tag.trim().trim_start_matches('#');
    if t.is_empty() || t.contains(|c: char| !c.is_alphanumeric() && c != '_' && c != '-' && c != '/') {
        None
    } else {
        Some(t.to_lowercase())
    }
}

fn parse_inline_list(val: &str, tags: &mut HashSet<String>) {
    let clean_val = val.trim_matches(|c| c == '[' || c == ']' || c == ' ');
    for t in clean_val.split(',') {
        if let Some(clean) = clean_tag(t) {
            tags.insert(clean);
        }
    }
}

/// Parses a note's tags and the basenames of the files it embeds in one pass.
fn extract_tags_and_links(path: &Path) -> (Vec<String>, Vec<String>) {
    let mut tags = HashSet::new();
    let mut links = HashSet::new();
    
    if let Ok(file) = fs::File::open(path) {
        let reader = BufReader::new(file);
        
        let mut in_frontmatter = false;
        let mut line_count = 0;
        let mut inside_tags_block = false;

        for line_result in reader.lines() {
            let line = match line_result {
                Ok(l) => l,
                Err(_) => break,
            };
            
            line_count += 1;
            let trimmed = line.trim();

            if line_count == 1 && trimmed == "---" {
                in_frontmatter = true;
                continue;
            } else if in_frontmatter && trimmed == "---" {
                in_frontmatter = false;
                inside_tags_block = false;
                continue;
            }

            if in_frontmatter {
                if trimmed.starts_with("tags:") || trimmed.starts_with("tag:") {
                    let parts: Vec<&str> = trimmed.splitn(2, ':').collect();
                    if parts.len() == 2 {
                        let val = parts[1].trim();
                        if val.is_empty() {
                            inside_tags_block = true;
                        } else {
                            parse_inline_list(val, &mut tags);
                            inside_tags_block = false;
                        }
                    }
                } else if inside_tags_block && trimmed.starts_with('-') {
                    let val = trimmed.trim_start_matches('-').trim();
                    if let Some(clean) = clean_tag(val) {
                        tags.insert(clean);
                    }
                } else if !trimmed.is_empty() {
                    inside_tags_block = false;
                }
            } else {
                for word in line.split_whitespace() {
                    if word.starts_with('#') {
                        if let Some(clean) = clean_tag(word) {
                            tags.insert(clean);
                        }
                    }
                }
            }

            // Runs for every line regardless of tag handling: an embed can sit
            // anywhere in the note, including inside frontmatter.
            let mut ptr = line.as_str();
            while let Some(start) = ptr.find("[[") {
                ptr = &ptr[start + 2..];
                if let Some(end) = ptr.find("]]") {
                    let content = &ptr[..end];
                    let target = content.split('|').next().unwrap_or(content).trim();
                    if let Some(name) = Path::new(target).file_name().and_then(|n| n.to_str()) {
                        links.insert(name.to_string());
                    }
                    ptr = &ptr[end + 2..];
                }
            }

            let mut ptr = line.as_str();
            while let Some(start) = ptr.find("](") {
                ptr = &ptr[start + 2..];
                if let Some(end) = ptr.find(')') {
                    let target = &ptr[..end];
                    if let Some(name) = Path::new(target).file_name().and_then(|n| n.to_str()) {
                        links.insert(name.replace("%20", " "));
                    }
                    ptr = &ptr[end + 1..];
                }
            }
        }
    }

    (tags.into_iter().collect(), links.into_iter().collect())
}

/// Reads one note's tags and embeds and builds its cache entry.
fn parse_note(path: &str, modified: SystemTime) -> FileResult {
    let note_path = Path::new(path);
    let (tags, links) = extract_tags_and_links(note_path);
    FileResult {
        title: note_path
            .file_stem()
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .to_string(),
        path: path.to_string(),
        modified,
        tags,
        links,
        snippet: None,
    }
}

/// Recomputes the newest modification time of every tag from scratch, so tags that
/// disappeared from the vault are dropped instead of lingering in the cache forever.
fn build_tag_recency(files: &[FileResult]) -> HashMap<String, SystemTime> {
    let mut tag_recency: HashMap<String, SystemTime> = HashMap::new();
    for res in files {
        for tag in &res.tags {
            let entry = tag_recency.entry(tag.clone()).or_insert(SystemTime::UNIX_EPOCH);
            if res.modified > *entry {
                *entry = res.modified;
            }
        }
    }
    tag_recency
}

// Helper to format system time for the subtitle
fn format_time_ago(time: SystemTime) -> String {
    let now = SystemTime::now();
    if let Ok(duration) = now.duration_since(time) {
        let secs = duration.as_secs();
        if secs < 60 {
            "Just now".to_string()
        } else if secs < 3600 {
            format!("{}m ago", secs / 60)
        } else if secs < 86400 {
            format!("{}h ago", secs / 3600)
        } else {
            format!("{}d ago", secs / 86400)
        }
    } else {
        "Unknown".to_string()
    }
}

/// Reads and deserializes the cache file, removing it if it is unreadable or corrupt.
fn load_cache(cache_path: &Path) -> Option<VaultCache> {
    match fs::read_to_string(cache_path) {
        Ok(file_content) => match serde_json::from_str::<VaultCache>(&file_content) {
            Ok(parsed_data) => Some(parsed_data),
            Err(_) => {
                fs::remove_file(cache_path).ok();
                None
            }
        },
        Err(_) => {
            fs::remove_file(cache_path).ok();
            None
        }
    }
}

/// Formats a remaining-time estimate, showing only the units that are needed.
fn format_eta(secs: u64) -> String {
    let hours = secs / 3600;
    let minutes = (secs % 3600) / 60;
    let seconds = secs % 60;

    if hours > 0 {
        format!("{}h{:02}m{:02}s", hours, minutes, seconds)
    } else if minutes > 0 {
        format!("{:02}m{:02}s", minutes, seconds)
    } else {
        format!("{:02}s", seconds)
    }
}

/// Serializes `value` to JSON and writes it to `path` without ever exposing a partial
/// file: the bytes land in a sibling `.json.tmp` file and are then renamed over the
/// target, so Alfred's 0.2s rerun polls read either the old file or the complete new one.
fn write_json_atomic<T: Serialize>(path: &Path, value: &T) {
    let json = match serde_json::to_string(value) {
        Ok(json) => json,
        Err(_) => return,
    };

    // The pid keeps two processes (main and a worker) from sharing one temporary file
    let tmp_path = path.with_extension(format!("json.{}.tmp", std::process::id()));
    if fs::write(&tmp_path, json).is_ok() {
        // A failed rename leaves the previous file in place, which is the safe outcome
        if fs::rename(&tmp_path, path).is_err() {
            fs::remove_file(&tmp_path).ok();
        }
    }
}

/// Removes the worker's state file when the worker ends, whether it finished normally,
/// bailed out early, or panicked, so a dead worker can never pin the UI to a progress bar.
struct StateFileGuard(PathBuf);

impl Drop for StateFileGuard {
    fn drop(&mut self) {
        fs::remove_file(&self.0).ok();
    }
}

fn run_worker(target_key: &str) {
    // Resolved and guarded before anything else, so even the early return below for an
    // unknown vault key still removes any state file this run inherited
    let cache_dir = get_workflow_cache_dir();
    let clean_key = target_key.replace("#", "");
    let state_path = cache_dir.join(format!("state_{}.json", clean_key));
    let _guard = StateFileGuard(state_path.clone());

    let vault_map = get_vault_map();
    let target_path = match vault_map.get(target_key) {
        Some(p) => expand_tilde(p),
        None => return,
    };
    let vault_dir = Path::new(&target_path);

    let cache_path = cache_dir.join(format!("vault_cache_{}.json", clean_key));

    let ocr_cache_path = cache_dir.join(format!("ocr_cache_{}.json", clean_key));

    // main() already counted the dirty files before spawning this worker, so reuse that
    // total rather than flashing "0 of 0 files processed" until the scan finishes
    let existing_total: u32 = fs::read_to_string(&state_path)
        .ok()
        .and_then(|content| serde_json::from_str::<State>(&content).ok())
        .map(|state| state.total)
        .unwrap_or(0);

    let mut state = State { progress: 0, total: existing_total, status: "Scanning vault...".to_string(), eta_secs: None };
    // Written unconditionally so the file's mtime is refreshed the moment the worker
    // starts, keeping main()'s staleness check from deleting it mid-run
    write_json_atomic(&state_path, &state);

    // Load both caches first: only the files that differ from them need real work
    let cached_files: Vec<FileResult> = fs::read_to_string(&cache_path)
        .ok()
        .and_then(|content| serde_json::from_str::<VaultCache>(&content).ok())
        .map(|cache| cache.files)
        .unwrap_or_default();

    let mut ocr_cache: OcrCache = fs::read_to_string(&ocr_cache_path)
        .ok()
        .and_then(|content| serde_json::from_str::<OcrCache>(&content).ok())
        .unwrap_or_default();

    let scan = scan_vault(vault_dir, &cached_files, &ocr_cache);

    // Progress covers only the files that actually get read, not the whole vault
    state.total = (scan.dirty_notes.len() + scan.dirty_images.len()) as u32;
    state.progress = 0;
    state.status = "Indexing vault...".to_string();
    write_json_atomic(&state_path, &state);

    // The cached entries are only needed as a lookup now that the scan is done, so the
    // vector is consumed into the map rather than being cloned into it
    let mut old_cache: HashMap<String, FileResult> = HashMap::with_capacity(cached_files.len());
    for file in cached_files {
        old_cache.insert(file.path.clone(), file);
    }

    // Unchanged notes are carried over untouched, so nothing re-reads them
    let mut results: Vec<FileResult> = Vec::with_capacity(scan.notes.len());
    for (path, modified) in &scan.notes {
        if let Some(cached) = old_cache.get(path) {
            if cached.modified == *modified {
                results.push(cached.clone());
            }
        }
    }

    let total_notes = scan.dirty_notes.len();
    let total_images = scan.dirty_images.len();

    // Notes are parsed far faster than images are OCR'd, so each phase is measured
    // separately rather than sharing one average that neither phase fits
    let notes_start = Instant::now();
    let mut notes_done: usize = 0;

    for path in &scan.dirty_notes {
        let modified = scan.notes.get(path).copied().unwrap_or(SystemTime::UNIX_EPOCH);
        results.push(parse_note(path, modified));

        notes_done += 1;
        state.progress += 1;
        // Small batches are reported file by file; larger ones every 25 files
        if state.total < 100 || state.progress % 25 == 0 {
            // The images have not started yet, so they are costed at the default rate
            let remaining_notes = total_notes.saturating_sub(notes_done);
            let avg_note_secs = notes_start.elapsed().as_secs_f64() / notes_done as f64;
            let est_secs = (remaining_notes as f64 * avg_note_secs)
                + (total_images as f64 * DEFAULT_IMAGE_OCR_SECS);
            state.eta_secs = Some(est_secs.ceil() as u64);

            write_json_atomic(&state_path, &state);
        }
    }

    let images_start = Instant::now();
    let mut images_done: usize = 0;

    for path in &scan.dirty_images {
        let modified = scan.images.get(path).copied().unwrap_or(SystemTime::UNIX_EPOCH);
        let text = recognize_text(path);
        ocr_cache.insert(path.clone(), OcrResult { modified, text });

        images_done += 1;
        state.progress += 1;

        // With the notes finished, the only work left is the remaining images
        let remaining_images = total_images.saturating_sub(images_done);
        let avg_image_secs = images_start.elapsed().as_secs_f64() / images_done as f64;
        state.eta_secs = Some((remaining_images as f64 * avg_image_secs).ceil() as u64);

        // OCR is slow enough that every image is worth reporting
        write_json_atomic(&state_path, &state);

        // Checkpoint so an interrupted run keeps the OCR work it already paid for
        if images_done % 20 == 0 {
            write_json_atomic(&ocr_cache_path, &ocr_cache);
        }
    }

    // Entries whose files vanished from disk are dropped rather than kept forever
    ocr_cache.retain(|path, _| scan.images.contains_key(path));

    results.sort_by(|a, b| b.modified.cmp(&a.modified));

    write_json_atomic(&ocr_cache_path, &ocr_cache);

    // Rebuild tag recency from the new results so tags that no longer exist are cleared
    let tag_recency = build_tag_recency(&results);

    write_json_atomic(&cache_path, &VaultCache { files: results, tag_recency });
    // state_path is removed by the guard here, whichever way the worker ended
}

fn main() {
    let args: Vec<String> = env::args().collect();
    
    if args.len() >= 3 && args[1] == "worker" {
        run_worker(&args[2]);
        return;
    }

    let vault_map = get_vault_map();
    if !vault_map.contains_key("default") {
        let output = AlfredOutput {
            rerun: None,
            items: vec![Item::new("Missing 'default' Vault")
                .set_subtitle("Your configuration must include a 'default:' path.")
                .set_valid(false)],
        };
        println!("{}", serde_json::to_string(&output).unwrap());
        return;
    }

    // args[1] is the mode ("search" / "createsearch" / "create"), args[2] is the query
    let mode = args.get(1).map(|s| s.as_str()).unwrap_or("");
    let allow_create = mode == "createsearch" || mode == "create";
    let is_create_only = mode == "create";
    let raw_query = args.get(2).map(|s| s.as_str()).unwrap_or("");
    let query = raw_query.trim_start();
    let lower_query = query.to_lowercase();
    let all_terms: Vec<&str> = lower_query.split_whitespace().collect();

    let mut target_key = "default";
    for term in &all_terms {
        if term.starts_with('#') && vault_map.contains_key(*term) {
            target_key = term;
            break;
        }
    }

    let target_path = vault_map.get(target_key).unwrap();
    let expanded_path = expand_tilde(target_path);
    let vault_dir = Path::new(&expanded_path);

    if !vault_dir.exists() {
        let output = AlfredOutput {
            rerun: None,
            items: vec![Item::new(format!("⚠️ Vault Path Not Found: {}", target_key))
                .set_subtitle(format!("Could not find directory at {}", expanded_path))
                .set_valid(false)],
        };
        println!("{}", serde_json::to_string(&output).unwrap());
        return;
    }

    let cache_dir = get_workflow_cache_dir();
    let clean_key = target_key.replace("#", "");
    let state_path = cache_dir.join(format!("state_{}.json", clean_key));
    let cache_path = cache_dir.join(format!("vault_cache_{}.json", clean_key));
    let ocr_cache_path = cache_dir.join(format!("ocr_cache_{}.json", clean_key));

    // If an indexer is actively writing, ALWAYS show the progress UI and rerun. A worker
    // that was killed leaves its state file behind, which would pin the UI to a progress
    // bar forever, so a file no worker has touched for STALE_STATE_SECS is discarded.
    if state_path.exists() {
        let state_age_secs = fs::metadata(&state_path)
            .and_then(|meta| meta.modified())
            .ok()
            .and_then(|modified| modified.elapsed().ok())
            .map(|elapsed| elapsed.as_secs())
            .unwrap_or(0);

        if state_age_secs > STALE_STATE_SECS {
            fs::remove_file(&state_path).ok();
        } else {
            let data = fs::read_to_string(&state_path).unwrap_or_default();
            let state = serde_json::from_str::<State>(&data).unwrap_or(State { progress: 0, total: 0, status: "Indexing vault...".to_string(), eta_secs: None });

            let percentage = if state.total > 0 {
                (state.progress as f32 / state.total as f32) * 100.0
            } else {
                0.0
            };

            // With no total yet the status alone explains what is happening, which is
            // what the cold-start and pre-scan states look like
            let subtitle = if state.total > 0 {
                format!("{} ({} of {} files processed). Please wait...", state.status, state.progress, state.total)
            } else {
                format!("{} Please wait...", state.status)
            };

            // The ETA only appears once the worker has processed enough files to guess
            let title = match state.eta_secs {
                Some(eta) => format!("Indexing Vault: {:.0}% ({})", percentage, format_eta(eta)),
                None => format!("Indexing Vault: {:.0}%", percentage),
            };

            let output = AlfredOutput {
                rerun: Some(0.2),
                items: vec![
                    Item::new(title)
                        .set_subtitle(subtitle)
                        .set_valid(false)
                ]
            };
            println!("{}", serde_json::to_string(&output).unwrap());
            return;
        }
    }

    let mut cached_data_opt = None;
    if cache_path.exists() {
        cached_data_opt = load_cache(&cache_path);
    }

    // Cold start: no cache exists at all
    if cached_data_opt.is_none() {
        write_json_atomic(&state_path, &State { progress: 0, total: 0, status: "Starting Indexer...".to_string(), eta_secs: None });

        let spawned = match env::current_exe() {
            Ok(exe) => Command::new(exe)
                .arg("worker")
                .arg(target_key)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                // Own process group, so closing Alfred cannot signal the worker
                .process_group(0)
                .spawn()
                .is_ok(),
            Err(_) => false,
        };

        // A worker that never started would leave the UI pinned to a progress bar
        if !spawned {
            fs::remove_file(&state_path).ok();
        }

        let output = AlfredOutput {
            rerun: Some(0.2),
            items: vec![
                Item::new("Indexing Vault: 0%")
                    .set_subtitle("Initializing background worker. Please wait...")
                    .set_valid(false)
            ]
        };
        println!("{}", serde_json::to_string(&output).unwrap());
        return;
    }

    // Empty query: reconcile the caches with the vault so the list is never stale.
    // A typed query never pays for the directory walk, it just searches what is cached.
    if query.is_empty() {
        if let Some(mut cached_data) = cached_data_opt.take() {
            let mut ocr_cache: OcrCache = fs::read_to_string(&ocr_cache_path)
                .ok()
                .and_then(|content| serde_json::from_str::<OcrCache>(&content).ok())
                .unwrap_or_default();

            let scan = scan_vault(vault_dir, &cached_data.files, &ocr_cache);
            let dirty_count = scan.dirty_notes.len() + scan.dirty_images.len();

            if dirty_count > DIRTY_FILE_THRESHOLD || scan.dirty_images.len() > DIRTY_IMAGE_THRESHOLD {
                // Too much work to finish while the user waits: hand it to the worker. Images
                // get their own limit because each one costs a Vision OCR call.
                let state = State { progress: 0, total: dirty_count as u32, status: "Indexing vault...".to_string(), eta_secs: None };
                write_json_atomic(&state_path, &state);

                let spawned = match env::current_exe() {
                    Ok(exe) => Command::new(exe)
                        .arg("worker")
                        .arg(target_key)
                        .stdin(Stdio::null())
                        .stdout(Stdio::null())
                        .stderr(Stdio::null())
                        // Own process group, so closing Alfred cannot signal the worker
                        .process_group(0)
                        .spawn()
                        .is_ok(),
                    Err(_) => false,
                };

                // A worker that never started would leave the UI pinned to a progress bar
                if !spawned {
                    fs::remove_file(&state_path).ok();
                }

                let output = AlfredOutput {
                    rerun: Some(0.2),
                    items: vec![
                        Item::new("Indexing Vault: 0%")
                            .set_subtitle("Updating index in background. Please wait...")
                            .set_valid(false)
                    ]
                };
                println!("{}", serde_json::to_string(&output).unwrap());
                return;
            }

            if dirty_count > 0 || scan.has_deleted {
                // Small enough to fix inline: mutate the loaded caches in place so unchanged
                // entries are never cloned or rebuilt from scratch
                cached_data.files.retain(|res| scan.notes.contains_key(&res.path));
                ocr_cache.retain(|path, _| scan.images.contains_key(path));

                // Map surviving paths to their slot so a dirty note overwrites its own
                // entry instead of being appended a second time
                let mut slot_by_path: HashMap<String, usize> = HashMap::with_capacity(cached_data.files.len());
                for (index, res) in cached_data.files.iter().enumerate() {
                    slot_by_path.insert(res.path.clone(), index);
                }

                for path in &scan.dirty_notes {
                    let modified = scan.notes.get(path).copied().unwrap_or(SystemTime::UNIX_EPOCH);
                    let parsed = parse_note(path, modified);

                    match slot_by_path.get(path).copied() {
                        Some(index) => cached_data.files[index] = parsed,
                        None => {
                            slot_by_path.insert(path.clone(), cached_data.files.len());
                            cached_data.files.push(parsed);
                        }
                    }
                }

                cached_data.files.sort_by(|a, b| b.modified.cmp(&a.modified));

                for path in &scan.dirty_images {
                    let modified = scan.images.get(path).copied().unwrap_or(SystemTime::UNIX_EPOCH);
                    let text = recognize_text(path);
                    ocr_cache.insert(path.clone(), OcrResult { modified, text });
                }

                cached_data.tag_recency = build_tag_recency(&cached_data.files);

                write_json_atomic(&cache_path, &cached_data);
                // The OCR cache is only rewritten when images changed or entries were dropped
                if !scan.dirty_images.is_empty() || scan.has_deleted {
                    write_json_atomic(&ocr_cache_path, &ocr_cache);
                }

                cached_data_opt = Some(cached_data);
            } else {
                cached_data_opt = Some(cached_data);
            }
        }
    }

    let cached_data = cached_data_opt.unwrap();
    // The full cached list, already sorted by modified descending. It is never mutated, so
    // the fallback search below can resolve a hit in any note, not just filtered ones.
    let all_files = cached_data.files;
    let tag_recency = cached_data.tag_recency;
    let mut items = Vec::new();

    let ends_with_space = raw_query.ends_with(' ');
    let last_term = all_terms.last().copied().unwrap_or("");
    let is_autocompleting_tag = !ends_with_space && last_term.starts_with('#');

    // Tag Autocomplete Mode
    if is_autocompleting_tag {
        let partial_tag = last_term.trim_start_matches('#');
        
        let mut matched_tags: Vec<(&String, &SystemTime)> = tag_recency.iter()
            .filter(|(t, _)| t.contains(partial_tag))
            .collect();
            
        matched_tags.sort_by(|a, b| b.1.cmp(a.1));
        
        let prefix = if all_terms.len() > 1 {
            let terms_before = &all_terms[..all_terms.len() - 1];
            format!("{} ", terms_before.join(" "))
        } else {
            "".to_string()
        };

        for (tag, modified_time) in matched_tags.into_iter().take(30) {
            let time_ago = format_time_ago(*modified_time);
            items.push(
                Item::new(format!("#{}", tag))
                    .set_subtitle(format!("{}", time_ago))
                    .set_autocomplete(format!("{}#{} ", prefix, tag))
                    .set_valid(false)
            );
        }
        
        if items.is_empty() {
            items.push(Item::new("No matching tags").set_valid(false));
        }

        let output = AlfredOutput { rerun: None, items };
        println!("{}", serde_json::to_string(&output).unwrap());
        return;
    }

    // Normal Search Mode
    let title_terms: Vec<&str> = all_terms.iter()
        .filter(|t| !t.starts_with('#'))
        .copied()
        .collect();
        
    let tag_terms: Vec<&str> = all_terms.iter()
        .filter(|t| t.starts_with('#') && !vault_map.contains_key(**t))
        .map(|t| t.trim_start_matches('#'))
        .collect();

    let is_empty_search = title_terms.is_empty() && tag_terms.is_empty();

    // Resolve the new note's title and check it against the full, un-filtered vault.
    // Built from `raw_query` rather than `title_terms`, because the latter are lowercased
    // for search purposes and would destroy the user's original capitalization.
    // Tag terms are skipped since they define tags, not the note title.
    let title_string: String = raw_query
        .split_whitespace()
        .filter(|term| !term.starts_with('#'))
        .collect::<Vec<&str>>()
        .join(" ");

    // Trim whitespace around any slashes so "folder / note" becomes "folder/note"
    let title_string = title_string
        .split('/')
        .map(|s| s.trim())
        .collect::<Vec<&str>>()
        .join("/");

    let title_string = if title_string.is_empty() {
        "Untitled".to_string()
    } else {
        title_string
    };

    // Checked against the whole vault, and only when a create item will actually be shown
    let is_duplicate = allow_create
        && !is_empty_search
        && all_files
            .iter()
            .any(|res| res.title.eq_ignore_ascii_case(&title_string));

    // Built from `all_files` rather than by narrowing it, so the list below can still be
    // extended with content and OCR hits for notes the filter dropped
    let mut results: Vec<FileResult> = if is_create_only {
        Vec::new()
    } else if is_empty_search {
        // Already sorted by modified descending, so the newest notes are the first 50
        all_files.iter().take(50).cloned().collect()
    } else {
        all_files
            .iter()
            .filter(|res| {
                let lower_title = res.title.to_lowercase();
                let matches_title = title_terms.iter().all(|term| lower_title.contains(*term));
                let matches_tags = tag_terms.iter().all(|term| res.tags.iter().any(|t| t == *term));

                matches_title && matches_tags
            })
            .cloned()
            .collect()
    };

    // Full-text fallback: when title/tag matches are sparse, grep the note bodies so
    // notes that merely mention the query still surface. Every term has to appear
    // somewhere in the file; terms under two characters are too noisy to search for.
    if !is_create_only && results.len() < 50 && !title_terms.is_empty() && title_terms.iter().all(|t| t.len() >= 2) {
        // Keyed by borrowed path and holding a borrowed file, so building the lookup costs
        // no clones at all. It spans the whole vault, which is what lets a content or OCR
        // hit resolve for a note the title/tag filter dropped.
        let file_lookup: HashMap<&str, &FileResult> = all_files
            .iter()
            .map(|res| (res.path.as_str(), res))
            .collect();

        // Read the recognized text only here: no other code path needs it, and it is
        // the largest file in the cache directory
        let ocr_cache: OcrCache = fs::read_to_string(&ocr_cache_path)
            .ok()
            .and_then(|content| serde_json::from_str::<OcrCache>(&content).ok())
            .unwrap_or_default();

        let matchers = build_term_matchers(&title_terms);

        // Every term must have compiled, otherwise the search would silently
        // ignore part of what the user typed
        if matchers.len() == title_terms.len() {
            let content_matches: Arc<Mutex<Vec<ContentMatch>>> = Arc::new(Mutex::new(Vec::new()));
            let mut visitor_builder = ContentVisitorBuilder {
                matchers,
                matches: Arc::clone(&content_matches),
            };

            WalkBuilder::new(vault_dir)
                .build_parallel()
                .visit(&mut visitor_builder);

            let found: Vec<ContentMatch> = match content_matches.lock() {
                Ok(mut guard) => std::mem::take(&mut *guard),
                Err(poisoned) => std::mem::take(&mut *poisoned.into_inner()),
            };

            for content_match in found {
                // Never list a note twice: title/tag hits keep their original entry
                if results.iter().any(|res| res.path == content_match.path) {
                    continue;
                }

                let cached = file_lookup.get(content_match.path.as_str()).copied();

                // A content hit still has to satisfy the tag filter the user typed. A file
                // with no cache entry has no known tags, so a tag filter excludes it.
                if !tag_terms.is_empty() {
                    let matches_tags = cached
                        .map(|cached_file| {
                            tag_terms
                                .iter()
                                .all(|term| cached_file.tags.iter().any(|t| t == *term))
                        })
                        .unwrap_or(false);

                    if !matches_tags {
                        continue;
                    }
                }

                let mut file_result = match cached {
                    Some(cached) => cached.clone(),
                    // A file too new to be in the cache still deserves a row
                    None => {
                        let path = Path::new(&content_match.path);
                        FileResult {
                            title: path
                                .file_stem()
                                .and_then(|n| n.to_str())
                                .unwrap_or(&content_match.path)
                                .to_string(),
                            path: content_match.path.clone(),
                            modified: fs::metadata(path)
                                .and_then(|m| m.modified())
                                .unwrap_or(SystemTime::UNIX_EPOCH),
                            tags: Vec::new(),
                            links: Vec::new(),
                            snippet: None,
                        }
                    }
                };

                file_result.snippet = Some(content_match.snippet);
                results.push(file_result);
            }
        }

        // An image is never shown on its own: only the Markdown note that embeds it
        // becomes an item, so Alfred always opens something editable.
        for (image_path, ocr_result) in &ocr_cache {
            let lower_text = ocr_result.text.to_lowercase();
            if !title_terms.iter().all(|term| lower_text.contains(*term)) {
                continue;
            }

            let image_name = Path::new(image_path)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or(image_path);

            // Find the first line that actually contains one of the query terms to show as the snippet
            let matching_line = ocr_result
                .text
                .lines()
                .map(|l| l.trim())
                .find(|l| {
                    let lower_line = l.to_lowercase();
                    title_terms.iter().any(|term| lower_line.contains(term))
                })
                .unwrap_or("");

            for &cached_file in file_lookup.values() {
                if !cached_file.links.iter().any(|l| l.eq_ignore_ascii_case(image_name)) {
                    continue;
                }

                // An OCR hit still has to satisfy the tag filter the user typed
                if !tag_terms.is_empty()
                    && !tag_terms
                        .iter()
                        .all(|term| cached_file.tags.iter().any(|t| t == *term))
                {
                    continue;
                }

                if results.iter().any(|res| res.path == cached_file.path) {
                    continue;
                }

                let mut file_result = cached_file.clone();
                file_result.snippet = Some(format!("🖼️ {}", build_snippet(matching_line)));
                results.push(file_result);
            }
        }
    }

    // Rank matches so exact title hits beat loose term hits, which in turn beat
    // content hits. Ties fall back to the most recently modified note.
    let exact_query = title_terms.join(" ").to_lowercase();

    let rank_result = |res: &FileResult| -> u8 {
        let lower_title = res.title.to_lowercase();
        let title_has_exact =
            !exact_query.is_empty() && (lower_title == exact_query || lower_title.contains(&exact_query));
        let title_has_all_terms = !title_terms.is_empty()
            && title_terms
                .iter()
                .all(|term| lower_title.contains(*term));

        match (res.snippet.is_some(), title_has_exact, title_has_all_terms) {
            // Tier 1: the title contains the whole query
            (false, true, _) => 0,
            // Tier 2: the title contains every term separately
            (false, _, true) => 1,
            // Tier 3 / 4: a content hit, ranked by whether the matched line holds
            // the whole query or just one of the terms
            (true, _, _) => {
                let snippet = res.snippet.as_ref().unwrap().to_lowercase();
                if !exact_query.is_empty() && snippet.contains(&exact_query) {
                    2
                } else {
                    3
                }
            }
            _ => 4,
        }
    };

    // Empty searches and create-only mode already hold the newest 50 notes in order and
    // have no query to rank against, so the sort is skipped entirely for them
    if !is_empty_search && !is_create_only {
        results.sort_by(|a, b| {
            rank_result(a)
                .cmp(&rank_result(b))
                .then_with(|| b.modified.cmp(&a.modified))
        });

        results.truncate(50);
    }

    // The vault folder name is used as the root label in result subtitles
    let vault_name = vault_dir
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(&clean_key)
        .to_string();

    // The vault name only adds useful context when more than one vault is configured
    let has_multiple_vaults = vault_map.len() > 1;

    if !is_create_only {
        for res in results.iter().take(50) {
            // Show the note's location relative to the vault, without the filename
            let note_path = Path::new(&res.path);
            let location = match note_path.strip_prefix(vault_dir) {
                Ok(relative) => match relative.parent() {
                    Some(parent) if !parent.as_os_str().is_empty() => {
                        if has_multiple_vaults {
                            format!("{}/{}", vault_name, parent.to_string_lossy())
                        } else {
                            parent.to_string_lossy().into_owned()
                        }
                    }
                    _ => {
                        if has_multiple_vaults {
                            vault_name.clone()
                        } else {
                            String::new()
                        }
                    }
                },
                Err(_) => {
                    if has_multiple_vaults {
                        vault_name.clone()
                    } else {
                        String::new()
                    }
                }
            };

            // When the title itself explains the match, the note's tags are more
            // useful than the matched line; otherwise the snippet explains the hit
            let lower_title = res.title.to_lowercase();
            let title_matches_all_terms =
                !title_terms.is_empty() && title_terms.iter().all(|term| lower_title.contains(term));

            let subtitle = if !title_matches_all_terms && res.snippet.is_some() {
                let snippet = res.snippet.as_ref().unwrap();
                if location.is_empty() {
                    snippet.clone()
                } else {
                    format!("{} | {}", location, snippet)
                }
            } else if res.tags.is_empty() {
                location
            } else if location.is_empty() {
                format!("#{}", res.tags.join(" #"))
            } else {
                format!("{} | #{}", location, res.tags.join(" #"))
            };

            // Obsidian Advanced URI expects the note path relative to the vault root, without the .md extension
            let arg_path = note_path
                .strip_prefix(vault_dir)
                .unwrap_or(note_path)
                .to_string_lossy()
                .trim_end_matches(".md")
                .to_string();

            let item = Item::new(res.title.clone())
                .set_subtitle(subtitle)
                .set_arg(format!(
                    "obsidian://advanced-uri?vault={}&filepath={}|{}",
                    url_encode(&vault_name),
                    url_encode(&arg_path),
                    res.title
                ))
                .set_valid(true);
        
            items.push(item);
        }
    }

    // Offer to open the existing note, or create a new one with the tags written into the body
    if allow_create && !is_empty_search {
        let tag_string = tag_terms
            .iter()
            .map(|t| format!("#{}", t))
            .collect::<Vec<String>>()
            .join(" ");

        let create_item = if is_duplicate {
            // Open the existing note as-is; omit mode=new and data so tags are never appended
            let open_uri = format!(
                "obsidian://advanced-uri?vault={}&filepath={}|{}",
                url_encode(&vault_name),
                url_encode(&title_string),
                title_string
            );

            Item::new(format!("Open existing \"{}\"", title_string))
                .set_subtitle("Note already exists")
                .set_arg(open_uri)
                .set_valid(true)
        } else {
            // A slash in the typed title means the note lands in a subdirectory of the vault.
            // The title shows only the note name; the directory is shown in the subtitle.
            let new_note_path = Path::new(&title_string);
            let file_name_str = new_note_path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or(&title_string)
                .to_string();
            let parent_str = match new_note_path.parent() {
                Some(p) if !p.as_os_str().is_empty() => p.to_string_lossy().into_owned(),
                _ => String::new(),
            };

            let create_location = if !parent_str.is_empty() {
                format!("{}/{}", vault_name, parent_str)
            } else if has_multiple_vaults {
                vault_name.clone()
            } else {
                String::new()
            };

            let create_subtitle = if tag_string.is_empty() {
                create_location
            } else if create_location.is_empty() {
                tag_string.clone()
            } else {
                format!("{} | {}", create_location, tag_string)
            };

            let body_string = if tag_string.is_empty() {
                String::new()
            } else {
                format!("\n\n{}", tag_string)
            };

            let create_uri = format!(
                "obsidian://advanced-uri?vault={}&filepath={}&mode=new&data={}|{}",
                url_encode(&vault_name),
                url_encode(&title_string),
                url_encode(&body_string),
                title_string
            );

            Item::new(format!("Create \"{}\"", file_name_str))
                .set_subtitle(create_subtitle)
                .set_arg(create_uri)
                .set_valid(true)
        };

        if !is_create_only && items.len() >= 2 {
            items.insert(2, create_item);
        } else {
            items.push(create_item);
        }
    }

    if items.is_empty() {
        if is_create_only && is_empty_search {
            items.push(
                Item::new("Create a new note")
                    .set_subtitle("Enter a title or #tags")
                    .set_valid(false)
            );
        } else if is_empty_search {
            items.push(
                Item::new("No matches found")
                    .set_subtitle(format!("No markdown files found in {} vault", target_key))
                    .set_valid(false)
            );
        } else {
            let mut all_search_terms = title_terms;
            all_search_terms.extend(tag_terms.iter().map(|t| *t));
            items.push(
                Item::new("No matches found")
                    .set_subtitle(format!("Searched in {} vault for '{}'", target_key, all_search_terms.join(" ")))
                    .set_valid(false)
            );
        }
    }

    let output = AlfredOutput { rerun: None, items };
    println!("{}", serde_json::to_string(&output).unwrap());
}