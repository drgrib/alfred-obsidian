use grep_regex::{RegexMatcher, RegexMatcherBuilder};
use grep_searcher::sinks::UTF8;
use grep_searcher::Searcher;
use ignore::{
    DirEntry, Error as IgnoreError, ParallelVisitor, ParallelVisitorBuilder, WalkBuilder, WalkState,
};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Component, Path};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use crate::types::{ContentMatch, FileResult, OcrCache, QueryTerm, MAX_CONTENT_MATCHES};

fn is_quote_char(c: char) -> bool {
    matches!(c, '"' | '\u{201C}' | '\u{201D}')
}

pub struct ParsedQuery {
    pub search_terms: Vec<QueryTerm>,
    pub tag_terms: Vec<String>,
    pub raw_tag_tokens: Vec<String>,
    pub create_title: String,
    /// `(prefix_before_hash, partial_tag_without_hash)` when the cursor sits on an
    /// unquoted `#tag` at the very end of `raw_query`.
    pub autocompleting_tag: Option<(String, String)>,
}

/// Splits the raw query into unquoted words, `"..."` exact phrases, and `#tag` filters.
///
/// Unclosed quotes still match as an exact phrase while the user is mid-typing, and `#`
/// inside quotes stays literal text rather than triggering tag autocomplete.
pub fn parse_query(raw_query: &str) -> ParsedQuery {
    let mut search_terms = Vec::new();
    let mut tag_terms = Vec::new();
    let mut raw_tag_tokens = Vec::new();
    let mut title_parts = Vec::new();
    let mut autocompleting_tag = None;

    let mut chars = raw_query.char_indices().peekable();
    while let Some(&(_, ch)) = chars.peek() {
        if ch.is_whitespace() {
            chars.next();
            continue;
        }

        if is_quote_char(ch) {
            chars.next();
            let start = chars.peek().map(|&(idx, _)| idx).unwrap_or(raw_query.len());
            let mut end = raw_query.len();
            while let Some((idx, c)) = chars.next() {
                if is_quote_char(c) {
                    end = idx;
                    break;
                }
            }
            let inner = &raw_query[start..end];
            let normalized_orig: String = inner.split_whitespace().collect::<Vec<&str>>().join(" ");
            if !normalized_orig.is_empty() {
                search_terms.push(QueryTerm {
                    text: normalized_orig.to_lowercase(),
                    quoted: true,
                });
                title_parts.push(normalized_orig);
            }
        } else {
            let (start, _) = chars.next().unwrap();
            let mut end = raw_query.len();
            while let Some(&(idx, c)) = chars.peek() {
                if c.is_whitespace() || is_quote_char(c) {
                    end = idx;
                    break;
                }
                chars.next();
            }
            let token = &raw_query[start..end];
            let at_eof = end == raw_query.len();

            if token.starts_with('#') {
                let lower_token = token.to_lowercase();
                let clean = lower_token.trim_start_matches('#').to_string();
                raw_tag_tokens.push(lower_token);
                if at_eof {
                    let raw_prefix = &raw_query[..start];
                    let prefix =
                        if !raw_prefix.is_empty() && !raw_prefix.ends_with(char::is_whitespace) {
                            format!("{} ", raw_prefix)
                        } else {
                            raw_prefix.to_string()
                        };
                    autocompleting_tag = Some((prefix, clean));
                } else if !clean.is_empty() {
                    tag_terms.push(clean);
                }
            } else {
                search_terms.push(QueryTerm {
                    text: token.to_lowercase(),
                    quoted: false,
                });
                title_parts.push(token.to_string());
            }
        }
    }

    ParsedQuery {
        search_terms,
        tag_terms,
        raw_tag_tokens,
        create_title: title_parts.join(" "),
        autocompleting_tag,
    }
}

/// Checks whether a note's tag satisfies a `#tag` query term using Obsidian's nested tag
/// hierarchy: `#games` (or `#games/`) matches both `#games` and any subtag like
/// `#games/nintendo`, while `#game` does not match `#games/nintendo`.
pub fn tag_matches(note_tag: &str, query_tag: &str) -> bool {
    let note = note_tag.trim_matches('/');
    let query = query_tag.trim_matches('/');
    if query.is_empty() {
        return false;
    }
    note == query
        || note
            .strip_prefix(query)
            .map_or(false, |rest| rest.starts_with('/'))
}

/// Escapes regex metacharacters so a query can be matched as a literal string.
pub fn escape_regex(input: &str) -> String {
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
pub fn build_snippet(line: &str) -> String {
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
/// Quoted phrases are always matched literally (with flexible whitespace between words),
/// while unquoted terms try regex compilation first so patterns like `foo.*bar` work and
/// fall back to an escaped literal when they contain characters like `c++`.
pub fn build_term_matchers(terms: &[QueryTerm]) -> Vec<RegexMatcher> {
    terms
        .iter()
        .filter(|t| !t.text.trim().is_empty())
        .filter_map(|t| {
            let mut builder = RegexMatcherBuilder::new();
            builder.case_insensitive(true);
            if t.quoted {
                let pattern = t
                    .text
                    .split_whitespace()
                    .map(escape_regex)
                    .collect::<Vec<String>>()
                    .join(r"\s+");
                builder.build(&pattern).ok()
            } else {
                builder
                    .build(&t.text)
                    .or_else(|_| builder.build(&escape_regex(&t.text)))
                    .ok()
            }
        })
        .collect()
}

/// Builds the per-thread visitors used by `WalkBuilder::build_parallel`.
pub struct ContentVisitorBuilder {
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
pub struct ContentVisitor {
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

/// Converts an absolute path inside `vault_dir` into a lowercase, `/`-separated path
/// relative to the vault root (for example, `"notes/2011 ford fiesta/image.png"`).
fn vault_rel_lower(vault_dir: &Path, full_path: &Path) -> Option<String> {
    let rel = full_path.strip_prefix(vault_dir).ok()?;
    let mut parts: Vec<String> = Vec::new();
    for comp in rel.components() {
        if let Component::Normal(os) = comp {
            parts.push(os.to_string_lossy().to_lowercase());
        }
    }
    Some(parts.join("/"))
}

/// Joins a `/`-separated link path onto a lowercase vault-relative directory, resolving
/// `.` and `..` lexically without touching the filesystem.
fn join_vault_rel(base_dir: &str, link: &str) -> Option<String> {
    let mut parts: Vec<String> = if base_dir.is_empty() {
        Vec::new()
    } else {
        base_dir
            .split('/')
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .collect()
    };

    for seg in link.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            other => parts.push(other.to_lowercase()),
        }
    }

    if parts.is_empty() {
        None
    } else {
        Some(parts.join("/"))
    }
}

/// Resolves a note's attachment link to the lowercase vault-relative path of an indexed
/// attachment, matching Markdown and Obsidian resolution rules so generic filenames like
/// `image.png` in different folders never bleed across notes:
///
/// 1. Explicit paths containing `/`:
///    - Leading `/` resolves strictly from the vault root.
///    - Leading `./` or `../` resolves strictly from the note's directory.
///    - Any other `folder/file.png` tries the note's directory first (standard Markdown
///      and Bear exports like `Note Title/image.png`), falling back to the vault root
///      (Obsidian vault-relative wikilinks like `Attachments/image.png`).
/// 2. Bare filenames without `/`:
///    - Prefers a sibling file in the note's own directory if one exists.
///    - Otherwise matches by basename only when that basename is unique across all
///      attachments in the vault (Obsidian's "Shortest path when possible" rule).
fn resolve_attachment_link<'a>(
    note_rel_dir: &str,
    link: &str,
    all_attachments: &'a HashSet<String>,
    unique_basenames: &'a HashMap<String, Option<String>>,
) -> Option<&'a str> {
    if link.contains('/') {
        if let Some(rooted) = link.strip_prefix('/') {
            let cand = join_vault_rel("", rooted)?;
            return all_attachments.get(&cand).map(|s| s.as_str());
        }
        if link.starts_with("./") || link.starts_with("../") {
            let cand = join_vault_rel(note_rel_dir, link)?;
            return all_attachments.get(&cand).map(|s| s.as_str());
        }
        if let Some(rel_cand) = join_vault_rel(note_rel_dir, link) {
            if let Some(found) = all_attachments.get(&rel_cand) {
                return Some(found.as_str());
            }
        }
        if !note_rel_dir.is_empty() {
            if let Some(root_cand) = join_vault_rel("", link) {
                if let Some(found) = all_attachments.get(&root_cand) {
                    return Some(found.as_str());
                }
            }
        }
        None
    } else {
        if let Some(sibling) = join_vault_rel(note_rel_dir, link) {
            if let Some(found) = all_attachments.get(&sibling) {
                return Some(found.as_str());
            }
        }
        let lower_name = link.to_lowercase();
        match unique_basenames.get(&lower_name) {
            Some(Some(unique_rel)) => Some(unique_rel.as_str()),
            _ => None,
        }
    }
}

/// Checks whether an attachment's extracted text satisfies every search term.
///
/// Unquoted words match anywhere in the text. Quoted phrases must appear contiguously in
/// order; whitespace runs (including newlines between Vision OCR boxes or wrapped PDF
/// lines) are collapsed to single spaces so a phrase split across two visual lines still
/// matches.
fn ocr_text_matches_all(ocr_text: &str, terms: &[QueryTerm]) -> bool {
    let lower_text = ocr_text.to_lowercase();

    for term in terms {
        if term.quoted {
            if !term.text.split_whitespace().all(|word| lower_text.contains(word)) {
                return false;
            }
        } else if !lower_text.contains(&term.text) {
            return false;
        }
    }

    let mut collapsed_lower: Option<String> = None;
    for term in terms {
        if term.quoted && !lower_text.contains(&term.text) {
            let collapsed = collapsed_lower.get_or_insert_with(|| {
                lower_text.split_whitespace().collect::<Vec<&str>>().join(" ")
            });
            if !collapsed.contains(&term.text) {
                return false;
            }
        }
    }

    true
}

/// Extracts a readable snippet from whitespace-collapsed text when a quoted phrase
/// matched across a line wrap rather than on a single raw line.
fn snippet_from_collapsed(text: &str, needle_lower: &str) -> String {
    let collapsed: String = text.split_whitespace().collect::<Vec<&str>>().join(" ");
    if collapsed.chars().count() <= 120 {
        return build_snippet(&collapsed);
    }
    let lower = collapsed.to_lowercase();
    if let Some(byte_pos) = lower.find(needle_lower) {
        let char_pos = lower[..byte_pos].chars().count();
        let start_char = char_pos.saturating_sub(30);
        let window: String = collapsed.chars().skip(start_char).take(120).collect();
        if start_char > 0 {
            return build_snippet(&format!("…{}", window.trim_start()));
        }
        return build_snippet(&window);
    }
    build_snippet(&collapsed)
}

/// Widens a sparse result set by grepping note bodies and extracted attachment text.
///
/// `results` is extended in place, so the title and tag hits the caller already collected
/// keep their place and nothing is listed twice. Every term has to appear somewhere in
/// the file; terms under two characters are too noisy to search for.
pub fn execute_fallback_search(
    vault_dir: &Path,
    title_terms: &[QueryTerm],
    tag_terms: &[&str],
    all_files: &[FileResult],
    results: &mut Vec<FileResult>,
    ocr_cache_path: &Path,
) {
    if results.len() < 50
        && !title_terms.is_empty()
        && title_terms.iter().all(|t| t.text.len() >= 2)
    {
        // Keyed by borrowed path and holding a borrowed file, so building the lookup costs
        // no clones at all. It spans the whole vault, which is what lets a content or
        // attachment hit resolve for a note the title/tag filter dropped.
        let file_lookup: HashMap<&str, &FileResult> = all_files
            .iter()
            .map(|res| (res.path.as_str(), res))
            .collect();

        let mut seen_paths: HashSet<String> = results.iter().map(|res| res.path.clone()).collect();

        // Read the extracted attachment text only here: no other code path needs it, and
        // it is the largest file in the cache directory
        let ocr_cache: OcrCache = fs::read_to_string(&ocr_cache_path)
            .ok()
            .and_then(|content| serde_json::from_str::<OcrCache>(&content).ok())
            .unwrap_or_default();

        let matchers = build_term_matchers(title_terms);

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
                if seen_paths.contains(&content_match.path) {
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
                                .all(|term| cached_file.tags.iter().any(|t| tag_matches(t, term)))
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
                seen_paths.insert(file_result.path.clone());
                results.push(file_result);
            }
        }

        // Collect only the attachments whose extracted text matches every query term,
        // keyed by lowercase vault-relative path -> `(is_pdf, snippet)`.
        let mut matched_by_rel: HashMap<String, (bool, String)> = HashMap::new();
        for (image_path, ocr_result) in &ocr_cache {
            if !ocr_text_matches_all(&ocr_result.text, title_terms) {
                continue;
            }

            let rel_path = match vault_rel_lower(vault_dir, Path::new(image_path)) {
                Some(rel) if !rel.is_empty() => rel,
                _ => continue,
            };

            // Prefer a single line containing every query term; if a quoted phrase
            // matched across a line wrap, build the snippet from the collapsed text;
            // otherwise fall back to the first line containing any query term.
            let single_line = ocr_result
                .text
                .lines()
                .map(|l| l.trim())
                .find(|l| {
                    let lower_line = l.to_lowercase();
                    title_terms.iter().all(|term| lower_line.contains(&term.text))
                });

            let snippet = if let Some(line) = single_line {
                build_snippet(line)
            } else if let Some(phrase) = title_terms.iter().find(|t| t.quoted && t.text.contains(' ')) {
                snippet_from_collapsed(&ocr_result.text, &phrase.text)
            } else {
                let any_line = ocr_result
                    .text
                    .lines()
                    .map(|l| l.trim())
                    .find(|l| {
                        let lower_line = l.to_lowercase();
                        title_terms.iter().any(|term| lower_line.contains(&term.text))
                    })
                    .unwrap_or("");
                build_snippet(any_line)
            };

            let is_pdf = Path::new(image_path)
                .extension()
                .and_then(|ext| ext.to_str())
                .map(|ext| ext.eq_ignore_ascii_case("pdf"))
                .unwrap_or(false);

            matched_by_rel.insert(rel_path, (is_pdf, snippet));
        }

        // An attachment is never shown on its own: only the Markdown note that embeds it
        // becomes an item, so Alfred always opens something editable.
        if !matched_by_rel.is_empty() {
            let mut all_attachments: HashSet<String> = HashSet::with_capacity(ocr_cache.len());
            let mut unique_basenames: HashMap<String, Option<String>> = HashMap::new();
            for image_path in ocr_cache.keys() {
                if let Some(rel) = vault_rel_lower(vault_dir, Path::new(image_path)) {
                    if !rel.is_empty() {
                        if let Some(base) = Path::new(&rel).file_name().and_then(|n| n.to_str()) {
                            unique_basenames
                                .entry(base.to_string())
                                .and_modify(|slot| *slot = None)
                                .or_insert_with(|| Some(rel.clone()));
                        }
                        all_attachments.insert(rel);
                    }
                }
            }

            for cached_file in all_files {
                if cached_file.links.is_empty() || seen_paths.contains(&cached_file.path) {
                    continue;
                }

                // An OCR hit still has to satisfy the tag filter the user typed
                if !tag_terms.is_empty()
                    && !tag_terms
                        .iter()
                        .all(|term| cached_file.tags.iter().any(|t| tag_matches(t, term)))
                {
                    continue;
                }

                let note_rel_dir = Path::new(&cached_file.path)
                    .parent()
                    .and_then(|parent| vault_rel_lower(vault_dir, parent))
                    .unwrap_or_default();

                for link in &cached_file.links {
                    if let Some(target_rel) = resolve_attachment_link(
                        &note_rel_dir,
                        link,
                        &all_attachments,
                        &unique_basenames,
                    ) {
                        if let Some((is_pdf, snippet_text)) = matched_by_rel.get(target_rel) {
                            let icon = if *is_pdf { "📄" } else { "🖼️" };
                            let mut file_result = cached_file.clone();
                            file_result.snippet = Some(format!("{} {}", icon, snippet_text));
                            seen_paths.insert(file_result.path.clone());
                            results.push(file_result);
                            break;
                        }
                    }
                }
            }
        }
    }
}

