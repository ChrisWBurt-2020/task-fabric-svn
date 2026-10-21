#!/usr/bin/env python3
"""Task Fabric SVN Context Distiller & Harvester.

Aggregates:
1. Git repositories in ~/Work (branches, HEAD commits, dirty status, recent log, doc snippets)
2. Multi-agent state & attention items (via task-fabric-svn-work status --json)
3. Cognitive memories from exocortex.db (recent decisions, receipts, reflections)
4. Personal study context (via task-fabric-svn-work study-context --json)

Outputs a token-budgeted, timestamped, SHA-256 hashed bundle to:
~/.local/state/task-fabric-svn/context-bundle.json
And notifies HomeNode (port 8790) over HTTP for immediate SSE broadcast.
"""

import os
import re
import sys
import json
import glob
import time
import hashlib
import sqlite3
import subprocess
from pathlib import Path

STATE_DIR = Path(os.path.expanduser("~/.local/state/task-fabric-svn"))
OUTPUT_BUNDLE = STATE_DIR / "context-bundle.json"
EXOCORTEX_DB = STATE_DIR / "exocortex.db"
WORK_DIR = Path(os.environ.get("TASK_FABRIC_SVN_WORK_DIR", str(Path.home() / "Work")))
NODE_NOTIFY_URL = "http://127.0.0.1:8790/v1/voice/notify"


def get_git_repo_info(repo_path: Path, is_top_active: bool = False) -> dict:
    try:
        # Branch
        branch_res = subprocess.run(
            ["git", "-C", str(repo_path), "branch", "--show-current"],
            capture_output=True, text=True, timeout=2
        )
        branch = branch_res.stdout.strip() or "HEAD (detached)"

        # HEAD commit
        log_res = subprocess.run(
            ["git", "-C", str(repo_path), "log", "-n", "3" if is_top_active else "1", "--pretty=format:%h|%an|%ar|%s"],
            capture_output=True, text=True, timeout=2
        )
        commits_raw = [line.split("|", 3) for line in log_res.stdout.strip().splitlines() if line]
        recent_commits = []
        for parts in commits_raw:
            if len(parts) == 4:
                recent_commits.append({
                    "sha": parts[0],
                    "author": parts[1],
                    "relativeTime": parts[2],
                    "subject": parts[3]
                })

        # Dirty files count
        status_res = subprocess.run(
            ["git", "-C", str(repo_path), "status", "--porcelain"],
            capture_output=True, text=True, timeout=2
        )
        status_lines = [l for l in status_res.stdout.splitlines() if l.strip()]
        dirty_count = len(status_lines)

        diff_summary = ""
        doc_excerpt = ""
        if is_top_active:
            # Diff stat for uncommitted changes
            diff_res = subprocess.run(
                ["git", "-C", str(repo_path), "diff", "--stat"],
                capture_output=True, text=True, timeout=2
            )
            diff_summary = diff_res.stdout.strip().splitlines()[-1] if diff_res.stdout.strip() else ""

            # Check for README or AGENTS.md
            for doc_name in ["AGENTS.md", "README.md", "CONTEXT.md"]:
                doc_path = repo_path / doc_name
                if doc_path.exists() and doc_path.is_file():
                    try:
                        content = doc_path.read_text(encoding="utf-8", errors="replace")
                        doc_excerpt = content[:600].strip()
                        break
                    except Exception:
                        pass

        head = recent_commits[0] if recent_commits else None
        return {
            "name": repo_path.name,
            "path": str(repo_path),
            "branch": branch,
            "headSha": head["sha"] if head else "",
            "headSubject": head["subject"] if head else "",
            "headAuthor": head["author"] if head else "",
            "headTime": head["relativeTime"] if head else "",
            "dirtyCount": dirty_count,
            "diffSummary": diff_summary,
            "recentCommits": recent_commits,
            "docExcerpt": doc_excerpt
        }
    except Exception as e:
        return {
            "name": repo_path.name,
            "path": str(repo_path),
            "error": str(e)
        }


def collect_repos() -> list:
    if not WORK_DIR.exists():
        return []

    # Find all git dirs and sort by most recent git activity
    repo_dirs = []
    for entry in WORK_DIR.iterdir():
        if entry.is_dir() and (entry / ".git").exists():
            git_index = entry / ".git" / "index"
            mtime = git_index.stat().st_mtime if git_index.exists() else entry.stat().st_mtime
            repo_dirs.append((entry, mtime))

    repo_dirs.sort(key=lambda x: x[1], reverse=True)

    # Top 8 most recently touched get deep inspection; others get compact inspection
    repos = []
    for idx, (path, _) in enumerate(repo_dirs):
        is_top = (idx < 8)
        repos.append(get_git_repo_info(path, is_top_active=is_top))

    return repos


def collect_work_status() -> dict:
    try:
        res = subprocess.run(["task-fabric-svn-work", "status", "--json"], capture_output=True, text=True, timeout=4)
        if res.returncode == 0 and res.stdout.strip():
            raw = json.loads(res.stdout)
            return {
                "project": raw.get("project", {}),
                "agents": [
                    {
                        "id": a.get("id"),
                        "label": a.get("label"),
                        "state": a.get("state"),
                        "detail": a.get("detail", "")[:300]
                    }
                    for a in raw.get("agents", [])
                ],
                "attention": [
                    {
                        "id": att.get("id"),
                        "title": att.get("title"),
                        "detail": att.get("detail", ""),
                        "severity": att.get("severity"),
                        "urgency": att.get("urgency"),
                        "interferenceClass": att.get("interference_class")
                    }
                    for att in raw.get("attention", [])
                ]
            }
    except Exception:
        pass
    return {"project": {}, "agents": [], "attention": []}


def collect_study_context() -> dict:
    try:
        res = subprocess.run(["task-fabric-svn-work", "study-context", "--json"], capture_output=True, text=True, timeout=3)
        if res.returncode == 0 and res.stdout.strip():
            return json.loads(res.stdout)
    except Exception:
        pass
    return {}


def collect_memories() -> list:
    if not EXOCORTEX_DB.exists():
        return []
    try:
        conn = sqlite3.connect(f"file:{EXOCORTEX_DB}?mode=ro", uri=True)
        cursor = conn.cursor()
        cursor.execute("""
            SELECT source, source_id, world_id, title, content_text, created_at
            FROM memories
            ORDER BY created_at DESC
            LIMIT 25
        """)
        rows = cursor.fetchall()
        conn.close()

        memories = []
        for r in rows:
            memories.append({
                "source": r[0],
                "sourceId": r[1],
                "worldId": r[2] or "general",
                "title": r[3],
                "snippet": r[4][:250].replace("\n", " ").strip(),
                "createdAt": r[5]
            })
        return memories
    except Exception:
        return []


def build_bundle() -> dict:
    repos = collect_repos()
    work_status = collect_work_status()
    study = collect_study_context()
    memories = collect_memories()

    payload = {
        "version": "1.0",
        "generatedAt": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
        "workstationHost": os.uname().nodename,
        "project": work_status.get("project", {}),
        "agents": work_status.get("agents", []),
        "attention": work_status.get("attention", []),
        "studyContext": study,
        "topRepos": repos[:8],
        "otherRepos": [
            {"name": r["name"], "branch": r.get("branch", ""), "headSubject": r.get("headSubject", ""), "dirtyCount": r.get("dirtyCount", 0)}
            for r in repos[8:]
        ],
        "recentMemories": memories
    }

    serialized = json.dumps(payload, sort_keys=True)
    bundle_hash = hashlib.sha256(serialized.encode("utf-8")).hexdigest()
    payload["bundleHash"] = bundle_hash
    return payload



def _node_token() -> str:
    """Resolve the HomeNode bearer token at runtime.

    Order: $HERON_NODE_TOKEN, then the private secrets file
    ~/.config/heron/homenode-secrets.env. Never hardcode the token in
    this (public) repository. Returns "" when unavailable, in which
    case the notify call is skipped.
    """
    tok = os.environ.get("HERON_NODE_TOKEN", "").strip()
    if tok:
        return tok
    try:
        secrets_path = Path.home() / ".config" / "heron" / "homenode-secrets.env"
        for line in secrets_path.read_text(encoding="utf-8").splitlines():
            m = re.match(r'\s*HERON_NODE_TOKEN\s*=\s*"?([^"\n]+)"?', line)
            if m:
                return m.group(1).strip()
    except OSError:
        pass
    return ""


def notify_node(bundle_hash: str):
    import urllib.request
    payload = json.dumps({
        "kind": "context_updated",
        "bundleHash": bundle_hash,
        "timestamp": int(time.time() * 1000),
        "summary": "Workstation context snapshot updated."
    }).encode("utf-8")

    token = _node_token()
    if not token:
        return

    try:
        req = urllib.request.Request(
            NODE_NOTIFY_URL,
            data=payload,
            headers={"Content-Type": "application/json", "Authorization": "Bearer " + token}
        )
        urllib.request.urlopen(req, timeout=2)
    except Exception:
        pass


def main():
    STATE_DIR.mkdir(parents=True, exist_ok=True)
    bundle = build_bundle()
    bundle_json = json.dumps(bundle, indent=2)

    temp_path = OUTPUT_BUNDLE.with_suffix(".tmp")
    temp_path.write_text(bundle_json, encoding="utf-8")
    temp_path.replace(OUTPUT_BUNDLE)

    bundle_size = OUTPUT_BUNDLE.stat().st_size
    print(f"Distilled context bundle written to {OUTPUT_BUNDLE} ({bundle_size} bytes, hash={bundle['bundleHash'][:12]})")

    notify_node(bundle['bundleHash'])


if __name__ == "__main__":
    main()
