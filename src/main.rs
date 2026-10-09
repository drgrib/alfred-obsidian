mod types;
mod ffi;
mod alfred;
mod cache;
mod obsidian;
mod ocr_pool;
mod search;
mod worker;

use alfred_workflow_rs::Item;
use std::collections::HashMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::alfred::*;
use crate::cache::*;
use crate::obsidian::{is_exact_route_key, resolve_vault_target, route_key, VaultTarget};
use crate::ocr_pool::run_ocr_shard;
use crate::search::*;
use crate::types::*;
use crate::worker::*;

/// One configured vault with every path the search needs for it.
struct Vault {
    key: String,
    clean_key: String,
    dir: PathBuf,
    state_path: PathBuf,
    cache_path: PathBuf,
    ocr_cache_path: PathBuf,
}

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

    // Every search spans every configured vault. The first tag that names a vault
    // (exactly, or as the parent of a nested tag such as `#corp/project`) decides only
    // where a new note is created.
    let mut target_key = "default";
    for term in &all_terms {
        if term.starts_with('#') {
            if let Some(key) = route_key(term, &vault_map) {
                target_key = key;
                break;
            }
        }
    }

    let cache_dir = get_workflow_cache_dir();

    // Default first, then the rest in a fixed order, so result lists and indexing
    // batches come out the same way every run
    let mut keys: Vec<&String> = vault_map.keys().collect();
    keys.sort_by(|a, b| (a.as_str() != "default").cmp(&(b.as_str() != "default")).then_with(|| a.cmp(b)));
    let vaults: Vec<Vault> = keys
        .iter()
        .map(|key| {
            let clean_key = key.replace("#", "");
            Vault {
                key: (*key).clone(),
                dir: PathBuf::from(expand_tilde(&vault_map[*key])),
                state_path: state_path_for(&cache_dir, key),
                cache_path: cache_dir.join(format!("vault_cache_{}.json", clean_key)),
                ocr_cache_path: cache_dir.join(format!("ocr_cache_{}.json", clean_key)),
                clean_key,
            }
        })
        .collect();
    let target_index = vaults.iter().position(|vault| vault.key == target_key).unwrap();

    // The create target has to exist; a missing secondary vault is skipped, not fatal
    if !vaults[target_index].dir.exists() {
        let output = AlfredOutput {
            rerun: None,
            items: vec![Item::new(format!("⚠️ Vault Path Not Found: {}", target_key))
                .set_subtitle(format!("Could not find directory at {}", vaults[target_index].dir.display()))
                .set_valid(false)],
        };
        println!("{}", serde_json::to_string(&output).unwrap());
        return;
    }

    // A live worker for any vault owns the UI: results would be incomplete while it
    // runs. Every state file in a batch carries the same combined progress, so whichever
    // one is found first is the one to show.
    for vault in &vaults {
        if let Some(output) = check_worker_status(&vault.state_path) {
            println!("{}", serde_json::to_string(&output).unwrap());
            return;
        }
    }

    // One cache per vault; `None` means that vault has never been indexed
    let mut caches: Vec<Option<VaultCache>> = vaults
        .iter()
        .map(|vault| if vault.dir.exists() { load_cache(&vault.cache_path) } else { None })
        .collect();

    // Vaults that need the background worker, with the number of files it will read
    let mut pending: Vec<(String, usize)> = Vec::new();

    if query.is_empty() {
        // Empty query: reconcile every vault's caches with its directory so the list is
        // never stale. A typed query never pays for the directory walk, it just searches
        // what is cached. Everything that needs the worker goes into one batch with one
        // combined total, so the countdown the user watches ends when all of it is done.
        for (index, vault) in vaults.iter().enumerate() {
            if !vault.dir.exists() {
                continue;
            }
            match caches[index].as_mut() {
                None => {
                    // Cold start: the walk against an empty cache counts every file, so
                    // the batch total is right from the first frame
                    let scan = scan_vault(&vault.dir, &[], &OcrCache::default());
                    pending.push((vault.key.clone(), scan.dirty_notes.len() + scan.dirty_images.len()));
                }
                Some(cached_data) => {
                    let mut ocr_cache: OcrCache = fs::read_to_string(&vault.ocr_cache_path)
                        .ok()
                        .and_then(|content| serde_json::from_str::<OcrCache>(&content).ok())
                        .unwrap_or_default();

                    let scan = scan_vault(&vault.dir, &cached_data.files, &ocr_cache);
                    let dirty_count = scan.dirty_notes.len() + scan.dirty_images.len();

                    if dirty_count > DIRTY_FILE_THRESHOLD || scan.dirty_images.len() > DIRTY_IMAGE_THRESHOLD {
                        // Too much work to finish while the user waits: hand it to the
                        // worker. Attachments get their own limit because each image or
                        // scanned PDF page costs a Vision pass.
                        pending.push((vault.key.clone(), dirty_count));
                    } else if dirty_count > 0 || scan.has_deleted {
                        reconcile_inline_cache(&vault.dir, cached_data, &mut ocr_cache, &scan, &vault.cache_path, &vault.ocr_cache_path);
                    }
                }
            }
        }
    } else {
        // A vault with no cache at all is indexed now rather than waiting for the next
        // empty query, since its notes would otherwise be missing from this search
        for (index, vault) in vaults.iter().enumerate() {
            if vault.dir.exists() && caches[index].is_none() {
                pending.push((vault.key.clone(), 0));
            }
        }
    }

    if !pending.is_empty() {
        if let Some(output) = start_worker_with_feedback(&pending, &cache_dir) {
            println!("{}", serde_json::to_string(&output).unwrap());
            return;
        }
    }

    // Only reachable with no cache for the create target if the worker could not be
    // spawned; there is nothing to search in that case, so the failure is reported
    if caches[target_index].is_none() {
        let output = AlfredOutput {
            rerun: None,
            items: vec![Item::new("Could not start indexer")
                .set_subtitle(format!("The background worker for the {} vault failed to launch", vaults[target_index].clean_key))
                .set_valid(false)],
        };
        println!("{}", serde_json::to_string(&output).unwrap());
        return;
    }

    // Per-vault note lists, each already sorted by modified descending and never mutated,
    // so the fallback search can resolve a hit in any note, not just filtered ones. Tag
    // recency is merged across vaults, keeping the newest sighting of each tag.
    let mut vault_files: Vec<Vec<FileResult>> = Vec::with_capacity(vaults.len());
    let mut tag_recency: HashMap<String, SystemTime> = HashMap::new();
    for cache in caches {
        match cache {
            Some(cache) => {
                for (tag, time) in cache.tag_recency {
                    let entry = tag_recency.entry(tag).or_insert(time);
                    if time > *entry {
                        *entry = time;
                    }
                }
                vault_files.push(cache.files);
            }
            None => vault_files.push(Vec::new()),
        }
    }
    let mut items = Vec::new();

    // Tag Autocomplete Mode
    //
    // The routing keys are offered even when no note carries them as a tag, so a vault
    // is always reachable through autocomplete; a key that is also a real tag keeps its
    // real recency.
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

    // An exact routing key only selects the create vault; a nested child of one is kept
    // as a real tag because it says more than which vault to use
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

    // Checked against the whole create vault, and only when a create item will be shown
    let is_duplicate = allow_create
        && !is_empty_search
        && vault_files[target_index]
            .iter()
            .any(|res| res.title.eq_ignore_ascii_case(&title_string));

    // Every hit carries the index of the vault it came from, so its row can be built
    // against the right directory and Advanced URI target
    let mut results: Vec<(usize, FileResult)> = Vec::new();

    if !is_create_only {
        for (index, files) in vault_files.iter().enumerate() {
            // Built from the full list rather than by narrowing it, so the fallback below
            // can still add content and attachment hits for notes the filter dropped
            let mut vault_results: Vec<FileResult> = if is_empty_search {
                // Already sorted by modified descending, so the newest notes are the first 50
                files.iter().take(50).cloned().collect()
            } else {
                files
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

            // Full-text fallback: when title/tag matches are sparse, grep the note bodies
            // so notes that merely mention the query still surface. Every term has to
            // appear somewhere in the file; terms under two characters are too noisy.
            execute_fallback_search(&vaults[index].dir, &title_terms, &tag_terms, files, &mut vault_results, &vaults[index].ocr_cache_path);

            results.extend(vault_results.into_iter().map(|res| (index, res)));
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

    if is_empty_search {
        // Each vault contributed its newest 50; merge them into the newest 50 overall
        results.sort_by(|a, b| b.1.modified.cmp(&a.1.modified));
    } else {
        results.sort_by(|a, b| {
            rank_result(&a.1)
                .cmp(&rank_result(&b.1))
                .then_with(|| b.1.modified.cmp(&a.1.modified))
        });
    }
    results.truncate(50);

    // Addressed by Obsidian's own vault ID when the directory is registered, so a
    // routed note cannot silently land in whichever vault happens to be active
    let targets: Vec<VaultTarget> = vaults
        .iter()
        .map(|vault| vault_target_for(&vault.dir, &vault.clean_key))
        .collect();

    // The vault name only adds useful context when more than one vault is configured
    let has_multiple_vaults = vault_map.len() > 1;

    if !is_create_only {
        for (index, res) in &results {
            items.extend(assemble_alfred_items(
                std::slice::from_ref(res),
                &vaults[*index].dir,
                &targets[*index],
                &title_terms,
                has_multiple_vaults,
            ));
        }
    }

    // Offer to open the existing note, or create a new one with the tags written into the body
    if allow_create && !is_empty_search {
        let create_item = build_create_item(&title_string, &tag_terms, &targets[target_index], is_duplicate, has_multiple_vaults);

        if !is_create_only && items.len() >= 2 {
            items.insert(2, create_item);
        } else {
            items.push(create_item);
        }
    }

    if items.is_empty() {
        let scope = if has_multiple_vaults { "all vaults".to_string() } else { format!("the {} vault", target_key) };
        if is_create_only && is_empty_search {
            items.push(
                Item::new("Create a new note")
                    .set_subtitle("Enter a title or #tags")
                    .set_valid(false)
            );
        } else if is_empty_search {
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
                    .set_subtitle(format!("Searched {} for '{}'", scope, all_search_terms.join(" ")))
                    .set_valid(false)
            );
        }
    }

    let output = AlfredOutput { rerun: None, items };
    println!("{}", serde_json::to_string(&output).unwrap());
}
