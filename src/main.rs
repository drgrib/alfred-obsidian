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
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

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

#[derive(Serialize, Deserialize, Clone)]
struct FileResult {
    title: String,
    path: String,
    modified: SystemTime,
    tags: Vec<String>,
    /// Set only for notes found via content search; never persisted to the cache.
    #[serde(skip)]
    snippet: Option<String>,
}

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

fn count_files(dir: &Path) -> u32 {
    let mut count = 0;
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                    if !name.starts_with('.') {
                        count += count_files(&path);
                    }
                }
            } else if path.extension().and_then(|e| e.to_str()) == Some("md") {
                count += 1;
            }
        }
    }
    count
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

fn extract_tags(path: &Path) -> Vec<String> {
    let mut tags = HashSet::new();
    
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
        }
    }
    
    tags.into_iter().collect()
}

fn process_files(dir: &Path, results: &mut Vec<FileResult>, old_cache: &HashMap<String, FileResult>, state: &mut State, state_path: &Path) {
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                    if !name.starts_with('.') {
                        process_files(&path, results, old_cache, state, state_path);
                    }
                }
            } else if path.extension().and_then(|e| e.to_str()) == Some("md") {
                if let Some(stem) = path.file_stem().and_then(|n| n.to_str()) {
                    let modified = fs::metadata(&path)
                        .and_then(|m| m.modified())
                        .unwrap_or(SystemTime::UNIX_EPOCH);

                    let path_key = path.to_string_lossy().into_owned();

                    // Reuse the cached entry when the file is unchanged, otherwise re-parse it
                    let result = match old_cache.get(&path_key) {
                        Some(cached) if cached.modified == modified => cached.clone(),
                        _ => FileResult {
                            title: stem.to_string(),
                            path: path.to_string_lossy().into_owned(),
                            modified,
                            tags: extract_tags(&path),
                            snippet: None,
                        },
                    };

                    results.push(result);

                    state.progress += 1;
                    if state.progress % 250 == 0 {
                        if let Ok(json) = serde_json::to_string(state) {
                            fs::write(state_path, json).ok();
                        }
                    }
                }
            }
        }
    }
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

fn run_worker(target_key: &str) {
    let vault_map = get_vault_map();
    let target_path = match vault_map.get(target_key) {
        Some(p) => expand_tilde(p),
        None => return,
    };
    let vault_dir = Path::new(&target_path);

    let cache_dir = get_workflow_cache_dir();
    let clean_key = target_key.replace("#", "");
    let state_path = cache_dir.join(format!("state_{}.json", clean_key));
    let cache_path = cache_dir.join(format!("vault_cache_{}.json", clean_key));

    let mut state = State { progress: 0, total: 0, status: "Counting files...".to_string() };
    fs::write(&state_path, serde_json::to_string(&state).unwrap()).ok();

    state.total = count_files(vault_dir);
    state.status = "Indexing vault...".to_string();
    fs::write(&state_path, serde_json::to_string(&state).unwrap()).ok();

    // Load the existing cache (if any) so unchanged files can be reused without re-reading them
    let existing_cache: Option<VaultCache> = fs::read_to_string(&cache_path)
        .ok()
        .and_then(|content| serde_json::from_str::<VaultCache>(&content).ok());

    let mut old_cache: HashMap<String, FileResult> = HashMap::new();
    if let Some(cache) = existing_cache {
        for file in cache.files {
            old_cache.insert(file.path.clone(), file);
        }
    }

    let mut results = Vec::new();
    process_files(vault_dir, &mut results, &old_cache, &mut state, &state_path);

    results.sort_by(|a, b| b.modified.cmp(&a.modified));

    // Rebuild tag recency from the new results so tags that no longer exist are cleared
    let mut tag_recency: HashMap<String, SystemTime> = HashMap::new();
    for res in &results {
        for tag in &res.tags {
            let entry = tag_recency.entry(tag.clone()).or_insert(SystemTime::UNIX_EPOCH);
            if res.modified > *entry {
                *entry = res.modified;
            }
        }
    }

    if let Ok(json) = serde_json::to_string(&VaultCache { files: results, tag_recency }) {
        fs::write(&cache_path, json).ok();
    }
    fs::remove_file(&state_path).ok();
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

    // Load the cache first so results can be served immediately on every keystroke
    let mut cached_data_opt = None;
    if cache_path.exists() {
        cached_data_opt = load_cache(&cache_path);
    }

    // With a usable cache, only refresh on the first launch (empty query) so that
    // typing never triggers a reindex. The refresh runs synchronously on this thread:
    // it is an incremental update against an existing cache, and blocking until it
    // finishes means Alfred renders the final list once instead of re-rendering
    // underneath the user's cursor (which caused the cursor to snap).
    if cached_data_opt.is_some() && query.is_empty() {
        run_worker(target_key);

        // The worker has just rewritten the cache file, so re-read it to serve the
        // freshly indexed data rather than the copy loaded above.
        if let Some(refreshed) = load_cache(&cache_path) {
            cached_data_opt = Some(refreshed);
        }
    }

    // Only show the progress screen when there is no cache to serve; otherwise the
    // worker finishes invisibly in the background
    if cached_data_opt.is_none() && state_path.exists() {
        let data = fs::read_to_string(&state_path).unwrap_or_default();
        let state = serde_json::from_str::<State>(&data).unwrap_or(State { progress: 0, total: 0, status: "Initializing...".to_string() });
        
        let percentage = if state.total > 0 {
            (state.progress as f32 / state.total as f32) * 100.0
        } else {
            0.0
        };

        let output = AlfredOutput {
            rerun: Some(0.2), 
            items: vec![
                Item::new(format!("{} {:.0}%", state.status, percentage))
                    .set_subtitle(format!("{} of {} files parsed. Please wait...", state.progress, state.total))
                    .set_valid(false)
            ]
        };
        println!("{}", serde_json::to_string(&output).unwrap());
        return;
    }

    // Nothing cached and no worker running: start one and show the progress UI
    if cached_data_opt.is_none() && !state_path.exists() {
        Command::new(env::current_exe().unwrap())
            .arg("worker")
            .arg(target_key)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("Failed to start background worker");

        let output = AlfredOutput {
            rerun: Some(0.2),
            items: vec![
                Item::new("Starting Indexer...")
                    .set_subtitle("Initializing background worker to build cache. Please wait...")
                    .set_valid(false)
            ]
        };
        println!("{}", serde_json::to_string(&output).unwrap());
        return;
    }

    let cached_data = cached_data_opt.unwrap();
    let mut results = cached_data.files;
    let tag_recency = cached_data.tag_recency;
    let mut items = Vec::new();

    // Snapshot of every cached file keyed by path, taken before `results` is narrowed
    // by the title/tag filter so content-search hits can still be resolved back to
    // their cached title, modified time and tags.
    let file_lookup: HashMap<String, FileResult> = results
        .iter()
        .map(|res| (res.path.clone(), res.clone()))
        .collect();

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

    let is_duplicate = results
        .iter()
        .any(|res| res.title.eq_ignore_ascii_case(&title_string));

    if !is_create_only && !is_empty_search {
        results.retain(|res| {
            let lower_title = res.title.to_lowercase();
            let matches_title = title_terms.iter().all(|term| lower_title.contains(*term));
            let matches_tags = tag_terms.iter().all(|term| res.tags.iter().any(|t| t == *term));
            
            matches_title && matches_tags
        });
    }

    // Full-text fallback: when title/tag matches are sparse, grep the note bodies so
    // notes that merely mention the query still surface. Every term has to appear
    // somewhere in the file; terms under two characters are too noisy to search for.
    if results.len() < 50 && !title_terms.is_empty() && title_terms.iter().all(|t| t.len() >= 2) {
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

                let mut file_result = match file_lookup.get(&content_match.path) {
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
                            snippet: None,
                        }
                    }
                };

                file_result.snippet = Some(content_match.snippet);
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

    results.sort_by(|a, b| {
        rank_result(a)
            .cmp(&rank_result(b))
            .then_with(|| b.modified.cmp(&a.modified))
    });

    results.truncate(50);

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
                .set_arg(format!("obsidian://advanced-uri?filepath={}|{}", url_encode(&arg_path), res.title))
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