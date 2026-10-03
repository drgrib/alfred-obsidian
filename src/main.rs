use alfred_workflow_rs::Item;
use serde::Serialize;
use std::env;

#[derive(Serialize)]
struct AlfredOutput {
    items: Vec<Item>,
}

fn main() {
    // Read the vault_path environment variable passed by Alfred
    let vault_path = env::var("vault_path").unwrap_or_else(|_| "".to_string());

    let item = if vault_path.trim().is_empty() {
        Item::new("Vault Path Not Configured")
            .set_subtitle("Please click the [x] icon in the Alfred workflow to set your vault_path.")
            .set_valid(false)
    } else {
        Item::new("Vault Connection Successful 🚀")
            .set_subtitle(format!("Reading from: {}", vault_path))
            .set_valid(false) // False because we aren't actioning this test item
    };

    let output = AlfredOutput {
        items: vec![item],
    };

    println!("{}", serde_json::to_string(&output).unwrap());
}