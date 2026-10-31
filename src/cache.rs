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

/// Normalises a candidate tag the way Obsidian does: the leading `#` is dropped, only
/// letters, digits, `_`, `-` and `/` are allowed, and the result must contain at least
/// one non-digit, since `#0` or `#2024` are never tags, even in plain prose.
pub fn clean_tag(raw_tag: &str) -> Option<String> {
    let t = raw_tag.trim().trim_start_matches('#');
    if t.is_empty()
        || t.contains(|c: char| !c.is_alphanumeric() && c != '_' && c != '-' && c != '/')
        || t.chars().all(|c| c.is_ascii_digit())
    {
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

/// Returns the line with every enclosed span blanked out, leaving only plain prose for
/// the tag pass. A `#word` inside any of these is not a tag in Obsidian or Markdown:
///
/// - inline code `` `...` ``
/// - wikilinks `[[...]]`, markdown links `[...](...)` and bare bracketed text `[...]`
/// - emphasis `*...*` / `**...**`, strikethrough `~~...~~`, highlight `==...==`
/// - inline and display math `$...$` / `$$...$$`
/// - Obsidian comments `%%...%%` and HTML comments `<!--...-->`
///
/// Each span is replaced by a single space so the words on either side stay separate.
/// An unterminated span is left alone, except for `%%` comments, which Obsidian lets
/// run across lines: `in_comment` carries that state to the next call.
///
/// Underscore emphasis is deliberately not handled: `_` is a legal tag character, so
/// `#a_b and #c_d` would otherwise lose everything between the underscores.
pub fn strip_enclosed_spans(line: &str, in_comment: &mut bool) -> String {
    // Longer delimiters first, so `**` is not consumed as two `*` and `$$` as two `$`
    const PAIRS: &[(&str, &str)] = &[
        ("<!--", "-->"),
        ("%%", "%%"),
        ("```", "```"),
        ("`", "`"),
        ("[[", "]]"),
        ("$$", "$$"),
        ("$", "$"),
        ("**", "**"),
        ("*", "*"),
        ("~~", "~~"),
        ("==", "=="),
    ];

    let mut out = String::with_capacity(line.len());
    let mut rest = line;

    while !rest.is_empty() {
        // Inside a comment that began on an earlier line: skip up to its close
        if *in_comment {
            match rest.find("%%") {
                Some(end) => {
                    rest = &rest[end + 2..];
                    *in_comment = false;
                    out.push(' ');
                    continue;
                }
                None => return out,
            }
        }

        let mut consumed = false;

        for (open, close) in PAIRS {
            if !rest.starts_with(open) {
                continue;
            }
            let body = &rest[open.len()..];
            match body.find(close) {
                Some(end) => {
                    // The span and both delimiters vanish; a space keeps neighbours apart
                    out.push(' ');
                    rest = &body[end + close.len()..];
                    consumed = true;
                }
                None if *open == "%%" => {
                    // An opening %% with no close starts a block comment
                    *in_comment = true;
                    return out;
                }
                None => {}
            }
            break;
        }
        if consumed {
            continue;
        }

        // A markdown link or bracketed text: `[label](target)` or just `[label]`
        if rest.starts_with('[') {
            if let Some(end) = rest.find(']') {
                let mut after = &rest[end + 1..];
                if after.starts_with('(') {
                    if let Some(paren_end) = after.find(')') {
                        after = &after[paren_end + 1..];
                    }
                }
                out.push(' ');
                rest = after;
                continue;
            }
        }

        // Ordinary character: copy it and move on
        let ch = rest.chars().next().unwrap();
        out.push(ch);
        rest = &rest[ch.len_utf8()..];
    }

    out
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Decodes `%XX` byte sequences (such as `%20` for spaces or `%28`/`%29` for parentheses
/// in Markdown exports) as UTF-8, leaving any malformed `%` sequence untouched.
fn percent_decode(input: &str) -> String {
    if !input.contains('%') {
        return input.to_string();
    }
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(hi), Some(lo)) = (hex_val(bytes[i + 1]), hex_val(bytes[i + 2])) {
                out.push((hi << 4) | lo);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Strips an optional trailing Markdown link title (`path/img.png "Title"` or `'Title'`).
fn strip_markdown_link_title(dest: &str) -> &str {
    let trimmed = dest.trim();
    if let Some(quote) = trimmed.chars().last().filter(|&c| c == '"' || c == '\'') {
        let inner = &trimmed[..trimmed.len() - quote.len_utf8()];
        if let Some(open_pos) = inner.rfind(quote) {
            let before = &inner[..open_pos];
            if before.ends_with(char::is_whitespace) {
                return before.trim_end();
            }
        }
    }
    trimmed
}

/// Normalizes a raw wikilink or Markdown link target down to its attachment path (keeping
/// any directory prefix), or returns `None` for external URLs and non-attachment targets.
fn clean_attachment_link(raw: &str, url_encoded: bool) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }

    let lower = trimmed.to_ascii_lowercase();
    for scheme in ["http://", "https://", "mailto:", "obsidian://", "data:", "file://"] {
        if lower.starts_with(scheme) {
            return None;
        }
    }

    // Strip `#page=3` / `#anchor` before percent-decoding so `%23` in an encoded filename
    // still decodes to a literal `#`
    let without_fragment = trimmed.split('#').next().unwrap_or(trimmed).trim();
    let decoded = if url_encoded {
        percent_decode(without_fragment)
    } else {
        without_fragment.to_string()
    };

    let normalized = decoded.replace('\\', "/").trim().to_string();
    if normalized.is_empty() || !is_supported_image(Path::new(&normalized)) {
        return None;
    }

    Some(normalized)
}

/// Parses a note's tags and embedded attachment links in one pass.
pub fn extract_tags_and_links(path: &Path) -> (Vec<String>, Vec<String>) {
    let mut tags = HashSet::new();
    let mut links = HashSet::new();
    
    if let Ok(file) = fs::File::open(path) {
        let reader = BufReader::new(file);
        
        let mut in_frontmatter = false;
        let mut line_count = 0;
        let mut inside_tags_block = false;
        // Multi-line constructs whose contents are never tags
        let mut in_fence = false;
        let mut in_comment = false;

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
            } else if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
                // Fenced code blocks span lines; nothing inside one is a tag, and the
                // fence line itself carries only the language hint
                in_fence = !in_fence;
            } else if !in_fence {
                // Only plain prose can hold a tag: code, links, brackets, emphasis and
                // the other enclosing syntaxes are removed before the words are read
                let prose = strip_enclosed_spans(&line, &mut in_comment);
                for word in prose.split_whitespace() {
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
                    // Drop any `|alias` or `|400` resize suffix first, then clean the path
                    // while keeping directory components intact
                    let target = content.split('|').next().unwrap_or(content);
                    if let Some(clean) = clean_attachment_link(target, false) {
                        links.insert(clean);
                    }
                    ptr = &ptr[end + 2..];
                } else {
                    break;
                }
            }

            let mut ptr = line.as_str();
            while let Some(start) = ptr.find("](") {
                ptr = &ptr[start + 2..];
                let trimmed_start = ptr.trim_start();
                if let Some(after_lt) = trimmed_start.strip_prefix('<') {
                    if let Some(gt_pos) = after_lt.find('>') {
                        let raw_target = &after_lt[..gt_pos];
                        if let Some(clean) = clean_attachment_link(raw_target, true) {
                            links.insert(clean);
                        }
                        let rest = &after_lt[gt_pos + 1..];
                        if let Some(close_paren) = rest.find(')') {
                            ptr = &rest[close_paren + 1..];
                        } else {
                            break;
                        }
                    } else {
                        break;
                    }
                } else {
                    // Track balanced parentheses so unencoded parens in a folder or file
                    // name like `![](note (xkcd)/image.png)` do not stop at the inner `)`
                    let mut depth = 0usize;
                    let mut end_idx = None;
                    for (idx, ch) in ptr.char_indices() {
                        match ch {
                            '(' => depth += 1,
                            ')' => {
                                if depth == 0 {
                                    end_idx = Some(idx);
                                    break;
                                }
                                depth -= 1;
                            }
                            _ => {}
                        }
                    }
                    if let Some(end) = end_idx {
                        let raw_dest = strip_markdown_link_title(&ptr[..end]);
                        if let Some(clean) = clean_attachment_link(raw_dest, true) {
                            links.insert(clean);
                        }
                        ptr = &ptr[end + 1..];
                    } else {
                        break;
                    }
                }
            }
        }
    }

    let mut tag_list: Vec<String> = tags.into_iter().collect();
    tag_list.sort();
    let mut link_list: Vec<String> = links.into_iter().collect();
    link_list.sort();
    (tag_list, link_list)
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
            // A cache from an older parser is treated like a corrupt one: removing it
            // triggers a cold start, so every note is re-read with the current rules
            Ok(parsed_data) if parsed_data.parser_version == PARSER_VERSION => Some(parsed_data),
            Ok(_) | Err(_) => {
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
    cached_data.parser_version = PARSER_VERSION;

    write_json_atomic(cache_path, cached_data);
    // The attachment cache is only rewritten when attachments changed or dropped
    if !scan.dirty_images.is_empty() || scan.has_deleted {
        write_json_atomic(ocr_cache_path, ocr_cache);
    }
}

