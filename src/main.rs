use alfred_workflow_rs::Item;
use serde::Serialize;
use std::collections::HashMap;
use std::env;

#[derive(Serialize)]
struct AlfredOutput {
    items: Vec<Item>,
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
    } else {
        items.push(
            Item::new("Vault Map Successfully Parsed")
                .set_subtitle(format!("Found {} routing rules.", vault_map.len()))
                .set_valid(false)
        );
        
        for (key, path) in &vault_map {
            items.push(
                Item::new(key.clone())
                    .set_subtitle(path.clone())
                    .set_valid(false)
            );
        }
    }

    let output = AlfredOutput { items };
    println!("{}", serde_json::to_string(&output).unwrap());
}