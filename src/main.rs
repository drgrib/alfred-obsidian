mod types;
mod ffi;
mod alfred;
mod cache;
mod ocr_pool;
mod search;
mod worker;

use alfred_workflow_rs::Item;
use std::env;
use std::fs;
use std::path::Path;

use crate::alfred::*;
use crate::cache::*;
use crate::ocr_pool::run_ocr_shard;
use crate::search::*;
use crate::types::*;
use crate::worker::*;

fn main() {
    let args: Vec<String> = env::args().collect();

    // A shard needs no configuration at all: it is fed paths over stdin by the pool
    if args.len() >= 2 && args[1] == "ocr-shard" {
        run_ocr_shard();
        return;
    }
    
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

    if let Some(output) = check_worker_status(&state_path) {
        println!("{}", serde_json::to_string(&output).unwrap());
        return;
    }

    let mut cached_data_opt = None;
    if cache_path.exists() {
        cached_data_opt = load_cache(&cache_path);
    }

    // Cold start: no cache exists at all
    if cached_data_opt.is_none() {
        write_json_atomic(&state_path, &State { progress: 0, total: 0, status: STATUS_SCANNING.to_string(), eta_secs: None, worker_pid: None });

        let spawned = spawn_worker(target_key);

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
                // Too much work to finish while the user waits: hand it to the worker.
                // Attachments get their own limit because each image or scanned PDF page
                // costs a Vision pass.
                let state = State { progress: 0, total: dirty_count as u32, status: STATUS_SCANNING.to_string(), eta_secs: None, worker_pid: None };
                write_json_atomic(&state_path, &state);

                let spawned = spawn_worker(target_key);

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
                reconcile_inline_cache(vault_dir, &mut cached_data, &mut ocr_cache, &scan, &cache_path, &ocr_cache_path);
            }

            cached_data_opt = Some(cached_data);
        }
    }

    let cached_data = cached_data_opt.unwrap();
    // The full cached list, already sorted by modified descending. It is never mutated, so
    // the fallback search below can resolve a hit in any note, not just filtered ones.
    let all_files = cached_data.files;
    let tag_recency = cached_data.tag_recency;
    let mut items = Vec::new();

    // Tag Autocomplete Mode
    if let Some(output) = handle_tag_autocomplete(raw_query, &all_terms, &tag_recency) {
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
    // extended with content and attachment hits for notes the filter dropped
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
    if !is_create_only {
        execute_fallback_search(vault_dir, &title_terms, &tag_terms, &all_files, &mut results, &ocr_cache_path);
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
        items.extend(assemble_alfred_items(&results, vault_dir, &vault_name, &title_terms, has_multiple_vaults));
    }

    // Offer to open the existing note, or create a new one with the tags written into the body
    if allow_create && !is_empty_search {
        let create_item = build_create_item(&title_string, &tag_terms, &vault_name, is_duplicate, has_multiple_vaults);

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