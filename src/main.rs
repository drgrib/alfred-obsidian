mod types;
mod ffi;
mod alfred;
mod cache;
mod obsidian;
mod ocr_pool;
mod search;
mod worker;

use alfred_workflow_rs::Item;
use std::env;
use std::fs;
use std::path::Path;
use std::time::SystemTime;

use crate::alfred::*;
use crate::cache::*;
use crate::obsidian::{is_exact_route_key, resolve_vault_target, route_key, VaultTarget};
use crate::ocr_pool::run_ocr_shard;
use crate::search::*;
use crate::types::*;
use crate::worker::*;

/// Resolves the URI target for a vault directory, falling back to the configured key
/// for both the label and the `vault=` parameter when Obsidian has not registered it.
fn vault_target_for(vault_dir: &Path, clean_key: &str) -> VaultTarget {
    let mut target = resolve_vault_target(vault_dir);
    if target.label.is_empty() {
        target.label = clean_key.to_string();
    }
    if target.uri_vault.is_empty() {
        target.uri_vault = clean_key.to_string();
    }
    target
}

fn main() {
    let args: Vec<String> = env::args().collect();

    // A shard needs no configuration at all: it is fed paths over stdin by the pool
    if args.len() >= 2 && args[1] == "ocr-shard" {
        run_ocr_shard();
        return;
    }
    
    // Every argument after "worker" is a vault key; they are indexed as one batch
    if args.len() >= 3 && args[1] == "worker" {
        run_worker(&args[2..]);
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

    // The first tag that names a vault (exactly, or as the parent of a nested tag such
    // as `#corp/project`) decides where the search and any new note go
    let mut target_key = "default";
    for term in &all_terms {
        if term.starts_with('#') {
            if let Some(key) = route_key(term, &vault_map) {
                target_key = key;
                break;
            }
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
    let state_path = state_path_for(&cache_dir, target_key);
    let cache_path = cache_dir.join(format!("vault_cache_{}.json", clean_key));
    let ocr_cache_path = cache_dir.join(format!("ocr_cache_{}.json", clean_key));

    // Other vaults in a fixed order, so the batch is assembled the same way every time
    let mut other_keys: Vec<&String> = vault_map.keys().filter(|key| key.as_str() != target_key).collect();
    other_keys.sort();

    if let Some(output) = check_worker_status(&state_path) {
        println!("{}", serde_json::to_string(&output).unwrap());
        return;
    }

    // On the empty query a worker for any vault owns the UI. Every state file in a batch
    // carries the same combined progress, so whichever one is found first is the one.
    if query.is_empty() {
        for key in &other_keys {
            if let Some(output) = check_worker_status(&state_path_for(&cache_dir, key)) {
                println!("{}", serde_json::to_string(&output).unwrap());
                return;
            }
        }
    }

    let mut cached_data_opt = None;
    if cache_path.exists() {
        cached_data_opt = load_cache(&cache_path);
    }

    if query.is_empty() {
        // Empty query: reconcile the caches with the vault so the list is never stale.
        // A typed query never pays for the directory walk, it just searches what is cached.
        //
        // Every configured vault is planned here, not just the one being displayed. A
        // query that routes to a non-default vault always carries its `#key`, so it is
        // never empty, and this is the only point where that vault's cache would be
        // brought up to date. A user who moves notes between vaults should not have to
        // know to type the key. Everything that needs the worker goes into one batch with
        // one combined total, so the countdown the user watches ends when all of it is done.
        let mut pending: Vec<(String, usize)> = Vec::new();

        let mut ocr_cache: OcrCache = fs::read_to_string(&ocr_cache_path)
            .ok()
            .and_then(|content| serde_json::from_str::<OcrCache>(&content).ok())
            .unwrap_or_default();

        match cached_data_opt.as_mut() {
            None => {
                // Cold start: the walk against an empty cache counts every file, so the
                // batch total is right from the first frame
                let scan = scan_vault(vault_dir, &[], &OcrCache::default());
                pending.push((target_key.to_string(), scan.dirty_notes.len() + scan.dirty_images.len()));
            }
            Some(cached_data) => {
                let scan = scan_vault(vault_dir, &cached_data.files, &ocr_cache);
                let dirty_count = scan.dirty_notes.len() + scan.dirty_images.len();

                if dirty_count > DIRTY_FILE_THRESHOLD || scan.dirty_images.len() > DIRTY_IMAGE_THRESHOLD {
                    // Too much work to finish while the user waits: hand it to the worker.
                    // Attachments get their own limit because each image or scanned PDF
                    // page costs a Vision pass.
                    pending.push((target_key.to_string(), dirty_count));
                } else if dirty_count > 0 || scan.has_deleted {
                    reconcile_inline_cache(vault_dir, cached_data, &mut ocr_cache, &scan, &cache_path, &ocr_cache_path);
                }
            }
        }

        for key in &other_keys {
            if let Some(count) = plan_vault_refresh(key, &vault_map[*key], &cache_dir) {
                pending.push(((*key).clone(), count));
            }
        }

        if !pending.is_empty() {
            if let Some(output) = start_worker_with_feedback(&pending, &cache_dir) {
                println!("{}", serde_json::to_string(&output).unwrap());
                return;
            }
        }
    } else if cached_data_opt.is_none() {
        // Cold start reached through a typed query: index just this vault now rather
        // than waiting for the next empty query to batch it
        if let Some(output) = start_worker_with_feedback(&[(target_key.to_string(), 0)], &cache_dir) {
            println!("{}", serde_json::to_string(&output).unwrap());
            return;
        }
    }

    // Only reachable with no cache if the worker could not be spawned; there is nothing
    // to search in that case, so the failure is reported rather than panicking
    if cached_data_opt.is_none() {
        let output = AlfredOutput {
            rerun: None,
            items: vec![Item::new("Could not start indexer")
                .set_subtitle(format!("The background worker for the {} vault failed to launch", clean_key))
                .set_valid(false)],
        };
        println!("{}", serde_json::to_string(&output).unwrap());
        return;
    }

    let cached_data = cached_data_opt.unwrap();
    // The full cached list, already sorted by modified descending. It is never mutated, so
    // the fallback search below can resolve a hit in any note, not just filtered ones.
    let all_files = cached_data.files;
    let mut tag_recency = cached_data.tag_recency;
    let mut items = Vec::new();

    // Tag Autocomplete Mode
    //
    // A half-typed routing key (`#corp-g`) still routes to the default vault, whose cache
    // legitimately has no `corp-google` notes once they live in their own vault. The keys
    // are offered regardless so the vault is always reachable through autocomplete; a key
    // the current vault actually uses as a tag keeps its real recency.
    for key in vault_map.keys().filter(|key| key.starts_with('#')) {
        tag_recency
            .entry(key.trim_start_matches('#').to_lowercase())
            .or_insert(SystemTime::now());
    }
    if let Some(output) = handle_tag_autocomplete(raw_query, &all_terms, &tag_recency) {
        println!("{}", serde_json::to_string(&output).unwrap());
        return;
    }

    // Normal Search Mode
    let title_terms: Vec<&str> = all_terms.iter()
        .filter(|t| !t.starts_with('#'))
        .copied()
        .collect();
        
    // An exact routing key only selects the vault; a nested child of one is kept as a
    // real tag because it says more than which vault to use
    let tag_terms: Vec<&str> = all_terms.iter()
        .filter(|t| t.starts_with('#') && !is_exact_route_key(t, &vault_map))
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

    // Addressed by Obsidian's own vault ID when the directory is registered, so a
    // routed note cannot silently land in whichever vault happens to be active
    let target = vault_target_for(vault_dir, &clean_key);

    // The vault name only adds useful context when more than one vault is configured
    let has_multiple_vaults = vault_map.len() > 1;

    if !is_create_only {
        if query.is_empty() && has_multiple_vaults {
            // The empty query is the one view that spans vaults: the newest notes from
            // every configured vault, merged by modification time. Each row is built
            // against its own vault so the location label and the Advanced URI are right.
            // A vault with no cache yet is skipped; it is already in the indexing batch.
            let mut vaults: Vec<(std::path::PathBuf, VaultTarget, Vec<FileResult>)> = Vec::new();
            vaults.push((vault_dir.to_path_buf(), target.clone(), results));
            for key in &other_keys {
                let other_dir = std::path::PathBuf::from(expand_tilde(&vault_map[*key]));
                let other_clean = key.replace("#", "");
                let other_cache = cache_dir.join(format!("vault_cache_{}.json", other_clean));
                if let Some(cache) = load_cache(&other_cache) {
                    let newest: Vec<FileResult> = cache.files.into_iter().take(50).collect();
                    vaults.push((other_dir.clone(), vault_target_for(&other_dir, &other_clean), newest));
                }
            }

            // (modified, vault index, position within that vault's list)
            let mut order: Vec<(SystemTime, usize, usize)> = Vec::new();
            for (vault_index, (_, _, files)) in vaults.iter().enumerate() {
                for (position, res) in files.iter().enumerate() {
                    order.push((res.modified, vault_index, position));
                }
            }
            order.sort_by(|a, b| b.0.cmp(&a.0));
            order.truncate(50);

            for (_, vault_index, position) in order {
                let (dir, vault_target, files) = &vaults[vault_index];
                items.extend(assemble_alfred_items(
                    std::slice::from_ref(&files[position]),
                    dir,
                    vault_target,
                    &title_terms,
                    has_multiple_vaults,
                ));
            }
        } else {
            items.extend(assemble_alfred_items(&results, vault_dir, &target, &title_terms, has_multiple_vaults));
        }
    }

    // Offer to open the existing note, or create a new one with the tags written into the body
    if allow_create && !is_empty_search {
        let create_item = build_create_item(&title_string, &tag_terms, &target, is_duplicate, has_multiple_vaults);

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
            let scope = if query.is_empty() && has_multiple_vaults { "any vault".to_string() } else { format!("{} vault", target_key) };
            items.push(
                Item::new("No matches found")
                    .set_subtitle(format!("No markdown files found in {}", scope))
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