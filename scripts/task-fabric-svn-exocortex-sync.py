#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.11"
# dependencies = [
#     "fastembed>=0.5.0",
#     "numpy>=1.26.0",
# ]
# ///
"""Task Fabric SVN Exocortex Cognitive Graph & Vector Sync.

Ingests world events, transition receipts, approval decisions, reflections,
and project definitions into a local SQLite vector database at
~/.local/state/task-fabric-svn/exocortex.db with dense bge-small-en-v1.5 embeddings.
"""

import os
import sys
import json
import glob
import hashlib
import sqlite3
import datetime
from pathlib import Path
import numpy as np
from fastembed import TextEmbedding

STATE_DIR = Path(os.path.expanduser("~/.local/state/task-fabric-svn"))
SHARE_DIR = Path(os.path.expanduser("~/.local/share/task-fabric-svn"))
CONFIG_DIR = Path(os.path.expanduser("~/.config/task-fabric-svn"))
DB_PATH = STATE_DIR / "exocortex.db"
MODEL_NAME = "BAAI/bge-small-en-v1.5"


def init_db(db_path: Path) -> sqlite3.Connection:
    STATE_DIR.mkdir(mode=0o700, parents=True, exist_ok=True)
    conn = sqlite3.connect(str(db_path))
    conn.execute("PRAGMA journal_mode = WAL")
    conn.execute("PRAGMA synchronous = NORMAL")
    with conn:
        conn.execute("""
            CREATE TABLE IF NOT EXISTS memories (
                id TEXT PRIMARY KEY,
                source TEXT NOT NULL,
                source_id TEXT NOT NULL,
                world_id TEXT,
                title TEXT NOT NULL,
                content_text TEXT NOT NULL,
                metadata_json TEXT,
                content_hash TEXT NOT NULL,
                embedding BLOB NOT NULL,
                created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
            )
        """)
        conn.execute("CREATE INDEX IF NOT EXISTS idx_memories_world ON memories(world_id)")
        conn.execute("CREATE INDEX IF NOT EXISTS idx_memories_source ON memories(source)")
        conn.execute("CREATE INDEX IF NOT EXISTS idx_memories_hash ON memories(content_hash)")
        conn.execute("CREATE INDEX IF NOT EXISTS idx_memories_created ON memories(created_at DESC)")

        conn.execute("""
            CREATE TABLE IF NOT EXISTS cognitive_links (
                source_id TEXT NOT NULL,
                target_id TEXT NOT NULL,
                relation TEXT NOT NULL,
                weight REAL DEFAULT 1.0,
                PRIMARY KEY(source_id, target_id, relation)
            )
        """)
    try:
        os.chmod(db_path, 0o600)
    except Exception:
        pass
    return conn


def parse_world_events(path: Path):
    if not path.exists():
        return []
    with open(path, "r", encoding="utf-8", errors="replace") as f:
        text = f.read()

    events = []
    decoder = json.JSONDecoder()
    idx = 0
    while idx < len(text):
        text_slice = text[idx:].lstrip()
        if not text_slice:
            break
        try:
            obj, end = decoder.raw_decode(text_slice)
            events.append(obj)
            idx += len(text[idx:]) - len(text_slice) + end
        except Exception:
            idx += 1
    return events


def parse_approvals(path: Path):
    if not path.exists():
        return []
    records = []
    with open(path, "r", encoding="utf-8", errors="replace") as f:
        for line in f:
            line = line.strip()
            if not line:
                continue
            try:
                records.append(json.loads(line))
            except Exception:
                continue
    return records


def parse_reflections(directory: Path):
    if not directory.exists():
        return []
    items = []
    for md_file in sorted(directory.glob("*.md")):
        if md_file.name == "latest.md":
            continue
        try:
            content = md_file.read_text(encoding="utf-8", errors="replace")
            items.append((md_file.stem, content))
        except Exception:
            continue
    return items


def parse_projects(path: Path):
    if not path.exists():
        return []
    try:
        import tomllib
    except ImportError:
        import tomli as tomllib
    try:
        with open(path, "rb") as f:
            data = tomllib.load(f)
        return data.get("projects", {})
    except Exception:
        return {}


def parse_receipts(directory: Path):
    if not directory.exists():
        return []
    receipts = []
    for receipt_path in sorted(directory.glob("receipt-*.json")):
        try:
            with open(receipt_path, "r", encoding="utf-8") as f:
                data = json.load(f)
                receipts.append(data)
        except Exception:
            continue
    return receipts


def collect_raw_documents():
    docs = []

    # 1. World Events
    events_file = STATE_DIR / "world-events.jsonl"
    events = parse_world_events(events_file)
    for ev in events:
        ev_id = ev.get("id", "")
        world_id = ev.get("world_id")
        kind = ev.get("kind", "event")
        summary = ev.get("summary", "")
        source = ev.get("source", "system")
        outcome = ev.get("outcome", "")
        ts = ev.get("timestamp_epoch_seconds", 0)
        dt_str = datetime.datetime.fromtimestamp(ts, datetime.timezone.utc).strftime("%Y-%m-%d %H:%M:%S UTC") if ts else "unknown"

        title = f"[World: {world_id or 'ambient'}] {kind} ({outcome}): {summary}"
        content = (
            f"Event ID: {ev_id}\n"
            f"World ID: {world_id or 'none'}\n"
            f"Event Kind: {kind}\n"
            f"Source Component: {source}\n"
            f"Outcome: {outcome}\n"
            f"Summary: {summary}\n"
            f"Recorded: {dt_str}\n"
            f"Object Ref: {ev.get('object_ref', '')}\n"
            f"Effect Ref: {ev.get('effect_ref', '')}\n"
            f"Evidence: {json.dumps(ev.get('evidence_refs', []))}"
        )
        docs.append({
            "id": f"event_{ev_id}_{hashlib.md5(content.encode()).hexdigest()[:6]}",
            "source": "world_event",
            "source_id": ev_id,
            "world_id": world_id,
            "title": title,
            "content_text": content,
            "metadata": json.dumps(ev),
        })

    # 2. Approval Decisions
    approvals_file = STATE_DIR / "approval-decisions.jsonl"
    approvals = parse_approvals(approvals_file)
    for app in approvals:
        ev_id = app.get("event_id", "")
        action = app.get("action", "")
        status = app.get("status", "")
        msg = app.get("message", "")
        title = f"[Consent] {action.upper()} for {ev_id} ({status})"
        content = (
            f"Consent Action: {action}\n"
            f"Target Event: {ev_id}\n"
            f"Status: {status}\n"
            f"Message / Rationale: {msg}"
        )
        docs.append({
            "id": f"approval_{ev_id}_{action}_{status}",
            "source": "approval_decision",
            "source_id": ev_id,
            "world_id": None,
            "title": title,
            "content_text": content,
            "metadata": json.dumps(app),
        })

    # 3. Reflections
    reflections_dir = SHARE_DIR / "reflections"
    for stem, text in parse_reflections(reflections_dir):
        title = f"[Reflection] {stem}"
        docs.append({
            "id": f"reflection_{stem}",
            "source": "reflection",
            "source_id": stem,
            "world_id": None,
            "title": title,
            "content_text": text,
            "metadata": json.dumps({"file": stem}),
        })

    # 4. Projects Definitions
    projects_file = CONFIG_DIR / "projects.toml"
    projects = parse_projects(projects_file)
    project_list = projects if isinstance(projects, list) else list(projects.values()) if isinstance(projects, dict) else []
    for p_data in project_list:
        p_id = p_data.get("id", "")
        name = p_data.get("label", p_id)
        objective = p_data.get("objective", "")
        repo = p_data.get("repo", "")
        services = [s.get("id", "") for s in p_data.get("services", [])] if isinstance(p_data.get("services"), list) else []
        title = f"[World Definition] {name} ({p_id})"
        content = (
            f"World: {name}\n"
            f"World ID: {p_id}\n"
            f"Objective: {objective}\n"
            f"Phase: {p_data.get('phase', '')}\n"
            f"Repository: {repo}\n"
            f"Services: {', '.join(services)}\n"
            f"Next Action: {p_data.get('next_action', '')}\n"
            f"Config: {json.dumps(p_data)}"
        )
        docs.append({
            "id": f"project_{p_id}",
            "source": "project",
            "source_id": p_id,
            "world_id": p_id,
            "title": title,
            "content_text": content,
            "metadata": json.dumps(p_data),
        })

    # 5. Transition Receipts
    transitions_dir = STATE_DIR / "world-transitions"
    receipts = parse_receipts(transitions_dir)
    for rec in receipts:
        r_id = rec.get("id", "")
        w_id = rec.get("world_id", "")
        outcome = rec.get("outcome", "")
        effects = rec.get("completed_effects", [])
        title = f"[Transition Receipt] {w_id} ({outcome})"
        content = (
            f"Receipt ID: {r_id}\n"
            f"World ID: {w_id}\n"
            f"Outcome: {outcome}\n"
            f"Plan ID: {rec.get('plan_id')}\n"
            f"Completed Effects Count: {len(effects)}"
        )
        docs.append({
            "id": f"receipt_{r_id}",
            "source": "receipt",
            "source_id": r_id,
            "world_id": w_id,
            "title": title,
            "content_text": content,
            "metadata": json.dumps(rec),
        })

    return docs


def sync_exocortex(force: bool = False, quiet: bool = False):
    conn = init_db(DB_PATH)
    if force:
        with conn:
            conn.execute("DELETE FROM memories")
            conn.execute("DELETE FROM cognitive_links")
        if not quiet:
            print("Cleared existing Exocortex memories.")

    # Load existing hashes
    cursor = conn.cursor()
    cursor.execute("SELECT id, content_hash FROM memories")
    existing_hashes = {row[0]: row[1] for row in cursor.fetchall()}

    docs = collect_raw_documents()
    to_embed = []

    for d in docs:
        c_hash = hashlib.sha256(d["content_text"].encode("utf-8")).hexdigest()
        d["content_hash"] = c_hash
        if d["id"] not in existing_hashes or existing_hashes[d["id"]] != c_hash:
            to_embed.append(d)

    if not quiet:
        print(f"Exocortex Ingestion: {len(docs)} total items collected, {len(to_embed)} new/updated to embed.")

    if to_embed:
        if not quiet:
            print(f"Generating embeddings using {MODEL_NAME}...")
        embedder = TextEmbedding(MODEL_NAME)
        texts = [d["content_text"] for d in to_embed]
        embeddings = list(embedder.embed(texts))

        with conn:
            for doc, emb in zip(to_embed, embeddings):
                emb_bytes = np.array(emb, dtype=np.float32).tobytes()
                conn.execute("""
                    INSERT OR REPLACE INTO memories (
                        id, source, source_id, world_id, title, content_text,
                        metadata_json, content_hash, embedding, created_at
                    ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, CURRENT_TIMESTAMP)
                """, (
                    doc["id"],
                    doc["source"],
                    doc["source_id"],
                    doc["world_id"],
                    doc["title"],
                    doc["content_text"],
                    doc["metadata"],
                    doc["content_hash"],
                    emb_bytes,
                ))

                # Cognitive links: link approvals to world events, receipts to projects
                if doc["source"] == "approval_decision" and doc["source_id"]:
                    conn.execute("""
                        INSERT OR IGNORE INTO cognitive_links (source_id, target_id, relation, weight)
                        VALUES (?, ?, 'consents_for', 1.0)
                    """, (doc["id"], f"event_{doc['source_id']}"))
                elif doc["source"] == "receipt" and doc["world_id"]:
                    conn.execute("""
                        INSERT OR IGNORE INTO cognitive_links (source_id, target_id, relation, weight)
                        VALUES (?, ?, 'transitioned_to', 1.0)
                    """, (doc["id"], f"project_{doc['world_id']}"))

        if not quiet:
            print(f"Successfully embedded and indexed {len(to_embed)} memories.")

    # Print summary stats
    cursor.execute("SELECT source, count(*) FROM memories GROUP BY source")
    stats = cursor.fetchall()
    if not quiet:
        print("\nCurrent Memory Store Distribution:")
        for src, count in stats:
            print(f"  - {src}: {count}")
    conn.close()


def print_stats():
    if not DB_PATH.exists():
        print("Exocortex database does not exist yet.")
        return
    conn = init_db(DB_PATH)
    cursor = conn.cursor()
    cursor.execute("SELECT count(*) FROM memories")
    total = cursor.fetchone()[0]
    cursor.execute("SELECT source, count(*) FROM memories GROUP BY source")
    stats = cursor.fetchall()
    cursor.execute("SELECT count(DISTINCT world_id) FROM memories WHERE world_id IS NOT NULL")
    worlds = cursor.fetchone()[0]
    cursor.execute("SELECT count(*) FROM cognitive_links")
    links = cursor.fetchone()[0]
    print("Exocortex Cognitive Graph Status:")
    print(f"  Database: {DB_PATH}")
    print(f"  Total Memories: {total}")
    print(f"  Distinct Worlds: {worlds}")
    print(f"  Cognitive Links: {links}")
    print("  By Source:")
    for src, cnt in stats:
        print(f"    - {src}: {cnt}")
    conn.close()


def main():
    force = "--force" in sys.argv
    quiet = "--quiet" in sys.argv or "-q" in sys.argv
    if "--stats" in sys.argv:
        print_stats()
        return
    sync_exocortex(force=force, quiet=quiet)


if __name__ == "__main__":
    main()
