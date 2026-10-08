//! Resolves a configured vault directory to the identity Obsidian itself uses for it.
//!
//! An `obsidian://` URI addresses a vault by name or ID. The name is normally the folder
//! name, but that lookup is fragile: a path reached through a symlink (such as a cloud
//! drive mount), or one that is a subfolder of a vault rather than its root, has no
//! vault of that name, and Obsidian then quietly falls back to whichever vault is
//! active. Reading the vault registry Obsidian maintains and addressing the vault by ID
//! removes that guesswork.

use std::collections::HashMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

/// Everything needed to build URIs for, and label, one configured vault entry.
#[derive(Clone)]
pub struct VaultTarget {
    /// Value for the URI's `vault=` parameter: the registry ID when the directory is
    /// known to Obsidian, otherwise the folder name.
    pub uri_vault: String,
    /// Path prefix (relative to the vault root, no trailing slash) when the configured
    /// directory is a subfolder of a registered vault; empty when it is the root.
    pub prefix: String,
    /// Human-readable name shown in subtitles.
    pub label: String,
}

impl VaultTarget {
    /// Joins a note path (relative to the configured directory) onto the vault-root
    /// relative prefix, which is what Advanced URI's `filepath` expects.
    pub fn filepath(&self, relative: &str) -> String {
        if self.prefix.is_empty() {
            relative.to_string()
        } else if relative.is_empty() {
            self.prefix.clone()
        } else {
            format!("{}/{}", self.prefix, relative)
        }
    }
}

/// Location of Obsidian's vault registry on macOS.
fn registry_path() -> Option<PathBuf> {
    let home = env::var("HOME").ok()?;
    Some(Path::new(&home).join("Library/Application Support/obsidian/obsidian.json"))
}

/// Reads `{ "vaults": { "<id>": { "path": "...", ... } } }` into id -> canonical path.
/// Any vault whose folder no longer exists is skipped rather than failing the read.
fn load_registered_vaults() -> HashMap<String, PathBuf> {
    let mut vaults = HashMap::new();

    let content = match registry_path().and_then(|path| fs::read_to_string(path).ok()) {
        Some(content) => content,
        None => return vaults,
    };
    let json: serde_json::Value = match serde_json::from_str(&content) {
        Ok(json) => json,
        Err(_) => return vaults,
    };

    if let Some(entries) = json.get("vaults").and_then(|v| v.as_object()) {
        for (id, entry) in entries {
            if let Some(path) = entry.get("path").and_then(|p| p.as_str()) {
                if let Ok(canonical) = fs::canonicalize(path) {
                    vaults.insert(id.clone(), canonical);
                }
            }
        }
    }

    vaults
}

/// Finds the registered vault that contains `vault_dir`, picking the deepest root when
/// vaults are nested, and returns how to address it. Falls back to the folder name when
/// the directory is not inside any registered vault.
pub fn resolve_vault_target(vault_dir: &Path) -> VaultTarget {
    let folder_name = vault_dir
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("")
        .to_string();

    let fallback = VaultTarget {
        uri_vault: folder_name.clone(),
        prefix: String::new(),
        label: folder_name.clone(),
    };

    // Both sides are canonicalized so a symlinked configured path still matches the
    // real location Obsidian recorded
    let canonical_dir = match fs::canonicalize(vault_dir) {
        Ok(path) => path,
        Err(_) => return fallback,
    };

    let registered = load_registered_vaults();
    let best = registered
        .iter()
        .filter(|(_, root)| canonical_dir.starts_with(root))
        .max_by_key(|(_, root)| root.as_os_str().len());

    match best {
        Some((id, root)) => {
            let prefix = canonical_dir
                .strip_prefix(root)
                .map(|rel| rel.to_string_lossy().into_owned())
                .unwrap_or_default();
            // The label stays the configured folder: with a subfolder entry, that is
            // the name the user chose to route to
            VaultTarget { uri_vault: id.clone(), prefix, label: folder_name }
        }
        None => fallback,
    }
}

/// Picks which vault a typed `#tag` routes to. An exact key wins; otherwise a nested
/// tag such as `#corp/project` routes to the vault keyed `#corp`, and the longest such
/// parent wins when several keys nest. Keys are matched case-insensitively because the
/// search terms arrive lowercased.
pub fn route_key<'a>(term: &str, vault_map: &'a HashMap<String, String>) -> Option<&'a str> {
    let lower_term = term.to_lowercase();
    let mut best: Option<&'a str> = None;

    for key in vault_map.keys() {
        if !key.starts_with('#') {
            continue;
        }
        let lower_key = key.to_lowercase();
        let is_match = lower_term == lower_key
            || (lower_term.starts_with(&lower_key) && lower_term[lower_key.len()..].starts_with('/'));
        if is_match && best.map_or(true, |current| key.len() > current.len()) {
            best = Some(key.as_str());
        }
    }

    best
}

/// True when the term is exactly a routing key (not a nested child of one). Exact keys
/// only select a vault and are not written into the note as tags; a nested child is
/// kept as a tag since it carries more than the routing decision.
pub fn is_exact_route_key(term: &str, vault_map: &HashMap<String, String>) -> bool {
    let lower_term = term.to_lowercase();
    vault_map.keys().any(|key| key.to_lowercase() == lower_term)
}
