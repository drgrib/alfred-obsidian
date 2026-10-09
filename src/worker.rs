use alfred_workflow_rs::Item;
use std::collections::{HashMap, VecDeque};
use std::env;
use std::fs;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use crate::alfred::format_eta;
use crate::cache::*;
use crate::ffi::kill;
use crate::ocr_pool::{ocr_worker_count, run_ocr_pool};
use crate::types::*;

/// Removes the worker's state files when the worker ends, whether it finished normally,
/// bailed out early, or panicked, so a dead worker can never pin the UI to a progress bar.
struct StateFileGuard(Vec<PathBuf>);

impl Drop for StateFileGuard {
    fn drop(&mut self) {
        for path in &self.0 {
            fs::remove_file(path).ok();
        }
    }
}

/// Measures throughput over a sliding window and turns it into a time-left estimate.
///
/// A lifetime average would be dragged around by whichever phase came first; a window
/// tracks what the pool is doing right now, so a stretch of scanned PDFs slows the
/// countdown instead of being hidden behind thousands of instant text-layer PDFs.
struct Throughput {
    completions: VecDeque<Instant>,
    window: Duration,
    /// Files per second to assume until the window holds enough samples to trust.
    fallback_rate: f64,
}

impl Throughput {
    fn new(fallback_rate: f64) -> Throughput {
        Throughput {
            completions: VecDeque::new(),
            window: Duration::from_secs_f64(THROUGHPUT_WINDOW_SECS),
            fallback_rate: fallback_rate.max(f64::EPSILON),
        }
    }

    fn record(&mut self, count: usize) {
        let now = Instant::now();
        for _ in 0..count {
            self.completions.push_back(now);
        }
        self.trim(now);
    }

    fn trim(&mut self, now: Instant) {
        while let Some(oldest) = self.completions.front() {
            if now.duration_since(*oldest) > self.window {
                self.completions.pop_front();
            } else {
                break;
            }
        }
    }

    fn eta_secs(&mut self, remaining: usize) -> u64 {
        let now = Instant::now();
        self.trim(now);

        let rate = match self.completions.front() {
            Some(oldest) if self.completions.len() >= MIN_WINDOW_SAMPLES => {
                // Span is measured from the oldest sample, so a window that is not yet
                // full does not understate the rate
                let span = now.duration_since(*oldest).as_secs_f64().max(0.001);
                self.completions.len() as f64 / span
            }
            _ => self.fallback_rate,
        };

        (remaining as f64 / rate).ceil() as u64
    }
}

/// Writes the state file at most every `STATE_WRITE_INTERVAL_SECS`, or immediately when
/// `force` is set for phase changes. The same state goes to every path, so a query that
/// routes to any vault in the batch sees the same combined progress.
struct StateWriter {
    paths: Vec<PathBuf>,
    last_write: Option<Instant>,
}

impl StateWriter {
    fn write(&mut self, state: &State, force: bool) {
        let due = match self.last_write {
            Some(last) => last.elapsed().as_secs_f64() >= STATE_WRITE_INTERVAL_SECS,
            None => true,
        };
        if force || due {
            for path in &self.paths {
                write_json_atomic(path, state);
            }
            self.last_write = Some(Instant::now());
        }
    }
}

/// Parses every dirty note across all cores and returns the results in no particular
/// order. `on_progress` is called on the current thread with the running count so the
/// state file can keep up while the threads work.
fn parse_notes_parallel<F>(dirty_notes: &[String], notes: &HashMap<String, SystemTime>, workers: usize, mut on_progress: F) -> Vec<FileResult>
where
    F: FnMut(usize),
{
    if dirty_notes.is_empty() {
        return Vec::new();
    }

    let workers = workers.clamp(1, dirty_notes.len());
    let chunk_size = dirty_notes.len().div_ceil(workers);
    let done = AtomicUsize::new(0);
    let (tx, rx) = mpsc::channel::<Vec<FileResult>>();

    thread::scope(|scope| {
        for chunk in dirty_notes.chunks(chunk_size) {
            let tx = tx.clone();
            let done = &done;
            scope.spawn(move || {
                let mut parsed = Vec::with_capacity(chunk.len());
                for path in chunk {
                    let modified = notes.get(path).copied().unwrap_or(SystemTime::UNIX_EPOCH);
                    parsed.push(parse_note(path, modified));
                    done.fetch_add(1, Ordering::Relaxed);
                }
                tx.send(parsed).ok();
            });
        }
        drop(tx);

        // Poll the counter instead of waiting on the channel: progress keeps moving
        // while the chunks are still in flight, and the receive ends the loop
        let mut results = Vec::with_capacity(dirty_notes.len());
        loop {
            match rx.recv_timeout(Duration::from_secs_f64(STATE_WRITE_INTERVAL_SECS)) {
                Ok(parsed) => results.extend(parsed),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
            on_progress(done.load(Ordering::Relaxed));
        }
        on_progress(done.load(Ordering::Relaxed));
        results
    })
}

/// Path of the progress state file for one vault key.
pub fn state_path_for(cache_dir: &Path, target_key: &str) -> PathBuf {
    cache_dir.join(format!("state_{}.json", target_key.replace("#", "")))
}

/// Human label for the vault or vaults a progress bar covers.
pub fn batch_label(vaults: &[String]) -> String {
    match vaults {
        [] => "vault".to_string(),
        [one] => format!("{} vault", one),
        many => format!("{} vaults", many.len()),
    }
}

/// Phase text for the state file. With more than one vault in the batch the current
/// vault is named, so the user can see the bar moving from one to the next.
fn phase_label(phase: &str, clean_key: &str, multi: bool) -> String {
    if multi {
        format!("{} in {}", phase, clean_key)
    } else {
        phase.to_string()
    }
}

/// One vault's resolved paths and the result of scanning it.
struct VaultJob {
    clean_key: String,
    cache_path: PathBuf,
    ocr_cache_path: PathBuf,
    cached_files: Vec<FileResult>,
    ocr_cache: OcrCache,
    scan: VaultScan,
}

/// Indexes every vault in `target_keys` as one batch.
///
/// All vaults are scanned before any is indexed, so the total and the ETA cover the
/// whole batch from the first frame. The same combined state is written to each vault's
/// state file, so whether the user is looking at the empty query or at one vault's
/// results, they see one countdown that ends when all the work is done.
pub fn run_worker(target_keys: &[String]) {
    // Resolved and guarded before anything else, so even an unknown vault key still has
    // the state file this run inherited removed
    let cache_dir = get_workflow_cache_dir();
    let state_paths: Vec<PathBuf> = target_keys.iter().map(|key| state_path_for(&cache_dir, key)).collect();
    let _guard = StateFileGuard(state_paths.clone());

    let vault_map = get_vault_map();

    // main() already counted the dirty files before spawning this worker, so reuse that
    // total rather than flashing "0 of 0 files processed" until the scans finish
    let existing_total: u32 = state_paths
        .iter()
        .filter_map(|path| fs::read_to_string(path).ok())
        .filter_map(|content| serde_json::from_str::<State>(&content).ok())
        .map(|state| state.total)
        .next()
        .unwrap_or(0);

    let clean_keys: Vec<String> = target_keys.iter().map(|key| key.replace("#", "")).collect();
    let multi = clean_keys.len() > 1;

    let mut state = State {
        progress: 0,
        total: existing_total,
        status: STATUS_SCANNING.to_string(),
        eta_secs: None,
        // main() probes this pid to tell a slow worker apart from a dead one
        worker_pid: Some(std::process::id()),
        vaults: clean_keys.clone(),
    };
    let mut state_writer = StateWriter { paths: state_paths, last_write: None };
    // Written unconditionally so the files' mtimes are refreshed the moment the worker
    // starts, keeping main()'s staleness check from deleting them mid-run
    state_writer.write(&state, true);

    // Scan every vault first: only the files that differ from the caches need real
    // work, and the batch total has to be known before the first file is read
    let mut jobs: Vec<VaultJob> = Vec::with_capacity(target_keys.len());
    for (key, clean_key) in target_keys.iter().zip(clean_keys.iter()) {
        let vault_dir = match vault_map.get(key) {
            Some(path) => PathBuf::from(expand_tilde(path)),
            None => continue,
        };
        let cache_path = cache_dir.join(format!("vault_cache_{}.json", clean_key));
        let ocr_cache_path = cache_dir.join(format!("ocr_cache_{}.json", clean_key));

        let cached_files: Vec<FileResult> = fs::read_to_string(&cache_path)
            .ok()
            .and_then(|content| serde_json::from_str::<VaultCache>(&content).ok())
            .map(|cache| cache.files)
            .unwrap_or_default();

        let ocr_cache: OcrCache = fs::read_to_string(&ocr_cache_path)
            .ok()
            .and_then(|content| serde_json::from_str::<OcrCache>(&content).ok())
            .unwrap_or_default();

        let scan = scan_vault(&vault_dir, &cached_files, &ocr_cache);

        jobs.push(VaultJob {
            clean_key: clean_key.clone(),
            cache_path,
            ocr_cache_path,
            cached_files,
            ocr_cache,
            scan,
        });
    }

    let total_notes: usize = jobs.iter().map(|job| job.scan.dirty_notes.len()).sum();
    let total_images: usize = jobs.iter().map(|job| job.scan.dirty_images.len()).sum();
    let workers = ocr_worker_count();

    // Progress covers only the files that actually get read, across every vault
    state.total = (total_notes + total_images) as u32;
    state.progress = 0;
    state_writer.write(&state, true);

    // Notes are parsed far faster than attachments are read, so each phase gets its own
    // throughput window rather than sharing one average that neither phase fits. Both
    // windows persist across vaults, so the second vault's estimate starts from what
    // the first one actually measured. Until a window fills, attachments are costed at
    // the default single-core rate times the number of shards.
    let image_fallback_rate = workers as f64 / DEFAULT_IMAGE_OCR_SECS;
    let mut notes_throughput = Throughput::new(1000.0);
    let mut images_throughput = Throughput::new(image_fallback_rate);
    // Running totals across the whole batch; the ETA is always what is left of both
    // phases in every vault, not just the one being read
    let mut notes_done: usize = 0;
    let mut images_done: usize = 0;

    for job in jobs {
        let VaultJob { clean_key, cache_path, ocr_cache_path, cached_files, mut ocr_cache, scan } = job;

        state.status = phase_label(STATUS_NOTES, &clean_key, multi);
        state_writer.write(&state, true);

        // The cached entries are only needed as a lookup now that the scan is done, so
        // the vector is consumed into the map rather than being cloned into it
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
        drop(old_cache);

        let notes_before = notes_done;
        let parsed_notes = parse_notes_parallel(&scan.dirty_notes, &scan.notes, workers, |done| {
            let done_total = notes_before + done;
            notes_throughput.record(done_total.saturating_sub(notes_done));
            notes_done = done_total;
            state.progress = (notes_done + images_done) as u32;
            let notes_eta = notes_throughput.eta_secs(total_notes.saturating_sub(notes_done));
            let images_eta = images_throughput.eta_secs(total_images.saturating_sub(images_done));
            state.eta_secs = Some(notes_eta + images_eta);
            state_writer.write(&state, false);
        });
        results.extend(parsed_notes);
        notes_done = notes_before + scan.dirty_notes.len();

        // Every note update is on disk before the first attachment is read, so a crash
        // during OCR can never lose the markdown work that already finished
        results.sort_by(|a, b| b.modified.cmp(&a.modified));
        let tag_recency = build_tag_recency(&results);
        if !scan.dirty_notes.is_empty() || scan.has_deleted {
            write_json_atomic(&cache_path, &VaultCacheRef { files: &results, tag_recency: &tag_recency });
        }

        state.progress = (notes_done + images_done) as u32;
        state.status = phase_label(STATUS_ATTACHMENTS, &clean_key, multi);
        state.eta_secs = Some(
            notes_throughput.eta_secs(total_notes.saturating_sub(notes_done))
                + images_throughput.eta_secs(total_images.saturating_sub(images_done)),
        );
        state_writer.write(&state, true);

        // Attachments go through the shard pool: each file is read in a disposable child
        // process, so a PNG that crashes ImageIO or Vision costs that one file, not the
        // run. The cache is checkpointed on a cadence rather than per file; rewriting the
        // whole map after every attachment made total I/O quadratic in the attachment count.
        let mut since_checkpoint: usize = 0;
        let mut last_checkpoint = Instant::now();

        run_ocr_pool(scan.dirty_images.clone(), workers, |outcome| {
            let modified = scan.images.get(&outcome.path).copied().unwrap_or(SystemTime::UNIX_EPOCH);
            // A crashed or timed-out file is recorded as empty with its real mtime, so it
            // is never retried on the next pass
            ocr_cache.insert(outcome.path, OcrResult { modified, text: outcome.text });

            images_done += 1;
            since_checkpoint += 1;
            images_throughput.record(1);

            state.progress = (notes_done + images_done) as u32;
            state.eta_secs = Some(
                notes_throughput.eta_secs(total_notes.saturating_sub(notes_done))
                    + images_throughput.eta_secs(total_images.saturating_sub(images_done)),
            );
            state_writer.write(&state, false);

            if since_checkpoint >= CHECKPOINT_FILES || last_checkpoint.elapsed().as_secs_f64() >= CHECKPOINT_SECS {
                write_json_atomic(&ocr_cache_path, &ocr_cache);
                since_checkpoint = 0;
                last_checkpoint = Instant::now();
            }
        });

        // Entries whose files vanished from disk are dropped rather than kept forever
        ocr_cache.retain(|path, _| scan.images.contains_key(path));

        if !scan.dirty_images.is_empty() || scan.has_deleted {
            write_json_atomic(&ocr_cache_path, &ocr_cache);
        }

        // The sort and tag recency were computed when the notes finished; reading
        // attachments changes neither, so the values from before the OCR pass still hold
        write_json_atomic(&cache_path, &VaultCacheRef { files: &results, tag_recency: &tag_recency });
    }
    // Every state file is removed by the guard here, whichever way the worker ended
}

/// Writes the initial state file for every vault in `pending`, spawns one worker for
/// the whole batch, and returns the first frame of the progress UI. Each entry pairs a
/// vault key with the number of files its indexing will read; the sum is the batch total.
///
/// A worker that never started would leave the UI pinned to a progress bar, so on a
/// failed spawn the state files are removed and `None` is returned instead.
pub fn start_worker_with_feedback(pending: &[(String, usize)], cache_dir: &Path) -> Option<AlfredOutput> {
    let keys: Vec<String> = pending.iter().map(|(key, _)| key.clone()).collect();
    let total: usize = pending.iter().map(|(_, count)| *count).sum();

    let state = State {
        progress: 0,
        total: total as u32,
        status: STATUS_SCANNING.to_string(),
        eta_secs: None,
        worker_pid: None,
        vaults: keys.iter().map(|key| key.replace("#", "")).collect(),
    };

    let state_paths: Vec<PathBuf> = keys.iter().map(|key| state_path_for(cache_dir, key)).collect();
    for path in &state_paths {
        write_json_atomic(path, &state);
    }

    if !spawn_worker(&keys) {
        for path in &state_paths {
            fs::remove_file(path).ok();
        }
        return None;
    }

    Some(AlfredOutput {
        rerun: Some(0.2),
        items: vec![
            Item::new(format!("Indexing {}: 0%", batch_label(&state.vaults)))
                .set_subtitle("Initializing background worker. Please wait...")
                .set_valid(false)
        ],
    })
}

/// Re-runs this binary as a detached background indexer for a batch of vaults.
///
/// Returns false when the spawn failed, so the caller can delete the state files it
/// just wrote instead of leaving Alfred pinned to a progress bar that never advances.
pub fn spawn_worker(target_keys: &[String]) -> bool {
    match env::current_exe() {
        Ok(exe) => Command::new(exe)
            .arg("worker")
            .args(target_keys)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            // Own process group, so closing Alfred cannot signal the worker
            .process_group(0)
            .spawn()
            .is_ok(),
        Err(_) => false,
    }
}

/// Shows the indexing progress UI while a worker owns `state_path`. The bar is labelled
/// for the whole batch recorded in the state, so a user looking at one vault still sees
/// a countdown that covers every vault being indexed.
///
/// Returns the output to print whenever a worker is live, and `None` when there is no
/// state file, or when the one found has been abandoned: either its worker pid is gone,
/// or it carries no pid to probe and has not been touched in `STALE_STATE_SECS`. An
/// abandoned file is deleted so the next run starts clean.
pub fn check_worker_status(state_path: &Path) -> Option<AlfredOutput> {
    if state_path.exists() {
        let state_age_secs = fs::metadata(&state_path)
            .and_then(|meta| meta.modified())
            .ok()
            .and_then(|modified| modified.elapsed().ok())
            .map(|elapsed| elapsed.as_secs())
            .unwrap_or(0);

        let parsed_state: Option<State> = fs::read_to_string(&state_path)
            .ok()
            .and_then(|data| serde_json::from_str::<State>(&data).ok());

        // Signal 0 checks existence only; it succeeds while the worker is running
        let worker_pid = parsed_state.as_ref().and_then(|state| state.worker_pid);
        let worker_alive = match worker_pid {
            Some(pid) => unsafe { kill(pid as i32, 0) == 0 },
            None => false,
        };

        let is_stale = match worker_pid {
            Some(_) => !worker_alive,
            // No pid to probe (state file from an older build, or unreadable)
            None => state_age_secs > STALE_STATE_SECS,
        };

        if is_stale {
            fs::remove_file(&state_path).ok();
        } else {
            let state = parsed_state.unwrap_or(State {
                progress: 0,
                total: 0,
                status: STATUS_SCANNING.to_string(),
                eta_secs: None,
                worker_pid: None,
                vaults: Vec::new(),
            });

            let percentage = if state.total > 0 {
                (state.progress as f32 / state.total as f32) * 100.0
            } else {
                0.0
            };

            // Only the phase, the count and the percentage are shown; the worker never
            // puts file names in the status. With no total yet the phase alone explains
            // what is happening, which is what the cold-start and pre-scan states look like.
            let subtitle = if state.total > 0 {
                format!("{} ({} of {} files). Please wait...", state.status, state.progress, state.total)
            } else {
                format!("{}. Please wait...", state.status)
            };

            let label = batch_label(&state.vaults);

            // The worker rewrites its own ETA as it goes; counting the state file's age off
            // it keeps the number ticking while one slow file is being read. Once a file
            // outruns the estimate there is nothing left to count, so the countdown is
            // dropped rather than left frozen at 01s.
            let title = match state.eta_secs {
                Some(eta) => {
                    let remaining = eta.saturating_sub(state_age_secs);
                    if remaining > 0 {
                        format!("Indexing {}: {:.0}% ({} left)", label, percentage, format_eta(remaining))
                    } else {
                        format!("Indexing {}: {:.0}%", label, percentage)
                    }
                }
                None => format!("Indexing {}: {:.0}%", label, percentage),
            };

            return Some(AlfredOutput {
                rerun: Some(0.2),
                items: vec![
                    Item::new(title)
                        .set_subtitle(subtitle)
                        .set_valid(false)
                ]
            });
        }
    }

    None
}
