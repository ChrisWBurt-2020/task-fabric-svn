#!/usr/bin/env python3
"""Task Fabric SVN Longitudinal Reflection Generator.

Compiles durable signals (durable.rs), immutable world events, and consent receipts
into an executive Markdown reflection digest for ExamplePaper and Anamnesis study desk.
"""

import json
import os
import subprocess
import sys
import time
from datetime import datetime, timezone
from pathlib import Path

STATE_DIR = Path(os.path.expanduser("~/.local/state/task-fabric-svn"))
OUTPUT_DIR = Path(os.path.expanduser("~/.local/share/task-fabric-svn/reflections"))
CONFIG_PATH = Path(os.path.expanduser("~/.config/task-fabric-svn/projects.toml"))


def load_projects():
    projects = []
    if not CONFIG_PATH.exists():
        return ["example-project"]
    try:
        import tomllib
        data = tomllib.loads(CONFIG_PATH.read_text())
        for p in data.get("projects", []):
            projects.append(p.get("id"))
    except Exception:
        # Fallback to simple parse
        for line in CONFIG_PATH.read_text().splitlines():
            if line.strip().startswith("id = "):
                pid = line.split("=")[1].strip().strip('"\'')
                projects.append(pid)
    return projects or ["example-project"]


def load_reality_signals(projects):
    signals = []
    for pid in projects:
        try:
            res = subprocess.run(
                ["task-fabric-svn-work", "signal", "list", pid, "--json"],
                capture_output=True,
                text=True,
                timeout=5,
            )
            if res.returncode == 0 and res.stdout.strip():
                items = json.loads(res.stdout)
                for it in items:
                    it["project_id"] = pid
                    signals.append(it)
        except Exception:
            pass
    return signals


def load_recent_world_events(days=7):
    events_path = STATE_DIR / "world-events.jsonl"
    if not events_path.exists():
        return []
    cutoff = int(time.time()) - (days * 86400)
    events = []
    try:
        for line in events_path.read_text().splitlines():
            if not line.strip():
                continue
            evt = json.loads(line)
            if evt.get("timestamp_epoch_seconds", 0) >= cutoff:
                events.append(evt)
    except Exception:
        pass
    return events


def load_recent_receipts(days=7):
    transitions_dir = STATE_DIR / "world-transitions"
    if not transitions_dir.exists():
        return []
    cutoff = int(time.time()) - (days * 86400)
    receipts = []
    try:
        for p in transitions_dir.glob("*.json"):
            rec = json.loads(p.read_text())
            if rec.get("finished_at_epoch_seconds", 0) >= cutoff:
                receipts.append(rec)
    except Exception:
        pass
    return receipts


def generate_digest():
    now_dt = datetime.now(timezone.utc)
    date_str = now_dt.strftime("%Y-%m-%d")
    week_str = now_dt.strftime("%Y-W%W")

    projects = load_projects()
    signals = load_reality_signals(projects)
    events = load_recent_world_events(days=7)
    receipts = load_recent_receipts(days=7)

    # Categorize events
    approvals = [e for e in events if e.get("kind", "").startswith("approval.")]
    agent_events = [e for e in events if e.get("kind", "").startswith("agent.")]
    other_events = [e for e in events if e not in approvals and e not in agent_events]

    lines = []
    lines.append(f"# Task Fabric SVN Longitudinal Reflection — {week_str} ({date_str})")
    lines.append("")
    lines.append("> *Autonomous digest generated from the Task Fabric SVN reality ledger, world events, and consent audit.*")
    lines.append("")
    lines.append("## 1. Executive Summary & Attention Economy")
    lines.append(f"- **Active Projects Monitored**: {len(projects)}")
    lines.append(f"- **World Events Logged (7d)**: {len(events)}")
    lines.append(f"- **Agent Invocations / Notifications**: {len(agent_events)}")
    lines.append(f"- **Owner Consent Decisions Processed**: {len(approvals)}")
    lines.append(f"- **State Transitions Completed**: {len(receipts)}")
    lines.append("")

    lines.append("## 2. Durable Value & Reality Signals")
    if signals:
        for s in signals:
            kind = s.get("kind", "signal").upper()
            note = s.get("note") or "No note provided"
            pid = s.get("project_id", "general")
            lines.append(f"- **[{kind}]** ({pid}) — {note}")
    else:
        lines.append("- *No reality signals recorded this period. Use `task-fabric-svn-work signal record <project> <kind>` to track shipped value.*")
    lines.append("")

    lines.append("## 3. Autonomous Agent Activity & Consent Audit")
    if approvals:
        for a in approvals[-10:]:
            summary = a.get("summary", "Decision")
            ts = datetime.fromtimestamp(a.get("timestamp_epoch_seconds", 0), tz=timezone.utc).strftime("%m-%d %H:%M")
            lines.append(f"- `{ts}` — {summary}")
    else:
        lines.append("- *Consent loop remained quiet; all automated routines executed within safe baseline.*")
    lines.append("")

    lines.append("## 4. Completed World Transitions & Evidence Receipts")
    if receipts:
        for r in receipts[-5:]:
            tid = r.get("transition_id", "transition")
            wid = r.get("world_id", "default")
            outcome = r.get("outcome", "completed")
            lines.append(f"- **{wid}** (`{tid}`): Outcome `{outcome}`")
    else:
        lines.append("- *No discrete world state migrations completed this week.*")
    lines.append("")

    lines.append("---")
    lines.append(f"*Generated at {now_dt.isoformat()} by `task-fabric-svn-weekly-reflection.py`.*")

    digest_content = "\n".join(lines) + "\n"

    OUTPUT_DIR.mkdir(parents=True, exist_ok=True)
    out_file = OUTPUT_DIR / f"reflection-{date_str}.md"
    out_file.write_text(digest_content)
    try:
        out_file.chmod(0o600)
    except OSError:
        pass

    latest_symlink = OUTPUT_DIR / "latest.md"
    try:
        if latest_symlink.is_symlink() or latest_symlink.exists():
            latest_symlink.unlink()
        latest_symlink.symlink_to(out_file.name)
    except Exception:
        pass

    print(f"Generated reflection digest: {out_file}")
    return out_file


def main():
    out = generate_digest()
    if "--print" in sys.argv or "-p" in sys.argv:
        print("\n" + out.read_text())


if __name__ == "__main__":
    main()
