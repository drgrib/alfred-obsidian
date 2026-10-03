use alfred_workflow_rs::Item;
use serde::Serialize;
use std::collections::HashMap;
use std::env;
use std::fs;
use std::path::Path;
use std::time::SystemTime;

#[derive(Serialize)]
struct AlfredOutput {
    items: Vec<Item>,
}

// Struct to hold file info so we can sort by modified time
struct FileResult {
    title: String,
    path: String,
    modified: SystemTime,
}

fn expand_tilde(path: &str) -> String {
    if path.starts_with("~/") {
        if let Ok(home) = env::var("HOME") {
            return path.replacen("~/", &format!("{}/", home), 1);
        }
    }
    path.to_string()
}

// Now returns results in a mutable vector instead of formatting Items immediately
fn search_vault(dir: &Path, terms: &[&str], results: &mut Vec<FileResult>) {
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            
            if path.is_dir() {
                // Ignore hidden directories
                if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                    if !name.starts_with('.') {
                        search_vault(&path, terms, results);
                    }
                }
            } else if path.extension().and_then(|e| e.to_str()) == Some("md") {
                if let Some(stem) = path.file_stem().and_then(|n| n.to_str()) {
                    let lower_stem = stem.to_lowercase();
                    
                    // If terms are empty, it automatically matches and collects everything
                    if terms.is_empty() || terms.iter().all(|term| lower_stem.contains(*term)) {
                        // Grab the modified time for sorting
                        let modified = fs::metadata(&path)
                            .and_then(|m| m.modified())
                            .unwrap_or(SystemTime::UNIX_EPOCH);

                        results.push(FileResult {
                            title: stem.to_string(),
                            path: path.to_string_lossy().into_owned(),
                            modified,
                        });
                    }
                }
            }
        }
    }
}

fn main() {
    let vault_map_env = env::var("vault_map").unwrap_or_else(|_| "".to_string());
    let mut vault_map: HashMap<String, String> = HashMap::new();

    for line in vault_map_env.lines() {
        if let Some((key, path)) = line.split_once(':') {
            let clean_key = key.trim().to_string();
            let clean_path = path.trim().to_string();
            if !clean_key.is_empty() && !clean_path.is_empty() {
                vault_map.insert(clean_key, clean_path);
            }
        }
    }

    let mut items = Vec::new();

    if !vault_map.contains_key("default") {
        items.push(
            Item::new("Missing 'default' Vault")
                .set_subtitle("Your configuration must include a 'default:' path.")
                .set_valid(false)
        );
        println!("{}", serde_json::to_string(&AlfredOutput { items }).unwrap());
        return;
    }

    let query = env::args().nth(1).unwrap_or_default();
    let lower_query = query.to_lowercase();
    let all_terms: Vec<&str> = lower_query.split_whitespace().collect();

    let mut target_key = "default";
    for term in &all_terms {
        if term.starts_with('#') && vault_map.contains_key(*term) {
            target_key = term;
            break;
        }
    }

    let search_terms: Vec<&str> = all_terms.into_iter().filter(|t| !t.starts_with('#')).collect();
    let is_empty_search = search_terms.is_empty();

    let target_path = vault_map.get(target_key).unwrap();
    let expanded_path = expand_tilde(target_path);
    let vault_dir = Path::new(&expanded_path);

    if !vault_dir.exists() {
        items.push(
            Item::new(format!("⚠️ Vault Path Not Found: {}", target_key))
                .set_subtitle(format!("Could not find directory at {}", expanded_path))
                .set_valid(false)
        );
    } else {
        let mut results = Vec::new();
        search_vault(vault_dir, &search_terms, &mut results);

        // Sort by modified time descending (newest first)
        results.sort_by(|a, b| b.modified.cmp(&a.modified));

        // If it's an empty search, cap the results at 20 to prevent Alfred UI lag
        if is_empty_search {
            results.truncate(20);
        }

        // Convert the FileResults into Alfred Items
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
            
            items.push(
                Item::new("No matches found")
                    .set_subtitle(msg)
                    .set_valid(false)
            );
        }
    }

    let output = AlfredOutput { items };
    println!("{}", serde_json::to_string(&output).unwrap());
}