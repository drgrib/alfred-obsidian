use alfred_workflow_rs::Item;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::env;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::SystemTime;

#[derive(Serialize)]
struct AlfredOutput {
    #[serde(skip_serializing_if = "Option::is_none")]
    rerun: Option<f32>,
    items: Vec<Item>,
}

#[derive(Serialize, Deserialize, Clone)]
struct FileResult {
    title: String,
    path: String,
    modified: SystemTime,
    tags: Vec<String>,
}

#[derive(Serialize, Deserialize)]
struct VaultCache {
    files: Vec<FileResult>,
}

#[derive(Serialize, Deserialize)]
struct State {
    progress: u32,
    total: u32,
    status: String,
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

fn process_files(dir: &Path, results: &mut Vec<FileResult>, state: &mut State, state_path: &Path) {
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                    if !name.starts_with('.') {
                        process_files(&path, results, state, state_path);
                    }
                }
            } else if path.extension().and_then(|e| e.to_str()) == Some("md") {
                if let Some(stem) = path.file_stem().and_then(|n| n.to_str()) {
                    let modified = fs::metadata(&path)
                        .and_then(|m| m.modified())
                        .unwrap_or(SystemTime::UNIX_EPOCH);

                    let tags = extract_tags(&path);

                    results.push(FileResult {
                        title: stem.to_string(),
                        path: path.to_string_lossy().into_owned(),
                        modified,
                        tags,
                    });

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

    let mut results = Vec::new();
    process_files(vault_dir, &mut results, &mut state, &state_path);

    results.sort_by(|a, b| b.modified.cmp(&a.modified));

    if let Ok(json) = serde_json::to_string(&VaultCache { files: results }) {
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

    // Preserve the raw query to check for trailing spaces
    let raw_query = args.get(1).map(|s| s.as_str()).unwrap_or("");
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

    if state_path.exists() {
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

    let mut cached_data_opt = None;
    if cache_path.exists() {
        if let Ok(file_content) = fs::read_to_string(&cache_path) {
            if let Ok(parsed_data) = serde_json::from_str::<VaultCache>(&file_content) {
                cached_data_opt = Some(parsed_data);
            } else {
                fs::remove_file(&cache_path).ok();
            }
        } else {
            fs::remove_file(&cache_path).ok();
        }
    }

    if cached_data_opt.is_none() {
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

    let mut results = cached_data_opt.unwrap().files;
    let mut items = Vec::new();

    let ends_with_space = raw_query.ends_with(' ');
    let last_term = all_terms.last().copied().unwrap_or("");
    let is_autocompleting_tag = !ends_with_space && last_term.starts_with('#');

    // Tag Autocomplete Mode
    if is_autocompleting_tag {
        let partial_tag = last_term.trim_start_matches('#');
        let mut tag_counts: HashMap<&String, u32> = HashMap::new();
        
        for res in &results {
            for tag in &res.tags {
                *tag_counts.entry(tag).or_insert(0) += 1;
            }
        }
        
        let mut matched_tags: Vec<(&String, u32)> = tag_counts.into_iter()
            .filter(|(t, _)| t.contains(partial_tag))
            .collect();
            
        matched_tags.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));
        
        let prefix = if all_terms.len() > 1 {
            let terms_before = &all_terms[..all_terms.len() - 1];
            format!("{} ", terms_before.join(" "))
        } else {
            "".to_string()
        };

        // Inject vault routing tags into autocomplete
        let mut matched_vaults: Vec<&String> = vault_map.keys()
            .filter(|k| k.starts_with('#') && k.trim_start_matches('#').contains(partial_tag))
            .collect();
        matched_vaults.sort();

        for vault_key in matched_vaults {
            items.push(
                Item::new(vault_key.clone())
                    .set_subtitle("Route search to this vault")
                    .set_autocomplete(format!("{}{}", prefix, vault_key))
                    .set_valid(false)
            );
        }

        for (tag, count) in matched_tags.into_iter().take(30) {
            items.push(
                Item::new(format!("#{}", tag))
                    .set_subtitle(format!("{} notes", count))
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

    if !is_empty_search {
        results.retain(|res| {
            let lower_title = res.title.to_lowercase();
            let matches_title = title_terms.iter().all(|term| lower_title.contains(*term));
            let matches_tags = tag_terms.iter().all(|term| res.tags.iter().any(|t| t == *term));
            
            matches_title && matches_tags
        });
    }

    if is_empty_search {
        results.truncate(20);
    } else {
        results.truncate(50);
    }

    for res in results {
        let subtitle = if res.tags.is_empty() {
            res.path.clone()
        } else {
            format!("{} | #{}", res.path, res.tags.join(" #"))
        };

        let item = Item::new(res.title)
            .set_subtitle(subtitle)
            .set_arg(res.path)
            .set_valid(true);
        
        items.push(item);
    }

    if items.is_empty() {
        let msg = if is_empty_search {
            format!("No markdown files found in {} vault", target_key)
        } else {
            let mut all_search_terms = title_terms;
            all_search_terms.extend(tag_terms.iter().map(|t| *t));
            format!("Searched in {} vault for '{}'", target_key, all_search_terms.join(" "))
        };
        items.push(Item::new("No matches found").set_subtitle(msg).set_valid(false));
    }

    let output = AlfredOutput { rerun: None, items };
    println!("{}", serde_json::to_string(&output).unwrap());
}