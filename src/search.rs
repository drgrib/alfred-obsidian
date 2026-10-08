use grep_regex::{RegexMatcher, RegexMatcherBuilder};
use grep_searcher::sinks::UTF8;
use grep_searcher::Searcher;
use ignore::{
    DirEntry, Error as IgnoreError, ParallelVisitor, ParallelVisitorBuilder, WalkBuilder, WalkState,
};
use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use crate::types::{ContentMatch, FileResult, OcrCache, MAX_CONTENT_MATCHES};

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
/// Each term is compiled as a regex first so patterns like `foo.*bar` work, but
/// most terms are plain words that may contain characters which are invalid regex
/// (e.g. `c++`), so those fall back to a literal, escaped search. A file only
/// counts as a content match when every term matches somewhere inside it.
pub fn build_term_matchers(terms: &[&str]) -> Vec<RegexMatcher> {
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

/// Widens a sparse result set by grepping note bodies and extracted attachment text.
///
/// `results` is extended in place, so the title and tag hits the caller already collected
/// keep their place and nothing is listed twice. Every term has to appear somewhere in
/// the file; terms under two characters are too noisy to search for.
pub fn execute_fallback_search(
    vault_dir: &Path,
    title_terms: &[&str],
    tag_terms: &[&str],
    all_files: &[FileResult],
    results: &mut Vec<FileResult>,
    ocr_cache_path: &Path,
) {
    if results.len() < 50 && !title_terms.is_empty() && title_terms.iter().all(|t| t.len() >= 2) {
        // Keyed by borrowed path and holding a borrowed file, so building the lookup costs
        // no clones at all. It spans the whole vault, which is what lets a content or
        // attachment hit resolve for a note the title/tag filter dropped.
        let file_lookup: HashMap<&str, &FileResult> = all_files
            .iter()
            .map(|res| (res.path.as_str(), res))
            .collect();

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

        // An attachment is never shown on its own: only the Markdown note that embeds it
        // becomes an item, so Alfred always opens something editable. This covers image
        // embeds and PDF attachments alike, since both are indexed the same way.
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

                // A PDF reads as a document, anything else as an image
                let is_pdf = Path::new(image_path)
                    .extension()
                    .and_then(|ext| ext.to_str())
                    .map(|ext| ext.eq_ignore_ascii_case("pdf"))
                    .unwrap_or(false);
                let icon = if is_pdf { "📄" } else { "🖼️" };

                let mut file_result = cached_file.clone();
                file_result.snippet = Some(format!("{} {}", icon, build_snippet(matching_line)));
                results.push(file_result);
            }
        }
    }
}

