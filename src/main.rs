use alfred_workflow_rs::Item;
use serde::Serialize;
use std::collections::HashMap;
use std::env;
use std::path::Path;

#[derive(Serialize)]
struct AlfredOutput {
    items: Vec<Item>,
}

fn expand_tilde(path: &str) -> String {
    if path.starts_with("~/") {
        if let Ok(home) = env::var("HOME") {
            return path.replacen("~/", &format!("{}/", home), 1);
        }
    }
    path.to_string()
}

fn path_exists(path: &str) -> bool {
    let expanded = expand_tilde(path);
    Path::new(&expanded).exists()
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

    if vault_map.is_empty() {
        items.push(
            Item::new("Vault Map Not Configured")
                .set_subtitle("Please click 'Configure Workflow...' to set your vault rules.")
                .set_valid(false)
        );
    } else if !vault_map.contains_key("default") {
         items.push(
            Item::new("Missing 'default' Vault")
                .set_subtitle("Your configuration must include a 'default:' path.")
                .set_valid(false)
        );
    } else {
        for (key, path) in &vault_map {
            let title = if key == "default" {
                "Default Vault".to_string()
            } else {
                format!("Routed Vault: {}", key)
            };

            let (subtitle, valid, icon) = if path_exists(path) {
                (path.clone(), true, None)
            } else {
                (format!("Path not found: {}", path), false, Some("⚠️"))
            };

            let mut item = Item::new(title.clone())
                .set_subtitle(subtitle)
                .set_valid(valid);
            
            if let Some(i) = icon {
                 item = Item::new(format!("{} {}", i, title))
                    .set_subtitle(format!("Path not found: {}", path))
                    .set_valid(false);
            }

            items.push(item);
        }
    }

    let output = AlfredOutput { items };
    println!("{}", serde_json::to_string(&output).unwrap());
}