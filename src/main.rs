use alfred_workflow_rs::Item;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::env;
use std::fs;
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

                    results.push(FileResult {
                        title: stem.to_string(),
                        path: path.to_string_lossy().into_owned(),
                        modified,
                    });

                    state.progress += 1;
                    // Write to disk every 250 files to update Alfred without severe disk thrashing
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

    // Initial state
    let mut state = State { progress: 0, total: 0, status: "Counting files...".to_string() };
    fs::write(&state_path, serde_json::to_string(&state).unwrap()).ok();

    // Count phase
    state.total = count_files(vault_dir);
    state.status = "Indexing vault...".to_string();
    fs::write(&state_path, serde_json::to_string(&state).unwrap()).ok();

    // Process phase
    let mut results = Vec::new();
    process_files(vault_dir, &mut results, &mut state, &state_path);

    // Sort newest first
    results.sort_by(|a, b| b.modified.cmp(&a.modified));

    // Save final cache and delete state
    if let Ok(json) = serde_json::to_string(&VaultCache { files: results }) {
        fs::write(&cache_path, json).ok();
    }
    fs::remove_file(&state_path).ok();
}

fn main() {
    let args: Vec<String> = env::args().collect();
    
    // Check if being called as the background worker
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

    // Extract search query correctly handling "$1"
    let query = args.get(1).map(|s| s.trim()).unwrap_or("");
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

    // 1. Check if the worker is actively running
    if state_path.exists() {
        let data = fs::read_to_string(&state_path).unwrap_or_default();
        let state = serde_json::from_str::<State>(&data).unwrap_or(State { progress: 0, total: 0, status: "Initializing...".to_string() });
        
        let percentage = if state.total > 0 {
            (state.progress as f32 / state.total as f32) * 100.0
        } else {
            0.0
        };

        let output = AlfredOutput {
            rerun: Some(0.2), // Refresh UI every 0.2s
            items: vec![
                Item::new(format!("{} {:.0}%", state.status, percentage))
                    .set_subtitle(format!("{} of {} files parsed. Please wait...", state.progress, state.total))
                    .set_valid(false) // Blocks the Enter key
            ]
        };
        println!("{}", serde_json::to_string(&output).unwrap());
        return;
    }

    // 2. Check if cache needs to be built
    if !cache_path.exists() {
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
                    .set_subtitle("Initializing background worker. Please wait...")
                    .set_valid(false)
            ]
        };
        println!("{}", serde_json::to_string(&output).unwrap());
        return;
    }

    // 3. Cache exists, load and filter it
    let mut items = Vec::new();
    let search_terms: Vec<&str> = all_terms.into_iter().filter(|t| !t.starts_with('#')).collect();
    let is_empty_search = search_terms.is_empty();

    if let Ok(file_content) = fs::read_to_string(&cache_path) {
        if let Ok(cached_data) = serde_json::from_str::<VaultCache>(&file_content) {
            let mut results = cached_data.files;

            if !is_empty_search {
                results.retain(|res| {
                    let lower_stem = res.title.to_lowercase();
                    search_terms.iter().all(|term| lower_stem.contains(*term))
                });
            }

            if is_empty_search {
                results.truncate(20);
            }

            for res in results {
                items.push(
                    Item::new(res.title)
                        .set_subtitle(res.path.clone())
                        .set_arg(res.path)
                        .set_valid(true)
                );
            }

            if items.is_empty() {
                let msg = if is_empty_search {
                    format!("No markdown files found in {} vault", target_key)
                } else {
                    format!("Searched in {} vault for '{}'", target_key, search_terms.join(" "))
                };
                items.push(Item::new("No matches found").set_subtitle(msg).set_valid(false));
            }
        }
    }

    let output = AlfredOutput { rerun: None, items };
    println!("{}", serde_json::to_string(&output).unwrap());
}