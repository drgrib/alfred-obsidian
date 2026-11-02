#!/usr/bin/env python3
"""Builds, sanitizes, packages, and uploads a draft GitHub release of Obsidian.alfredworkflow."""

import json
import os
from pathlib import Path
import plistlib
import re
import shutil
import subprocess
import sys
import tempfile

DEFAULT_VAULT_MAP = "default: ~/Documents/ObsidianVault"
WORKFLOW_FILENAME = "Obsidian.alfredworkflow"
STAGED_FILES = ["LICENSE", "alfred-obsidian", "icon.png", "info.plist"]


def read_cargo_version(cargo_toml: Path) -> str:
    text = cargo_toml.read_text(encoding="utf-8")
    pkg_match = re.search(r"(?ms)^\[package\]\s*(.*?)(?=^\[|\Z)", text)
    if not pkg_match:
        sys.exit("Error: [package] section not found in Cargo.toml")
    ver_match = re.search(r'^version\s*=\s*"([^"]+)"', pkg_match.group(1), re.MULTILINE)
    if not ver_match:
        sys.exit("Error: version field not found in Cargo.toml [package]")
    return ver_match.group(1).strip()


def sync_repo_plist_version(plist_path: Path, version: str) -> None:
    raw = plist_path.read_bytes()
    data = plistlib.loads(raw)
    if data.get("version") == version:
        return
    text = raw.decode("utf-8")
    updated, count = re.subn(
        r"(\n\t<key>version</key>\n\t<string>)[^<]*(</string>)",
        rf"\g<1>{version}\g<2>",
        text,
        count=1,
    )
    if count != 1:
        sys.exit("Error: failed to locate top-level <key>version</key> in info.plist")
    plistlib.loads(updated.encode("utf-8"))
    plist_path.write_text(updated, encoding="utf-8")
    print(f"Updated info.plist version to {version}")


def build_sanitized_plist(src_plist: Path, dst_plist: Path, prefs_plist: Path, version: str) -> None:
    with src_plist.open("rb") as f:
        data = plistlib.load(f)

    data["version"] = version

    cleared_hotkeys = 0
    for obj in data.get("objects", []):
        if obj.get("type") == "alfred.workflow.trigger.hotkey":
            cfg = obj.setdefault("config", {})
            if cfg.get("hotkey", 0) != 0 or cfg.get("hotmod", 0) != 0 or cfg.get("hotstring", "") != "":
                cleared_hotkeys += 1
            cfg["hotkey"] = 0
            cfg["hotmod"] = 0
            cfg["hotstring"] = ""

    found_vault_map = False
    for item in data.get("userconfigurationconfig", []):
        if item.get("variable") == "vault_map":
            item.setdefault("config", {})["default"] = DEFAULT_VAULT_MAP
            found_vault_map = True

    if not found_vault_map:
        sys.exit("Error: vault_map entry missing from userconfigurationconfig in info.plist")

    serialized = plistlib.dumps(data)

    if prefs_plist.exists():
        with prefs_plist.open("rb") as f:
            prefs = plistlib.load(f)
        serialized_text = serialized.decode("utf-8")
        for line in str(prefs.get("vault_map", "")).splitlines():
            stripped = line.strip()
            if stripped and stripped != DEFAULT_VAULT_MAP and stripped in serialized_text:
                sys.exit("Error: personal vault_map setting detected in staged info.plist")

    dst_plist.write_bytes(serialized)
    print(f"Sanitized staged info.plist (version={version}, cleared {cleared_hotkeys} hotkey(s))")


def check_git_state(repo_dir: Path) -> None:
    if not (repo_dir / ".git").exists() or not shutil.which("git"):
        return
    status = subprocess.run(
        ["git", "status", "--porcelain"],
        cwd=repo_dir,
        capture_output=True,
        text=True,
        check=False,
    )
    if status.returncode == 0 and status.stdout.strip():
        print("Note: You have uncommitted changes. Commit and push to GitHub before publishing the draft release.")
        return

    branch_status = subprocess.run(
        ["git", "status", "-sb"],
        cwd=repo_dir,
        capture_output=True,
        text=True,
        check=False,
    )
    if branch_status.returncode == 0 and "[ahead " in branch_status.stdout:
        print("Note: Local branch is ahead of origin. Run 'git push' before publishing the draft release.")


def upload_github_draft(repo_dir: Path, out_path: Path, tag: str) -> None:
    if not shutil.which("gh"):
        print("Note: 'gh' CLI not found (install via 'brew install gh && gh auth login' for auto-upload).")
        subprocess.run(["open", "-R", str(out_path)], check=False)
        return

    auth = subprocess.run(["gh", "auth", "status"], cwd=repo_dir, capture_output=True, text=True, check=False)
    if auth.returncode != 0:
        print("Note: 'gh' CLI is not logged in (run 'gh auth login' for auto-upload). Revealing file in Finder.")
        subprocess.run(["open", "-R", str(out_path)], check=False)
        return

    check_git_state(repo_dir)

    api_res = subprocess.run(
        ["gh", "api", "repos/:owner/:repo/releases"],
        cwd=repo_dir,
        stdout=subprocess.PIPE,
        text=True,
        check=True,
    )
    releases = json.loads(api_res.stdout)
    existing = next((r for r in releases if r.get("tag_name") == tag), None)

    if existing:
        if not existing.get("draft", False):
            sys.exit(
                f"Error: Release {tag} is already published on GitHub. "
                "Bump the version in Cargo.toml before creating a new release."
            )
        print(f"Updating {WORKFLOW_FILENAME} on existing draft release {tag} (preserving draft notes)...")
        subprocess.run(
            ["gh", "release", "upload", tag, str(out_path), "--clobber"],
            cwd=repo_dir,
            check=True,
        )
        html_url = existing["html_url"]
    else:
        print(f"Creating GitHub draft release {tag} and uploading {WORKFLOW_FILENAME}...")
        create_res = subprocess.run(
            ["gh", "release", "create", tag, str(out_path), "--draft", "--title", tag, "--notes", ""],
            cwd=repo_dir,
            stdout=subprocess.PIPE,
            text=True,
            check=True,
        )
        html_url = create_res.stdout.strip().splitlines()[-1]

    edit_url = html_url.replace("/releases/tag/", "/releases/edit/")
    print(f"Opening draft release editor: {edit_url}")
    subprocess.run(["open", edit_url], check=False)


def main() -> None:
    repo_dir = Path(__file__).resolve().parent.parent
    version = read_cargo_version(repo_dir / "Cargo.toml")
    tag = f"v{version}"

    sync_repo_plist_version(repo_dir / "info.plist", version)

    target_dir = Path("/tmp/alfred-obsidian-target")
    local_bin = repo_dir / "alfred-obsidian"
    stable_bin = repo_dir / "alfred-obsidian-stable"
    out_path = Path.home() / "Downloads" / WORKFLOW_FILENAME

    try:
        print(f"Building alfred-obsidian {version} (--release)...")
        subprocess.run(
            ["cargo", "build", "--release", "--target-dir", str(target_dir)],
            cwd=repo_dir,
            check=True,
        )
        built_bin = target_dir / "release" / "alfred-obsidian"

        if local_bin.exists():
            shutil.copy2(local_bin, stable_bin)
        shutil.copy2(built_bin, local_bin)

        with tempfile.TemporaryDirectory(prefix="alfred-obsidian-release-") as tmp:
            stage_dir = Path(tmp)

            staged_bin = stage_dir / "alfred-obsidian"
            shutil.copyfile(built_bin, staged_bin)
            os.chmod(staged_bin, 0o755)
            subprocess.run(["strip", "-x", str(staged_bin)], check=True)
            subprocess.run(["codesign", "--force", "--sign", "-", str(staged_bin)], check=True)

            build_sanitized_plist(
                src_plist=repo_dir / "info.plist",
                dst_plist=stage_dir / "info.plist",
                prefs_plist=repo_dir / "prefs.plist",
                version=version,
            )
            shutil.copyfile(repo_dir / "icon.png", stage_dir / "icon.png")
            shutil.copyfile(repo_dir / "LICENSE", stage_dir / "LICENSE")

            present = sorted(p.name for p in stage_dir.iterdir())
            if present != STAGED_FILES:
                sys.exit(f"Error: unexpected staged files: {present}")

            out_path.parent.mkdir(parents=True, exist_ok=True)
            if out_path.exists():
                out_path.unlink()

            subprocess.run(
                ["/usr/bin/zip", "-q", "-X", str(out_path), *STAGED_FILES],
                cwd=stage_dir,
                check=True,
            )
    finally:
        shutil.rmtree(target_dir, ignore_errors=True)

    size_kb = out_path.stat().st_size / 1024
    print(f"Packaged {out_path} ({size_kb:.0f} KB)")

    upload_github_draft(repo_dir, out_path, tag)


if __name__ == "__main__":
    main()
