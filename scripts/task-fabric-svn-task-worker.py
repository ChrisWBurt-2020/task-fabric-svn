#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.11"
# dependencies = []
# ///
"""Task Fabric SVN Task Fabric worker -- OpenCode adapter (build step 4).

Claims a task from the durable ledger and executes it by driving
`opencode run` in the task's target repo. This is the first worker
adapter; it proves the full loop:

    submit -> starting -> working -> input_required -> working -> completed

plus the approval detour:

    working -> approval_required -> working (approved) | cancelled (denied)

v1 worker<->harness contract (documented here because it is the protocol):
  - The worker tells opencode: if you need a human decision, end your turn
    with exactly `TASK_FABRIC_SVN_QUESTION: <question>` and nothing after it.
  - If you need the human to approve a specific action before you take it,
    end your turn with exactly `TASK_FABRIC_SVN_APPROVAL: <short description of the
    action>` and nothing after it.
  - The worker moves the task to input_required with that question, then
    polls `task-fabric-svn-work task inspect` until the task leaves input_required
    (someone ran `task respond`, or it was cancelled / timed out).
  - The worker moves the task to approval_required with the action
    description, then polls until the human runs `task approve` (yes/no,
    with an optional free-text write-in carried as the approval reason).
  - On resume the worker re-invokes opencode with the human's answer (for
    questions) or the approval plus any write-in constraint (for
    approvals), in the same session. A denial ends the task terminally:
    the ledger maps denial to Cancelled, and the worker asks the human to
    record the reason as `approval_denied: <why>`.
  - `TASK_FABRIC_SVN_DONE: <result summary>` ends the turn successfully; the worker
    completes the task with that summary plus any new/modified repo files
    as artifact refs (via `git status`).
  - Anything else (nonzero exit, timeout, no marker) fails the task with
    an honest reason. No silent retries, no invented results.
  - Precedence when several markers appear on one turn: a question wins
    over an approval, which wins over done. A turn that asks something (or
    requests approval) is not done.

The pure helpers below (marker extraction, turn classification, prompt and
env construction, opencode event parsing, artifact-ref sanitization, and
resume/session adoption) are covered by unit tests in
scripts/tests/test_task_fabric_svn_task_worker.py.

Usage:
    task-fabric-svn-task-worker.py <task-id> [--model provider/model]
                         [--max-iterations N] [--run-timeout S]
                         [--poll-interval S] [--deadline S] [--dir PATH]
                         [--env-file PATH] [--resume]

Defaults: --model litellm/free-roulette-big (free; the -fast roulette's
small-context endpoints overflow on opencode's system prompt), --max-iterations 5,
--run-timeout 900, --poll-interval 15, --deadline 3600.
"""

from __future__ import annotations

import argparse
import json
import os
import random
import re
import secrets
import shutil
import subprocess
import sys
import time
import tomllib
from pathlib import Path
from typing import NoReturn

sys.path.insert(0, str(Path(__file__).resolve().parent))
from task_containment import ContainmentError, TaskContainment, load_config

STATE_DIR = Path(os.path.expanduser("~/.local/state/task-fabric-svn"))
PROJECTS_TOML = Path(os.path.expanduser("~/.config/task-fabric-svn/projects.toml"))
QUESTION_MARKER = "TASK_FABRIC_SVN_QUESTION:"
APPROVAL_MARKER = "TASK_FABRIC_SVN_APPROVAL:"
DONE_MARKER = "TASK_FABRIC_SVN_DONE:"
SUMMARY_LIMIT = 4000
SUPPORTED_TASK_HARNESS = "opencode"


def validate_task_harness(task: dict) -> None:
    """Reject unsupported or legacy-unbound routes before claim and execution."""
    if task.get("harness") != SUPPORTED_TASK_HARNESS:
        raise ValueError("unsupported_harness: this Phase 1 worker accepts only tasks explicitly bound to opencode")


def die(msg: str, code: int = 1) -> NoReturn:
    print(f"task-fabric-svn-task-worker: error: {msg}", file=sys.stderr)
    sys.exit(code)


def run_task_fabric_svn_work(*args: str) -> dict:
    """Run `task-fabric-svn-work task ...`, return parsed stdout JSON. Raises on failure."""
    exe = shutil.which("task-fabric-svn-work")
    if not exe:
        die("task-fabric-svn-work not found on PATH")
    proc = subprocess.run(
        [exe, "task", *args],
        capture_output=True,
        text=True,
        timeout=60,
    )
    if proc.returncode != 0:
        raise RuntimeError(f"task-fabric-svn-work task {' '.join(args)} failed: {proc.stderr.strip()}")
    try:
        return json.loads(proc.stdout)
    except json.JSONDecodeError as e:
        raise RuntimeError(f"task-fabric-svn-work printed non-JSON: {e}\n{proc.stdout[:500]}")


def inspect_task(task_id: str) -> dict:
    out = run_task_fabric_svn_work("inspect", task_id)
    return out["task"]


def note(task_id: str, text: str) -> None:
    try:
        run_task_fabric_svn_work("note-progress", task_id, "--note", text[:2000])
    except Exception as e:  # progress notes must never kill the worker
        print(f"warning: note-progress failed: {e}", file=sys.stderr)


def note_if_working(task_id: str, task: dict, text: str) -> None:
    """Append a progress note, but only while the task is in Working.

    The ledger rejects note-progress for any other state (e.g.
    input_required, starting), so calling it there only prints a
    warning. Skip it instead.
    """
    if task.get("status") == "working":
        note(task_id, text)


def resolve_repo_dir(target_repo_identity: str | None, explicit_dir: str | None) -> Path:
    if explicit_dir:
        d = Path(explicit_dir).expanduser()
        if not d.is_dir():
            die(f"--dir is not a directory: {d}")
        return d
    if not target_repo_identity:
        die("task has no target_repo_identity and no --dir given")
    try:
        cfg = tomllib.load(open(PROJECTS_TOML, "rb"))
    except FileNotFoundError:
        die(f"projects.toml not found at {PROJECTS_TOML}")
    for p in cfg.get("projects", []):
        if p.get("id") == target_repo_identity:
            repo = p.get("repo")
            if repo and Path(repo).is_dir():
                return Path(repo)
            die(f"project '{target_repo_identity}' has no usable repo dir: {repo!r}")
    die(f"no project with id '{target_repo_identity}' in {PROJECTS_TOML}")


def changed_files(repo_dir: Path) -> list[str]:
    """Files the harness created or modified, for artifact refs. Honest: git-observed only."""
    try:
        proc = subprocess.run(
            ["git", "status", "--porcelain"],
            cwd=repo_dir,
            capture_output=True,
            text=True,
            timeout=30,
        )
    except Exception:
        return []
    if proc.returncode != 0:
        return []
    files = []
    for line in proc.stdout.splitlines():
        line = line.rstrip()
        if len(line) > 3 and line[2] == " ":
            files.append(line[3:].strip().strip('"'))
    return sorted(set(files))[:20]


def sanitize_artifact_ref(path: str) -> str:
    """Map a repo-relative path to a ledger-safe artifact ref.

    The ledger rejects refs containing '/', '\\' or '..' ("not a safe id"),
    so path separators become ':':  scripts/foo.py -> repo:scripts:foo.py
    """
    safe = path.replace("\\", ":").replace("/", ":")
    while ".." in safe:
        safe = safe.replace("..", ".")
    safe = "".join(c for c in safe if c == "\t" or ord(c) >= 32).strip()
    return f"repo:{safe}" if safe else "repo:unknown"


def artifact_refs_for(repo_dir: Path) -> list[str]:
    return [sanitize_artifact_ref(f) for f in changed_files(repo_dir)]


def build_prompt(task: dict, answer: str | None, resumed: bool,
                 resume_kind: str = "question") -> str:
    tid = task.get("task_id", "?")
    intent = task.get("intent_summary", "")
    # Task records serialize this field as `parameters`; accept the old
    # `typed_parameters` spelling as a compatibility fallback.
    params = task.get("parameters")
    if params is None:
        params = task.get("typed_parameters", {})
    if resumed:
        lead = answer if resume_kind == "approval" else f"The human answered your question: {answer}"
        return (
            f"You are continuing Task Fabric SVN task {tid}.\n"
            f"{lead}\n\n"
            "Continue the work. If you need another human decision, end your turn with exactly:\n"
            f"{QUESTION_MARKER} <your question>\n"
            "and nothing after it. If you need the human to approve a specific action before you take it, end your turn with exactly:\n"
            f"{APPROVAL_MARKER} <short description of the action>\n"
            "and nothing after it. When the work is fully done, end with exactly:\n"
            f"{DONE_MARKER} <one-paragraph result summary>\n"
            "and nothing after it."
        )
    return (
        f"You are executing Task Fabric SVN task {tid}: {intent}\n"
        f"Typed parameters: {json.dumps(params)}\n\n"
        "Work in the current directory. Keep changes minimal and honest; do not invent results.\n"
        "If you need a decision from the human to proceed, end your final message with exactly:\n"
        f"{QUESTION_MARKER} <your question>\n"
        "and nothing after it.\n"
        "If you need the human to approve a specific action before you take it, end your final message with exactly:\n"
        f"{APPROVAL_MARKER} <short description of the action>\n"
        "and nothing after it.\n"
        "When the work is fully done, end your final message with exactly:\n"
        f"{DONE_MARKER} <one-paragraph result summary>\n"
        "and nothing after it.\n"
        "Do not print any marker unless you mean it."
    )


def load_env_file(path: str | None) -> dict[str, str]:
    """Load KEY=VALUE lines (KEY only, values never logged) for the harness env."""
    if not path:
        return {}
    env: dict[str, str] = {}
    try:
        with open(Path(path).expanduser()) as f:
            for line in f:
                line = line.strip()
                if not line or line.startswith("#") or "=" not in line:
                    continue
                k, v = line.split("=", 1)
                k = k.strip()
                v = v.strip().strip('"').strip("'")
                if k and v:
                    env[k] = v
    except FileNotFoundError:
        print(f"warning: env file not found: {path}", file=sys.stderr)
    return env


def apply_harness_env(harness_env: dict[str, str]) -> bool:
    """Prefer the litellm proxy master key as the litellm provider credential.

    The opencode `litellm` provider reads {env:OPENAI_API_KEY}; the
    OPENAI_API_KEY entry in litellm.env is stale (not in the proxy's token
    table), while LITELLM_MASTER_KEY is always a valid bearer credential.
    Values are never logged. Returns True when the mapping was applied.
    """
    if "LITELLM_MASTER_KEY" in harness_env:
        harness_env["OPENAI_API_KEY"] = harness_env["LITELLM_MASTER_KEY"]
        return True
    return False


def fail_task(task_id: str, reason: str, detail: str = "") -> None:
    """Fail with the detail folded into the reason (fail takes --reason only)."""
    text = reason if not detail else f"{reason}: {detail}"
    run_task_fabric_svn_work("fail", task_id, "--reason", text[:2000])


def build_opencode_cmd(opencode_exe: str, repo_dir: Path, model: str, prompt: str,
                       session_id: str | None, pure: bool) -> list[str]:
    """Build the `opencode run` argv.

    The first turn omits --session (opencode creates the session); later
    turns pass the real session id back so the same conversation continues.
    """
    cmd = [opencode_exe, "run", "--format", "json", "--dir", str(repo_dir),
           "--model", model]
    if pure:
        cmd.append("--pure")
    if session_id:
        cmd += ["--session", session_id]
    cmd.append(prompt)
    return cmd


def parse_opencode_stdout(stdout: str) -> tuple[str | None, str, list[str]]:
    """Parse `opencode run --format json` event lines.

    Returns (session_id, assistant_text, errors): the session id from the
    top-level `sessionID` envelope, assistant text reassembled from `text`
    events, and provider error messages. If no text events were seen, the
    raw stdout is returned as the text (markers contain no JSON-special
    characters, so the marker scan still works on it).
    """
    sid: str | None = None
    texts: list[str] = []
    errors: list[str] = []
    for line in stdout.splitlines():
        line = line.strip()
        if not line.startswith("{"):
            continue
        try:
            d = json.loads(line)
        except json.JSONDecodeError:
            continue
        if isinstance(d.get("sessionID"), str):
            sid = d["sessionID"]
        etype = d.get("type")
        if etype == "text":
            t = (d.get("part") or {}).get("text")
            if isinstance(t, str):
                texts.append(t)
        elif etype == "error":
            err = d.get("error") or {}
            data = err.get("data") or {}
            msg = data.get("message") if isinstance(data, dict) else str(data)
            errors.append(f"{err.get('name', 'error')}: {str(msg)[:300]}")
    text = "".join(texts)
    if not text:
        # Fallback: scan raw stdout for marker-shaped content (markers survive
        # JSON escaping since they contain no special characters).
        text = stdout
    return sid, text, errors


def run_opencode(repo_dir: Path, model: str, prompt: str, run_timeout: int,
                 extra_env: dict[str, str] | None = None,
                 session_id: str | None = None,
                 pure: bool = True, containment=None,
                 cancelled=lambda: False) -> tuple[int, str | None, str, list[str]]:
    """Run only inside the task boundary; never read/inherit provider secrets."""
    if containment is None or extra_env or not pure:
        return 126, session_id, "", ["containment_required: unrestricted harness execution unavailable"]
    cmd = build_opencode_cmd("/opt/task_fabric_svn/opencode", Path("/workspace"),
                             "task-fabric-svn-contained/" + model, prompt, session_id, True)
    try:
        rc, stdout, error = containment.run(cmd, run_timeout, cancelled)
    except ContainmentError:
        return 126, session_id, "", ["containment_unavailable: task was not launched"]
    sid, text, errors = parse_opencode_stdout(stdout)
    if rc != 0:
        # Do not copy provider responses or raw harness stderr into task logs.
        errors = ["contained_harness_cancelled" if rc == 125 else
                  "contained_harness_timeout" if rc == 124 else "contained_harness_failed"]
    return rc, sid or session_id, text, errors


def parse_session_id(stdout: str) -> str | None:
    for line in stdout.splitlines():
        line = line.strip()
        if not line.startswith("{"):
            continue
        try:
            d = json.loads(line)
        except json.JSONDecodeError:
            continue
        if isinstance(d.get("sessionID"), str):
            return d["sessionID"]
    return None


def normalize_marker_line(line: str) -> str:
    """Strip leading markdown wrappers so ``**TASK_FABRIC_SVN_QUESTION:** ...`` still matches."""
    s = line.strip()
    while True:
        old = s
        for tok in ("**", "__", "*", "_", ">", "-", "#"):
            if s.startswith(tok):
                s = s[len(tok):].lstrip()
        if s == old:
            break
    return s


def extract_marker(output: str, marker: str) -> str | None:
    """Find the LAST normalized line starting with the marker.

    Returns the text after the marker. Tolerates the model wrapping the
    marker in markdown bold (``**TASK_FABRIC_SVN_QUESTION:** ...``), including a
    paired trailing closer (``**TASK_FABRIC_SVN_DONE:** ... **``).
    """
    found = None
    for line in output.splitlines():
        s = normalize_marker_line(line)
        if not s.startswith(marker):
            continue
        rest = s[len(marker):].strip()
        # Drop one markdown emphasis wrapper that survived normalization.
        # Only strip a trailing closer when a leading opener of the same
        # token was stripped (paired emphasis), so a legitimate trailing
        # "*" in the payload is left alone.
        for tok in ("**", "__", "*", "_"):
            if rest.startswith(tok):
                rest = rest[len(tok):].lstrip()
                if rest.endswith(tok) and len(rest) > len(tok):
                    rest = rest[: -len(tok)].rstrip()
                break
        found = rest
    return found or None


def classify_turn(question: str | None, approval: str | None,
                  done_summary: str | None) -> tuple[str, str | None]:
    """Decide the outcome of one harness turn.

    Returns (action, payload) where action is one of:
      "question"       -- the harness asked for human input (payload: the question)
      "approval"       -- the harness proposed a gated action (payload: the action description)
      "done"           -- the harness finished (payload: the result summary)
      "missing_marker" -- clean output but no protocol marker (payload: None)

    A turn that asks something is not done: a question takes precedence over
    an approval, which takes precedence over done.
    """
    if question:
        return ("question", question)
    if approval:
        return ("approval", approval)
    if done_summary:
        return ("done", done_summary)
    return ("missing_marker", None)


def build_approval_resume_text(action: str, write_in: str | None) -> str:
    """Text injected into the resume prompt after the human approves.

    The write-in is the human's free-text reason from `task approve
    --reason`; it is passed to the harness as a hard constraint.
    """
    text = f"The human APPROVED your proposed action: {action}."
    if write_in:
        text += (f" Their written instruction, which you must follow as a hard "
                 f"constraint: {write_in}")
    else:
        text += " Proceed with the action."
    return text


def latest_approval_resolution(task_id: str) -> tuple[bool, str | None, str | None] | None:
    """Return the latest approval gate outcome, or None if never resolved.

    Returns (approved, action, write_in): the approved flag and the human's
    free-text reason from the latest `approval_resolved` history event, plus
    the action description from the matching `approval_required` event.
    Never raises; returns None when the history lookup fails.
    """
    try:
        hist = run_task_fabric_svn_work("history", task_id)
    except Exception:
        return None
    approved: bool | None = None
    write_in: str | None = None
    action: str | None = None
    for ev in reversed(hist.get("events", [])):
        kind = ev.get("kind")
        payload = ev.get("payload", {}) or {}
        if approved is None and kind == "approval_resolved":
            approved = bool(payload.get("approved"))
            write_in = payload.get("reason")
        elif approved is not None and action is None and kind == "approval_required":
            action = payload.get("action")
            break
    if approved is None:
        return None
    return (approved, action, write_in)


def latest_answer(task_id: str) -> str | None:
    """Return the most recent human answer from task history, if any."""
    hist = run_task_fabric_svn_work("history", task_id)
    for ev in reversed(hist.get("events", [])):
        if ev.get("kind") == "input_responded":
            return ev.get("payload", {}).get("answer")
    return None


def adopted_session_id(task_id: str) -> str | None:
    """Recover the opencode session id from the worker's own progress notes."""
    try:
        hist = run_task_fabric_svn_work("history", task_id)
    except Exception:
        return None
    sid = None
    for ev in hist.get("events", []):
        # note-progress emits kind "progress" with a {"note": ...} payload
        if ev.get("kind") not in ("progress", "progress_noted"):
            continue
        m = re.search(r"opencode session (ses_[A-Za-z0-9]+)",
                      str(ev.get("payload", {}).get("note", "")))
        if m:
            sid = m.group(1)
    return sid


def wait_for_response(task_id: str, poll_interval: int, deadline_ts: float) -> dict:
    """Poll until the task leaves input_required/approval_required. Returns fresh record."""
    while True:
        if time.time() > deadline_ts:
            fail_task(task_id, "deadline_exceeded",
                      "deadline exceeded while waiting for human response")
            die("deadline exceeded while waiting for human response", code=3)
        time.sleep(poll_interval)
        try:
            task = inspect_task(task_id)
        except Exception as e:
            print(f"warning: inspect during wait failed: {e}", file=sys.stderr)
            continue
        if task["status"] not in ("input_required", "approval_required"):
            return task
        print(f"  ... waiting (status={task['status']})", flush=True)


def main() -> int:
    ap = argparse.ArgumentParser(description="Task Fabric SVN Task Fabric worker (OpenCode adapter)")
    ap.add_argument("task_id")
    ap.add_argument("--model", default=None,
                    help="opencode model (default: litellm/free-roulette-big; the -fast "
                         "roulette's small-context endpoints overflow on opencode's "
                         "system prompt)")
    ap.add_argument("--max-iterations", type=int, default=5)
    ap.add_argument("--run-timeout", type=int, default=900)
    ap.add_argument("--poll-interval", type=int, default=15)
    ap.add_argument("--deadline", type=int, default=3600)
    ap.add_argument("--dir", default=None)
    ap.add_argument("--resume", action="store_true",
                    help="rejoin a task this worker already claimed (status working, "
                         "starting, or input_required); skips the start/begin-work "
                         "claim step and adopts the opencode session from notes")
    ap.add_argument("--containment-config", default=None,
                    help="Explicit owner-controlled manifest for a task-scoped inference proxy; required")
    ap.add_argument("--env-file", default=None,
                    help="KEY=VALUE file whose entries are added to the harness "
                         "environment (e.g. provider keys). Values are never logged.")
    ap.add_argument("--no-pure", action="store_true",
                    help="run opencode with plugins/MCP (default is --pure: no "
                         "external plugins, for a predictable worker)")
    args = ap.parse_args()

    task_id = args.task_id
    deadline_ts = time.time() + args.deadline
    if args.env_file or args.no_pure:
        die("unrestricted_harness_options_unavailable: use a contained inference proxy")
    session_id = f"harness-{task_id.replace('task-', '')[:8]}-{random.randint(1000, 9999)}"

    # --- claim ---
    try:
        task = inspect_task(task_id)
    except Exception as e:
        die(f"cannot inspect task: {e}")
    try:
        validate_task_harness(task)
    except ValueError as e:
        die(str(e))
    if task["status"] != "queued":
        if not (args.resume and task["status"] in ("working", "starting", "input_required",
                                                   "approval_required")):
            die(f"task is {task['status']}, not queued -- refusing to steal it "
                f"(use --resume only to rejoin a task this worker already claimed)")
    repo_dir = resolve_repo_dir(task.get("target_repo_identity"), args.dir)
    try:
        config = load_config(args.containment_config, task_id, task.get("trace_id"), args.model)
        containment = TaskContainment(repo_dir, STATE_DIR / "task-workspaces", task_id,
                                      config, resume=args.resume)
    except (ContainmentError, OSError, ValueError, subprocess.SubprocessError):
        # Refuse before task claim or any model execution. No fallback route.
        die("containment_unavailable: valid task proxy, manifest and isolated snapshot required")
    opencode_session = adopted_session_id(task_id) if args.resume else None
    answer = None
    resume_kind = "question"
    resumed_with_answer = False
    if args.resume:
        print(f"task {task_id}: resuming previously claimed task (status={task['status']}, "
              f"session={opencode_session})", flush=True)
        # note-progress requires Working; skip the note for other states
        # (e.g. input_required, approval_required) instead of printing a warning.
        note_if_working(task_id, task, "worker resumed after restart; rejoining claimed task")
        if task["status"] in ("input_required", "approval_required"):
            waiting_on = task["status"]
            task = wait_for_response(task_id, args.poll_interval, deadline_ts)
            if task["status"] in ("cancelled", "failed"):
                print(f"task ended ({task['status']}) while waiting; worker stopping", flush=True)
                return 0
            if waiting_on == "input_required":
                answer = latest_answer(task_id)
                if not answer and task["status"] == "working":
                    answer = "(the human resumed the task without a recorded answer; continue with your best judgment)"
                resume_kind = "question"
            else:
                resolution = latest_approval_resolution(task_id)
                if resolution is None or not resolution[0]:
                    print(f"task {task_id}: approval was denied while the worker was down; stopping",
                          flush=True)
                    return 0
                _, action_text, write_in = resolution
                fail_task(task_id, "unsupported_escalation",
                          "approval cannot widen the contained worker boundary")
                return 3
            resumed_with_answer = True
            print(f"task {task_id}: resuming opencode ({waiting_on} resolved)", flush=True)
    else:
        print(f"task {task_id}: claiming (repo={repo_dir}, session={session_id})", flush=True)
        try:
            run_task_fabric_svn_work("start", task_id, "--harness-session-id", session_id)
            run_task_fabric_svn_work("begin-work", task_id)
        except Exception as e:
            die(f"claim failed: {e}")
        note(task_id, f"worker claimed task; opencode session {session_id} in {repo_dir}")

    # --- work loop ---
    # opencode_session/answer may already be set by --resume (adopted session,
    # or an answer that arrived while the worker was down).
    for iteration in range(1, args.max_iterations + 1):
        if time.time() > deadline_ts:
            fail_task(task_id, "deadline_exceeded", "worker deadline exceeded")
            die("deadline exceeded", code=3)
        continued = iteration > 1 or resumed_with_answer
        print(f"task {task_id}: iteration {iteration} (model={args.model})", flush=True)
        task = inspect_task(task_id)
        note_if_working(task_id, task, f"iteration {iteration}: invoking opencode ({args.model})")
        prompt = build_prompt(task, answer, resumed=continued, resume_kind=resume_kind)
        next_status_check = 0.0
        def task_cancelled():
            nonlocal next_status_check
            if time.monotonic() < next_status_check:
                return False
            next_status_check = time.monotonic() + 1.0
            try:
                return inspect_task(task_id)["status"] != "working"
            except Exception:
                return True  # authority unavailable: stop, never continue blindly
        rc, new_session, text, errors = run_opencode(
            repo_dir, args.model, prompt, args.run_timeout,
            session_id=opencode_session, containment=containment, cancelled=task_cancelled)
        if rc == 125:
            print(f"task {task_id}: contained work stopped; task authority changed", flush=True)
            return 0
        if new_session and new_session != opencode_session:
            opencode_session = new_session
            note_if_working(task_id, task, f"opencode session {opencode_session}")
        transcript_tail = text[-3000:]
        err_summary = "; ".join(errors[:3])
        if rc == 124:
            fail_task(task_id, "harness_timeout",
                      f"opencode run timed out after {args.run_timeout}s: {transcript_tail[-500:]}")
            die("opencode run timed out", code=3)
        if rc != 0:
            detail = (err_summary or transcript_tail[-2000:])[-2000:]
            fail_task(task_id, "harness_error", f"opencode exited {rc}: {detail}")
            die(f"opencode exited {rc}", code=3)

        question = extract_marker(text, QUESTION_MARKER)
        approval = extract_marker(text, APPROVAL_MARKER)
        done_summary = extract_marker(text, DONE_MARKER)
        action, payload = classify_turn(question, approval, done_summary)
        if (question or approval) and done_summary:
            note_if_working(task_id, task, "harness emitted multiple markers; precedence: question > approval > done")
        if action == "question":
            assert payload is not None
            print(f"task {task_id}: harness asked: {payload}", flush=True)
            # Note BEFORE request-input: the task is still working here; after
            # request-input it is input_required and the note would warn.
            note_if_working(task_id, task, "harness asked a question; requesting human input")
            resume_token = secrets.token_hex(16)
            run_task_fabric_svn_work("request-input", task_id,
                           "--question", payload[:2000],
                           "--resume-token", resume_token)
            # The token is the capability the human needs to answer. It lives in
            # the task record and the worker log; it is single-use per question.
            print(f"task {task_id}: INPUT REQUIRED -- respond with:\n"
                  f"  task-fabric-svn-work task respond {task_id} "
                  f"--resume-token {resume_token} --answer \"...\"",
                  flush=True)
            task = wait_for_response(task_id, args.poll_interval, deadline_ts)
            if task["status"] == "cancelled":
                print("task was cancelled while waiting; worker stopping", flush=True)
                return 0
            if task["status"] == "failed":
                print("task failed externally; worker stopping", flush=True)
                return 0
            # find the latest response
            answer = latest_answer(task_id)
            if not answer and task["status"] == "working":
                answer = "(the human resumed the task without a recorded answer; continue with your best judgment)"
            resume_kind = "question"
            print(f"task {task_id}: resuming with answer", flush=True)
            note_if_working(task_id, task, "human responded; resuming opencode")
            continue
        if action == "approval":
            assert payload is not None
            action_desc = payload
            print(f"task {task_id}: harness requested approval: {action_desc}", flush=True)
            # Note BEFORE request-approval: the task is still working here;
            # after request-approval it is approval_required and the note would warn.
            note_if_working(task_id, task, "harness proposed a gated action; requesting approval")
            run_task_fabric_svn_work("request-approval", task_id, "--action", action_desc[:2000])
            # The human resolves the gate with `task approve`. The free-text
            # write-in travels as --reason; denial maps to Cancelled in the
            # ledger, so the convention is to record it as "approval_denied: ...".
            print(f"task {task_id}: APPROVAL REQUIRED -- respond with:\n"
                  f"  task-fabric-svn-work task approve {task_id} --approved true [--reason \"write-in instruction\"]\n"
                  f"  task-fabric-svn-work task approve {task_id} --approved false --reason \"approval_denied: <why>\"",
                  flush=True)
            task = wait_for_response(task_id, args.poll_interval, deadline_ts)
            if task["status"] == "cancelled":
                resolution = latest_approval_resolution(task_id)
                if resolution is not None and not resolution[0]:
                    print(f"task {task_id}: approval denied "
                          f"({resolution[2] or 'no reason given'}); worker stopping", flush=True)
                else:
                    print(f"task {task_id}: cancelled while waiting for approval; worker stopping",
                          flush=True)
                return 0
            if task["status"] == "failed":
                print("task failed externally; worker stopping", flush=True)
                return 0
            # Approved: the task is working again. Resume the same session
            # with the approval and any write-in constraint.
            resolution = latest_approval_resolution(task_id)
            resolved_action = (resolution[1] if resolution and resolution[1]
                               else action_desc)
            write_in = resolution[2] if resolution else None
            # Approval does not widen the process/filesystem/network boundary.
            fail_task(task_id, "unsupported_escalation",
                      "protected and external action execution is unavailable in this contained adapter")
            return 3
            resume_kind = "approval"
            resumed_with_answer = True
            print(f"task {task_id}: approval granted; resuming opencode", flush=True)
            note_if_working(task_id, task, "approval granted; resuming opencode")
            continue
        if action == "done":
            assert payload is not None
            try:
                manifest, changes = containment.finish()
            except (ContainmentError, OSError):
                fail_task(task_id, "unsafe_contained_output", "snapshot output cannot be exported")
                return 3
            summary = "Contained result staged; source checkout unchanged. " + payload
            complete_args = ["complete", task_id, "--result-summary", summary[:SUMMARY_LIMIT],
                             "--artifact-ref", "contained:" + task_id + ":result"]
            for name in changes[:20]:
                complete_args += ["--artifact-ref", "contained:" + sanitize_artifact_ref(name)]
            try:
                run_task_fabric_svn_work(*complete_args)
            except Exception as e:
                fail_task(task_id, "harness_error",
                          f"task completion rejected by ledger: {e}")
                die(f"complete failed: {e}", code=3)
            print(f"task {task_id}: COMPLETED", flush=True)
            return 0
        # action == "missing_marker": a clean exit with no protocol marker is a
        # harness contract violation. Completing from the transcript would mean
        # inventing a result, so fail terminally instead.
        fail_task(task_id, "missing_completion_marker",
                  "opencode exited 0 with no TASK_FABRIC_SVN_QUESTION/TASK_FABRIC_SVN_APPROVAL/TASK_FABRIC_SVN_DONE marker; "
                  f"transcript tail: {text.strip()[-500:]}")
        die("opencode exited cleanly without a completion marker", code=3)

    fail_task(task_id, "max_iterations",
              f"exceeded {args.max_iterations} iterations without TASK_FABRIC_SVN_DONE")
    die("max iterations exceeded", code=3)


if __name__ == "__main__":
    sys.exit(main())
