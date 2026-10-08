use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::env;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::ffi::is_supported_image;
use crate::ocr_pool::recognize_text_isolated;
use crate::types::*;

pub fn expand_tilde(path: &str) -> String {
    if path.starts_with("~/") {
        if let Ok(home) = env::var("HOME") {
            return path.replacen("~/", &format!("{}/", home), 1);
        }
    }
    path.to_string()
}

pub fn get_vault_map() -> HashMap<String, String> {
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

pub fn get_workflow_cache_dir() -> PathBuf {
    let dir = env::var("alfred_workflow_cache")
        .unwrap_or_else(|_| env::temp_dir().to_string_lossy().into_owned());
    let path = PathBuf::from(dir);
    fs::create_dir_all(&path).ok();
    path
}

/// Walks the vault once (skipping hidden dot-directories) and reports which notes and
/// attachments differ from what the caches already hold, so only those need re-reading.
pub fn scan_vault(dir: &Path, cached_files: &[FileResult], ocr_cache: &OcrCache) -> VaultScan {
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

/// Recursively records every markdown note and supported attachment (images and PDFs)
/// below `dir`, skipping hidden directories the way the rest of the workflow does.
pub fn collect_vault_files(
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

pub fn clean_tag(raw_tag: &str) -> Option<String> {
    let t = raw_tag.trim().trim_start_matches('#');
    if t.is_empty() || t.contains(|c: char| !c.is_alphanumeric() && c != '_' && c != '-' && c != '/') {
        None
    } else {
        Some(t.to_lowercase())
    }
}

pub fn parse_inline_list(val: &str, tags: &mut HashSet<String>) {
    let clean_val = val.trim_matches(|c| c == '[' || c == ']' || c == ' ');
    for t in clean_val.split(',') {
        if let Some(clean) = clean_tag(t) {
            tags.insert(clean);
        }
    }
}

/// Parses a note's tags and the basenames of the files it embeds in one pass.
pub fn extract_tags_and_links(path: &Path) -> (Vec<String>, Vec<String>) {
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
                    // Drop the display alias first, then any #page= / #anchor fragment,
                    // so `[[paper.pdf#page=3|See p3]]` still resolves to paper.pdf
                    let target = content.split('|').next().unwrap_or(content);
                    let target = target.split('#').next().unwrap_or(target).trim();
                    if let Some(name) = Path::new(target).file_name().and_then(|n| n.to_str()) {
                        links.insert(name.to_string());
                    }
                    ptr = &ptr[end + 2..];
                } else {
                    break;
                }
            }

            let mut ptr = line.as_str();
            while let Some(start) = ptr.find("](") {
                ptr = &ptr[start + 2..];
                if let Some(end) = ptr.find(')') {
                    // Same idea here: ![](paper.pdf#page=3) must still match paper.pdf
                    let target = &ptr[..end];
                    let target = target.split('#').next().unwrap_or(target).trim();
                    if let Some(name) = Path::new(target).file_name().and_then(|n| n.to_str()) {
                        links.insert(name.replace("%20", " "));
                    }
                    ptr = &ptr[end + 1..];
                } else {
                    break;
                }
            }
        }
    }

    (tags.into_iter().collect(), links.into_iter().collect())
}

/// Reads one note's tags and embeds and builds its cache entry.
pub fn parse_note(path: &str, modified: SystemTime) -> FileResult {
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
pub fn build_tag_recency(files: &[FileResult]) -> HashMap<String, SystemTime> {
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

/// Reads and deserializes the cache file, removing it if it is unreadable or corrupt.
pub fn load_cache(cache_path: &Path) -> Option<VaultCache> {
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

/// Serializes `value` to JSON and writes it to `path` without ever exposing a partial
/// file: the bytes land in a sibling `.json.tmp` file and are then renamed over the
/// target, so Alfred's 0.2s rerun polls read either the old file or the complete new one.
pub fn write_json_atomic<T: Serialize>(path: &Path, value: &T) {
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

/// Brings the two in-memory caches in line with a fresh `scan` of the vault, then writes
/// them back to disk atomically.
///
/// Only what the scan marked dirty is re-read: unchanged notes keep their cached entry,
/// entries whose files vanished are dropped, and the tag recency map is rebuilt from the
/// surviving files so tags that disappeared do not linger in the cache forever.
pub fn reconcile_inline_cache(
    vault_dir: &Path,
    cached_data: &mut VaultCache,
    ocr_cache: &mut OcrCache,
    scan: &VaultScan,
    cache_path: &Path,
    ocr_cache_path: &Path,
) {
    // The scan already carries every absolute path this reconciliation works with, so
    // the vault directory itself is not needed here; it stays in the signature for the
    // Phase 2 module split.
    let _ = vault_dir;

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

    // Even the single inline attachment is read in a disposable shard process: a PNG
    // that crashes ImageIO or Vision must not take the Alfred script filter down with it
    for path in &scan.dirty_images {
        let modified = scan.images.get(path).copied().unwrap_or(SystemTime::UNIX_EPOCH);
        let text = recognize_text_isolated(path);
        ocr_cache.insert(path.clone(), OcrResult { modified, text });
    }

    cached_data.tag_recency = build_tag_recency(&cached_data.files);

    write_json_atomic(cache_path, cached_data);
    // The attachment cache is only rewritten when attachments changed or dropped
    if !scan.dirty_images.is_empty() || scan.has_deleted {
        write_json_atomic(ocr_cache_path, ocr_cache);
    }
}

