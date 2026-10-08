use alfred_workflow_rs::Item;
use std::collections::HashMap;
use std::path::Path;
use std::time::SystemTime;

use crate::types::{AlfredOutput, FileResult};

pub fn url_encode(input: &str) -> String {
    let mut encoded = String::new();
    for byte in input.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                encoded.push(byte as char);
            }
            _ => {
                encoded.push_str(&format!("%{:02X}", byte));
            }
        }
    }
    encoded
}

/// Builds the row that opens an existing note or creates a new one, with any typed tags
/// written into the new note's body.
pub fn build_create_item(
    title_string: &str,
    tag_terms: &[&str],
    vault_name: &str,
    is_duplicate: bool,
    has_multiple_vaults: bool,
) -> Item {
    let tag_string = tag_terms
        .iter()
        .map(|t| format!("#{}", t))
        .collect::<Vec<String>>()
        .join(" ");

    if is_duplicate {
        // Open the existing note as-is; omit mode=new and data so tags are never appended
        let open_uri = format!(
            "obsidian://advanced-uri?vault={}&filepath={}|{}",
            url_encode(vault_name),
            url_encode(title_string),
            title_string
        );

        Item::new(format!("Open existing \"{}\"", title_string))
            .set_subtitle("Note already exists")
            .set_arg(open_uri)
            .set_valid(true)
    } else {
        // A slash in the typed title means the note lands in a subdirectory of the vault.
        // The title shows only the note name; the directory is shown in the subtitle.
        let new_note_path = Path::new(title_string);
        let file_name_str = new_note_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(title_string)
            .to_string();
        let parent_str = match new_note_path.parent() {
            Some(p) if !p.as_os_str().is_empty() => p.to_string_lossy().into_owned(),
            _ => String::new(),
        };

        let create_location = if !parent_str.is_empty() {
            format!("{}/{}", vault_name, parent_str)
        } else if has_multiple_vaults {
            vault_name.to_string()
        } else {
            String::new()
        };

        let create_subtitle = if tag_string.is_empty() {
            create_location
        } else if create_location.is_empty() {
            tag_string.clone()
        } else {
            format!("{} | {}", create_location, tag_string)
        };

        let body_string = if tag_string.is_empty() {
            String::new()
        } else {
            format!("\n\n{}", tag_string)
        };

        let create_uri = format!(
            "obsidian://advanced-uri?vault={}&filepath={}&mode=new&data={}|{}",
            url_encode(vault_name),
            url_encode(title_string),
            url_encode(&body_string),
            title_string
        );

        Item::new(format!("Create \"{}\"", file_name_str))
            .set_subtitle(create_subtitle)
            .set_arg(create_uri)
            .set_valid(true)
    }
}

/// Turns ranked search hits into Alfred rows: title, a subtitle that explains the match,
/// and the Obsidian Advanced URI that opens the note.
pub fn assemble_alfred_items(
    results: &[FileResult],
    vault_dir: &Path,
    vault_name: &str,
    title_terms: &[&str],
    has_multiple_vaults: bool,
) -> Vec<Item> {
    let mut items = Vec::new();

    for res in results.iter().take(50) {
        // Show the note's location relative to the vault, without the filename
        let note_path = Path::new(&res.path);
        let location = match note_path.strip_prefix(vault_dir) {
            Ok(relative) => match relative.parent() {
                Some(parent) if !parent.as_os_str().is_empty() => {
                    if has_multiple_vaults {
                        format!("{}/{}", vault_name, parent.to_string_lossy())
                    } else {
                        parent.to_string_lossy().into_owned()
                    }
                }
                _ => {
                    if has_multiple_vaults {
                        vault_name.to_string()
                    } else {
                        String::new()
                    }
                }
            },
            Err(_) => {
                if has_multiple_vaults {
                    vault_name.to_string()
                } else {
                    String::new()
                }
            }
        };

        // When the title itself explains the match, the note's tags are more
        // useful than the matched line; otherwise the snippet explains the hit
        let lower_title = res.title.to_lowercase();
        let title_matches_all_terms =
            !title_terms.is_empty() && title_terms.iter().all(|term| lower_title.contains(term));

        let subtitle = if !title_matches_all_terms && res.snippet.is_some() {
            let snippet = res.snippet.as_ref().unwrap();
            if location.is_empty() {
                snippet.clone()
            } else {
                format!("{} | {}", location, snippet)
            }
        } else if res.tags.is_empty() {
            location
        } else if location.is_empty() {
            format!("#{}", res.tags.join(" #"))
        } else {
            format!("{} | #{}", location, res.tags.join(" #"))
        };

        // Obsidian Advanced URI expects the note path relative to the vault root, without the .md extension
        let arg_path = note_path
            .strip_prefix(vault_dir)
            .unwrap_or(note_path)
            .to_string_lossy()
            .trim_end_matches(".md")
            .to_string();

        let item = Item::new(res.title.clone())
            .set_subtitle(subtitle)
            .set_arg(format!(
                "obsidian://advanced-uri?vault={}&filepath={}|{}",
                url_encode(vault_name),
                url_encode(&arg_path),
                res.title
            ))
            .set_valid(true);

        items.push(item);
    }

    items
}

// Helper to format system time for the subtitle
pub fn format_time_ago(time: SystemTime) -> String {
    let now = SystemTime::now();
    if let Ok(duration) = now.duration_since(time) {
        let secs = duration.as_secs();
        if secs < 60 {
            "Just now".to_string()
        } else if secs < 3600 {
            format!("{}m ago", secs / 60)
        } else if secs < 86400 {
            format!("{}h ago", secs / 3600)
        } else {
            format!("{}d ago", secs / 86400)
        }
    } else {
        "Unknown".to_string()
    }
}

/// Formats a remaining-time estimate, showing only the units that are needed.
pub fn format_eta(secs: u64) -> String {
    let hours = secs / 3600;
    let minutes = (secs % 3600) / 60;
    let seconds = secs % 60;

    if hours > 0 {
        format!("{}h{:02}m{:02}s", hours, minutes, seconds)
    } else if minutes > 0 {
        format!("{:02}m{:02}s", minutes, seconds)
    } else {
        format!("{:02}s", seconds)
    }
}

/// Builds the tag suggestion list while the user is typing a `#tag`.
///
/// Returns `None` the moment the cursor is not sitting on a partial tag, so the caller
/// falls through to the normal search path instead of showing suggestions.
pub fn handle_tag_autocomplete(
    raw_query: &str,
    all_terms: &[&str],
    tag_recency: &HashMap<String, SystemTime>,
) -> Option<AlfredOutput> {
    let ends_with_space = raw_query.ends_with(' ');
    let last_term = all_terms.last().copied().unwrap_or("");
    let is_autocompleting_tag = !ends_with_space && last_term.starts_with('#');

    if !is_autocompleting_tag {
        return None;
    }

    let mut items = Vec::new();

    // Tag Autocomplete Mode
    let partial_tag = last_term.trim_start_matches('#');

    let mut matched_tags: Vec<(&String, &SystemTime)> = tag_recency.iter()
        .filter(|(t, _)| t.contains(partial_tag))
        .collect();

    matched_tags.sort_by(|a, b| b.1.cmp(a.1));

    let prefix = raw_query.trim_end_matches(|c: char| !c.is_whitespace());

    for (tag, modified_time) in matched_tags.into_iter().take(30) {
        let time_ago = format_time_ago(*modified_time);
        items.push(
            Item::new(format!("#{}", tag))
                .set_subtitle(format!("{}", time_ago))
                .set_autocomplete(format!("{}#{} ", prefix, tag))
                .set_valid(false)
        );
    }

    if items.is_empty() {
        items.push(Item::new("No matching tags").set_valid(false));
    }

    Some(AlfredOutput { rerun: None, items })
}

