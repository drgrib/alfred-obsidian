use alfred_workflow_rs::Item;
use std::collections::HashMap;
use std::env;
use std::fs;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Instant, SystemTime};

use crate::alfred::format_eta;
use crate::cache::*;
use crate::ffi::{kill, recognize_text};
use crate::types::*;

/// Removes the worker's state file when the worker ends, whether it finished normally,
/// bailed out early, or panicked, so a dead worker can never pin the UI to a progress bar.
struct StateFileGuard(PathBuf);

impl Drop for StateFileGuard {
    fn drop(&mut self) {
        fs::remove_file(&self.0).ok();
    }
}

pub fn run_worker(target_key: &str) {
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

    let mut state = State { progress: 0, total: existing_total, status: "Scanning vault...".to_string(), eta_secs: None, worker_pid: None };
    // main() probes this pid to tell a slow worker apart from a dead one
    state.worker_pid = Some(std::process::id());
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

    // Notes are parsed far faster than attachments are read, so each phase is measured
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
            // The attachments have not started yet, so they are costed at the default rate
            let remaining_notes = total_notes.saturating_sub(notes_done);
            let avg_note_secs = notes_start.elapsed().as_secs_f64() / notes_done as f64;
            let est_secs = (remaining_notes as f64 * avg_note_secs)
                + (total_images as f64 * DEFAULT_IMAGE_OCR_SECS);
            state.eta_secs = Some(est_secs.ceil() as u64);

            write_json_atomic(&state_path, &state);
        }
    }

    // Every note update is on disk before the first attachment is read, so a crash
    // during OCR can never lose the markdown work that already finished
    results.sort_by(|a, b| b.modified.cmp(&a.modified));
    let tag_recency = build_tag_recency(&results);
    if !scan.dirty_notes.is_empty() || scan.has_deleted {
        write_json_atomic(&cache_path, &VaultCache {
            files: results.clone(),
            tag_recency: tag_recency.clone(),
        });
    }

    let mut images_done: usize = 0;
    // Only Vision-speed work feeds the average: a PDF with an embedded text layer
    // returns in milliseconds and would otherwise drag the estimate to nothing
    let mut slow_ocr_secs: f64 = 0.0;
    let mut slow_ocr_count: usize = 0;

    for path in &scan.dirty_images {
        let modified = scan.images.get(path).copied().unwrap_or(SystemTime::UNIX_EPOCH);

        // ETA for the attachments left, counting the one about to be read
        let remaining_images = total_images.saturating_sub(images_done);
        let avg_secs = if slow_ocr_count > 0 {
            slow_ocr_secs / slow_ocr_count as f64
        } else {
            DEFAULT_IMAGE_OCR_SECS
        };
        state.eta_secs = Some((remaining_images as f64 * avg_secs).ceil() as u64);

        // Heartbeat before the slow call: Alfred sees which file is being read, and the
        // state file's mtime proves the worker is still making progress
        let file_name = Path::new(path)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("file");
        state.status = format!("Indexing {}", file_name);
        write_json_atomic(&state_path, &state);

        // Pre-checkpoint with empty text: if this file crashes PDFKit or Vision with a
        // fatal signal, it is already recorded on disk and will not be retried forever
        ocr_cache.insert(path.clone(), OcrResult { modified, text: String::new() });
        write_json_atomic(&ocr_cache_path, &ocr_cache);

        let item_start = Instant::now();
        let text = recognize_text(path);
        let item_secs = item_start.elapsed().as_secs_f64();
        if item_secs >= 0.1 {
            slow_ocr_secs += item_secs;
            slow_ocr_count += 1;
        }

        if !text.is_empty() {
            ocr_cache.insert(path.clone(), OcrResult { modified, text });
            write_json_atomic(&ocr_cache_path, &ocr_cache);
        }

        images_done += 1;
        state.progress += 1;

        // Re-estimate with whatever the file just taught us about Vision's speed
        let remaining_images = total_images.saturating_sub(images_done);
        let avg_secs = if slow_ocr_count > 0 {
            slow_ocr_secs / slow_ocr_count as f64
        } else {
            DEFAULT_IMAGE_OCR_SECS
        };
        state.eta_secs = Some((remaining_images as f64 * avg_secs).ceil() as u64);

        write_json_atomic(&state_path, &state);
    }

    // Entries whose files vanished from disk are dropped rather than kept forever
    ocr_cache.retain(|path, _| scan.images.contains_key(path));

    write_json_atomic(&ocr_cache_path, &ocr_cache);

    // The sort and tag recency were computed when the notes finished; reading attachments
    // changes neither, so the values from before the OCR pass still hold
    write_json_atomic(&cache_path, &VaultCache { files: results, tag_recency });
    // state_path is removed by the guard here, whichever way the worker ended
}

/// Re-runs this binary as a detached background indexer for one vault.
///
/// Returns false when the spawn failed, so the caller can delete the state file it just
/// wrote instead of leaving Alfred pinned to a progress bar that never advances.
pub fn spawn_worker(target_key: &str) -> bool {
    match env::current_exe() {
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
    }
}

/// Shows the indexing progress UI while a worker owns `state_path`.
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
            let state = parsed_state.unwrap_or(State { progress: 0, total: 0, status: "Indexing vault...".to_string(), eta_secs: None, worker_pid: None });

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

            // The worker rewrites its own ETA as it goes; counting the state file's age off
            // it keeps the number ticking while one slow file is being read. Once a file
            // outruns the estimate there is nothing left to count, so the countdown is
            // dropped rather than left frozen at 01s.
            let title = match state.eta_secs {
                Some(eta) => {
                    let remaining = eta.saturating_sub(state_age_secs);
                    if remaining > 0 {
                        format!("Indexing Vault: {:.0}% ({})", percentage, format_eta(remaining))
                    } else {
                        format!("Indexing Vault: {:.0}%", percentage)
                    }
                }
                None => format!("Indexing Vault: {:.0}%", percentage),
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

