# Alfred Obsidian Workflow

Lean, high-speed note searching and creation for [Obsidian](https://obsidian.md/) using [Alfred](https://www.alfredapp.com/).

Built in Rust as the spiritual successor to [`alfred-bear`](https://github.com/drgrib/alfred-bear), with a cache-first architecture for zero-latency typing, unified multi-vault search and tag-based vault routing, and native macOS Vision OCR + PDF attachment indexing.

## Requirements

- **macOS** (Apple Silicon)
- **Alfred 5** with the [Powerpack](https://www.alfredapp.com/powerpack/)
- **Obsidian** with the **[Advanced URI](https://github.com/Vinzent03/obsidian-advanced-uri)** community plugin installed and **enabled in every vault** you configure
  - *Note: Obsidian manages community plugins per vault. If you configure multiple vaults, make sure Advanced URI is installed and enabled in each one.*

## Install

1. Download the latest `Obsidian.alfredworkflow` from [Releases](https://github.com/drgrib/alfred-obsidian/releases) and double-click it to import into Alfred.
2. Set your vault path(s) in Alfred's workflow configuration dialog (see [Configuration](#configuration)).
3. Authorize the workflow binary so macOS Gatekeeper allows it to run:

### Authorize Executable

Because this is a free open-source project distributed without a paid Apple Developer signing certificate, macOS blocks the `alfred-obsidian` binary on first run. To authorize it in one step:

1. Open **Alfred Preferences** → **Workflows**.
2. Right-click **Obsidian** in the sidebar and click **Open in Terminal**.
3. Run:
   ```bash
   xattr -rd com.apple.quarantine .
   ```

## Configuration

Click **Configure Workflow...** on the workflow canvas in Alfred Preferences to set up your vault(s) and optional OCR settings:

<!-- Screenshot: Workflow configuration sheet showing Vault Map, More Accurate Slower OCR, and OCR Workers -->
![Workflow Configuration](screenshots/configure-workflow.png)

- **Vault Map** *(required)*: `key: path` pairs, one per line.
  - `default:` is required and sets your primary vault (e.g., `default: ~/Documents/ObsidianVault`).
  - Additional vaults can be mapped to `#tag:` keys (e.g., `#work: ~/WorkDrive/Obsidian/Work`) for unified cross-vault search and tag-routed note creation (see [Multi-Vault Support](#multi-vault-support)).
- **More Accurate, Slower OCR** *(optional, off by default)*: Switches macOS Vision from its fast text recognizer to its accurate recognizer when indexing image attachments and scanned PDF pages. Fast mode uses a fraction of the CPU/battery and is more than enough for typical screenshots; enable this if you regularly search dense, small, or stylized text.
- **OCR Workers** *(optional, default `auto`)*: Number of parallel isolated worker processes used when indexing attachments. Leaving this blank uses one per CPU core, up to 8.

## Caching & Initial Indexing

<!-- Screenshot: Alfred showing live indexing progress bar with percentage, ETA countdown, and file count -->
![Indexing Progress](screenshots/indexing-progress.png)

The first time you trigger the workflow—or whenever a large batch of files changes (more than 10 notes or more than 1 attachment)—a background worker indexes your vault(s) while Alfred displays a live progress percentage, file count, and adaptive time-remaining countdown. If multiple vaults need indexing, they are processed together under a single unified progress bar.

How the cache stays fast day to day:

- **Zero per-keystroke disk walks**: Vault directory scans **only happen on an empty query**. As soon as you start typing, every character reads strictly from the local cache, keeping search instantaneous regardless of vault size.
- **Inline micro-updates**: Small day-to-day changes (up to 10 modified notes or 1 new attachment) reconcile inline in milliseconds when you open the workflow on an empty query, with no progress bar interruption.
- **Isolated attachment cache**: Text extracted from `.png`, `.jpg`, `.jpeg`, `.webp`, and `.pdf` attachments (via PDFKit text layers + macOS Vision OCR on scanned pages) is stored in a separate attachment cache so unchanged media files are never re-processed. Each attachment is read inside a crash-isolated child process so a malformed image or PDF cannot interrupt indexing.

To force a full re-index from scratch (for example, after toggling **More Accurate, Slower OCR**):

```bash
rm -rf ~/Library/Caches/com.runningwithcrayons.Alfred/Workflow\ Data/com.drgrib.obsidian
```

## Search

Trigger search with the `obsidiansearch` keyword (or bind a shortcut to its pre-wired Hotkey node on the workflow canvas / shorten the keyword to `os`).

### Recent Notes

Leave the query empty to see your 50 most recently modified notes across all configured vaults, with each note's folder path and `#tags` shown in the subtitle.

<!-- Screenshot: Empty query showing most recent notes with vault/folder paths and tags -->
![Recent Notes](screenshots/recent-notes.png)

### Basic, Full-Text & Attachment Search

Type search terms in any order to search across note titles, note bodies, and embedded attachments:

- **Smart ranking**: Exact title phrase matches rank first, followed by multi-term title matches, exact phrase body/attachment matches, and multi-term body/attachment matches—breaking ties by most recently modified.
- **Body snippets**: When a note matches inside its body rather than its title, the matching line is shown in the subtitle.
- **Image OCR & PDF search**: Matches found inside an embedded image or PDF surface the Markdown note that embeds it, prefixed with `🖼️` (image) or `📄` (PDF) and the recognized text line.

<!-- Screenshot: Multi-word title and full-text body search results -->
![Basic Search](screenshots/basic-search.png)

<!-- Screenshot: Search result matching text inside an embedded screenshot (🖼️) or PDF (📄) -->
![Attachment OCR Search](screenshots/attachment-search.png)

### Tag Search

Type `#` anywhere in your query to autocomplete tags across your vaults, sorted by how recently each tag was modified (`Just now`, `2h ago`, `3d ago`). Press `Tab` or `Enter` on a tag to complete it and continue typing.

Chain multiple `#tags` with search terms in any order to narrow results to notes containing every specified tag and term. Tags are parsed from both YAML frontmatter (`tags:`) and inline `#tags` in prose (ignoring `#` inside code blocks, links, emphasis, math, and comments).

<!-- Screenshot: Tag autocomplete dropdown showing matching #tags and relative recency -->
![Tag Autocomplete](screenshots/tag-autocomplete.png)

<!-- Screenshot: Search combining #tags and title/content terms -->
![Tag Filtered Search](screenshots/tag-search.png)

### Opening Notes & Modifiers

- **`Enter`**: Open the selected note in a tab inside Obsidian's **main window**.
- **`⌘ + Enter` (`Command`)**: Open the selected note in a standalone **pop-out window**.
- **`⇧ + Enter` (`Shift`) — Link Pasting**: Copy and auto-paste an Obsidian wikilink (`[[Note Title]]`) into your active application.

<!-- Screenshot: Holding Shift or Command on a search result showing the modifier subtitle -->
![Modifier Actions](screenshots/modifier-actions.png)

## Create

Trigger note creation with the `obsidiancreate` keyword (or bind its Hotkey node / shorten the keyword to `oc`).

- **Title & tags**: Type a title and any `#tags` in any order (e.g., `obsidiancreate Q4 Planning #work #roadmap`). Tags are appended to the body of the new note.
- **Subfolders**: Include `/` in the title (e.g., `Projects/Q4 Planning`) to create the note inside a subfolder of the target vault.
- **Duplicate protection**: If a note with the same title already exists in the target vault, the action automatically switches to **Open existing "..."** rather than overwriting it.
- **Window target**: Press `Enter` to open the new note in Obsidian's main window, or `⌘ + Enter` to open it in a pop-out window.

<!-- Screenshot: Creating a new note with subfolder path and #tags -->
![Create Note](screenshots/create-note.png)

## Create/Search

Trigger combined search and creation with the `obsidiancreatesearch` keyword—or use its pre-wired Hotkey node (`⌥O` by default, or customize it in Alfred Preferences).

As you type, the workflow searches across all your vaults while placing a **Create "..."** item at the third row of the results list (or at the bottom when fewer than two notes match) whenever your query includes a note title. If an exact title match already exists in the target vault, the row becomes **Open existing "..."**.

<!-- Screenshot: Create/Search showing top matching notes alongside the Create row at position 3 -->
![Create/Search](screenshots/create-search.png)

## Multi-Vault Support

You can map multiple vaults in **Vault Map** by pairing a `#tag` routing key with each additional vault path:

```yaml
default: ~/Drive/Obsidian/Personal
#work: ~/WorkDrive/Obsidian/Work
```

- **Unified search across vaults**: Both empty queries and typed queries always search across **all** vaults present on your machine—you never need to prefix a search with a vault tag just to find a note. When more than one vault is configured, subtitles automatically include the vault name (`Personal` or `Work/Projects`).
- **Tag-routed creation**: New notes land in your `default` vault unless your query contains a mapped `#tag` (or a nested child such as `#work/q4`), which routes the new note to that vault *and* writes the tag into the note body.
- **Cross-machine sync friendly**: If you sync Alfred Preferences between machines (for instance, a personal Mac where `~/WorkDrive/Obsidian/Work` is not mounted), missing secondary vault directories are silently skipped during search and indexing, and only surface a warning if you explicitly attempt to route a note to that tag. Symlinked cloud-drive paths and subfolders of registered vaults are resolved automatically via Obsidian's vault registry.

<!-- Screenshot: Unified search results spanning Personal and Work vaults, or creating a routed #work note -->
![Multi-Vault Support](screenshots/multi-vault.png)

## Background & Design

### Why Move from Bear (`alfred-bear`) to Obsidian?

Two practical limitations prompted migrating from Bear to Obsidian:

1. **Corporate machine restrictions**: Bear relies inherently on iCloud sync, which many corporate environments block on work machines. Losing fast note access on a work laptop made a single-sync ecosystem unworkable, whereas Obsidian vaults are plain folders that can live on separate drives or sync mechanisms per vault.
2. **Plain Markdown storage**: Bear stores notes inside a SQLite database, which makes Git versioning and bulk refactoring or querying with AI coding agents awkward. Plain `.md` files on disk work natively with standard developer tools.

### Why Not Shimmering Obsidian?

[Shimmering Obsidian](https://github.com/chrisgrieser/shimmering-obsidian) is a feature-packed workflow if you want deep integration with every corner of Obsidian, whereas `alfred-obsidian` intentionally keeps a minimal footprint focused on instant search and note creation. Shimmering Obsidian also requires the third-party **Metadata Extractor** plugin to export vault JSON on a schedule from a running Obsidian instance—a plugin blocked in some corporate environments, and unnecessary when a native Rust binary can index `.md` files directly from disk.

### Differences from `alfred-bear`

- **Rust & Cache-First Architecture**: Rewritten from Go in Rust with incremental background indexing. Because directory checks only run on an empty query and every subsequent keystroke reads strictly from cache, typing feels noticeably snappier than querying SQLite per keystroke.
- **Opens in Main Window by Default**: In Bear, standalone note windows close cleanly and stay out of the way; Obsidian behaves much more like a browser with tabs. `Enter` opens notes in Obsidian's main window to avoid window clutter, while `⌘ + Enter` opens a pop-out window when you want one.
- **Built-in Image OCR & PDF Indexing**: Searches text inside embedded images and PDFs locally via macOS Vision and PDFKit without needing external services.
- **Native Wikilinks**: `⇧ + Enter` pastes `[[Note Title]]` wikilinks rather than app-specific callback URLs.
- **No Table of Contents (`⌘ + ⇧`) Pasting**: Dropped from `alfred-bear` due to low day-to-day usage (Obsidian natively supports `[[Note#Heading]]` autocompletion).

## License

[MIT](LICENSE)
