#!/usr/bin/env python3
"""Fabric ledger terminal-state reconciliation verifier for task-fabric-svn.

The durable task ledger (`~/.local/state/task-fabric-svn`: one JSON snapshot
per task under `tasks/`, plus the append-only `task-events.jsonl` journal) is
treated across the estate as the record of what happened. Nothing reconciled
its terminal states against external reality (branch / commit / PR), so the
ledger could record success as failure and nobody would know: on 2026-10-16
task-7992bae48730 (heron-field issue #79, runAlignWords) was approved in chat,
the contained worker's boundary refused the widened action, and the ledger
closed the task `failed` ("unsupported_escalation") -- while the parent
fulfilled the work externally: commit 1417b03 pushed, draft PR #80 opened and
later merged. The ledger still says `failed`. The estate audit likewise flags
chat-approved work as orphaned because "it only reads ledger state, it can't
know about chat approvals", and the resolved approval card for that task still
sits in `pending-approvals/`, ownerless.

This verifier reconciles, read-only:

  * per TERMINAL task (completed / failed / cancelled), it extracts the
    task's external claims (issue, branch, PR, commit) from the record and
    its journal events, then probes external reality -- the registered
    repository's local checkout (git refs, named-commit ancestry vs
    origin/main) and GitHub PRs via `gh` -- and emits a ledger-vs-external
    row per task;
  * a failed/cancelled task with external success evidence (a merged PR, or
    a named commit on origin/main) is a MISMATCH. It is explained only by an
    appended `fulfilled_externally` journal record naming the fulfiller and
    the PR; otherwise the run is RECONCILE_BLOCKED and the ledger is NOT
    trusted for that task. The ledger is never silently rewritten -- this
    tool writes nothing into the state dir; `--emit-records` writes the
    proposed record lines to a caller-chosen file outside it, for an
    operator to append. (The append itself is the ledger CLI's
    `task record-fulfilled-externally` subcommand -- `note_progress`
    requires `working`, so terminal tasks needed their own path; the
    record shape is defined here and recognized on read-back.)
  * a terminal task with no evidence record anywhere (no artifact/receipt
    refs, no result/failure reason, no external trace) FAILS by name;
  * a NON-terminal task whose last activity is older than `--stale-hours`
    FAILS by name (stuck, ownerless);
  * pending-approval cards are aged against the ledger: a card whose task
    is already terminal is STALE (resolved -- age out), a card naming a
    task the ledger does not hold is an ORPHAN; both FAIL by name.

Fail-closed throughout: an unreadable ledger (missing state dir, missing
journal, one unparseable snapshot) or a probe that cannot run for a task
carrying an external claim reads UNVERIFIED -- never "reconciled".

Exit codes: 0 reconciled / 1 RECONCILE_BLOCKED (findings) / 2 UNVERIFIED.
Exit 2 dominates: a run that could not check must not masquerade as a
completed reconciliation, even a failing one.
"""

from __future__ import annotations

import argparse
import json
import re
import subprocess
import sys
import time
import tomllib
from dataclasses import dataclass, field
from pathlib import Path

TERMINAL_STATES = frozenset({"completed", "failed", "cancelled"})
KNOWN_STATES = TERMINAL_STATES | frozenset(
    {"queued", "starting", "working", "input_required", "approval_required"}
)

DEFAULT_STALE_HOURS = 24.0
GH_PR_LIMIT = 1000


class Unverified(Exception):
    """The ledger or a required probe could not be read -- fail closed."""


# ---------------------------------------------------------------------------
# Ledger loading
# ---------------------------------------------------------------------------

@dataclass
class LedgerData:
    tasks: dict[str, dict]                 # task_id -> snapshot record
    events: dict[str, list[dict]]          # task_id -> journal events
    journal_path: Path


def _load_json_file(path: Path, what: str) -> dict:
    try:
        doc = json.loads(path.read_text())
    except (OSError, json.JSONDecodeError) as e:
        raise Unverified(f"{what} unreadable: {path}: {e}")
    if not isinstance(doc, dict):
        raise Unverified(f"{what} is not a JSON object: {path}")
    return doc


def load_ledger(state_dir: Path) -> LedgerData:
    if not state_dir.is_dir():
        raise Unverified(f"ledger state dir missing: {state_dir}")
    tasks_dir = state_dir / "tasks"
    if not tasks_dir.is_dir():
        raise Unverified(f"ledger tasks dir missing: {tasks_dir}")
    journal_path = state_dir / "task-events.jsonl"
    if not journal_path.is_file():
        raise Unverified(f"ledger journal missing: {journal_path}")

    tasks: dict[str, dict] = {}
    for snap in sorted(tasks_dir.glob("*.json")):
        rec = _load_json_file(snap, "task snapshot")
        task_id = rec.get("task_id")
        status = rec.get("status")
        if not isinstance(task_id, str) or not task_id:
            raise Unverified(f"task snapshot has no task_id: {snap}")
        if status not in KNOWN_STATES:
            raise Unverified(
                f"task snapshot {snap} has unknown status {status!r} -- "
                "this verifier does not know that state; NOT reconciled")
        if task_id in tasks:
            raise Unverified(f"duplicate snapshot for {task_id}: {snap}")
        tasks[task_id] = rec

    events: dict[str, list[dict]] = {}
    try:
        lines = journal_path.read_text().splitlines()
    except OSError as e:
        raise Unverified(f"ledger journal unreadable: {journal_path}: {e}")
    for lineno, line in enumerate(lines, 1):
        if not line.strip():
            continue
        try:
            ev = json.loads(line)
        except json.JSONDecodeError as e:
            raise Unverified(
                f"ledger journal line {lineno} unparseable: {e} -- "
                "history is incomplete; NOT reconciled")
        if not isinstance(ev, dict) or not isinstance(ev.get("task_id"), str):
            raise Unverified(
                f"ledger journal line {lineno} is not a task event object")
        events.setdefault(ev["task_id"], []).append(ev)
    return LedgerData(tasks=tasks, events=events, journal_path=journal_path)


# ---------------------------------------------------------------------------
# Repository registry (config/task-repositories.toml, supervisor's format)
# ---------------------------------------------------------------------------

@dataclass
class RepoEntry:
    identity: str
    path: Path
    fingerprint: str | None
    owner_repo: str | None       # "owner/name" parsed from the fingerprint


_GITHUB_URL_RE = re.compile(r"github\.com[:/]([^/]+)/([^/]+?)(?:\.git)?$")


def load_registry(registry_path: Path) -> dict[str, RepoEntry]:
    try:
        with open(registry_path, "rb") as f:
            doc = tomllib.load(f)
    except FileNotFoundError:
        raise Unverified(f"repository registry not found: {registry_path}")
    except tomllib.TOMLDecodeError as e:
        raise Unverified(f"repository registry is not valid TOML: {e}")
    repos = doc.get("repositories")
    if not isinstance(repos, dict):
        raise Unverified(f"registry has no [repositories] table: {registry_path}")
    entries: dict[str, RepoEntry] = {}
    for identity, raw in repos.items():
        if not isinstance(raw, dict) or not raw.get("path"):
            raise Unverified(f"registry entry '{identity}' has no path")
        fingerprint = raw.get("fingerprint")
        owner_repo = None
        if isinstance(fingerprint, str):
            m = _GITHUB_URL_RE.search(fingerprint.strip())
            if m:
                owner_repo = f"{m.group(1)}/{m.group(2)}"
        entries[identity] = RepoEntry(
            identity=identity,
            path=Path(raw["path"]).expanduser(),
            fingerprint=fingerprint,
            owner_repo=owner_repo,
        )
    return entries


# ---------------------------------------------------------------------------
# External probe. The real implementation reads git refs in the registered
# checkout and GitHub PRs via `gh`; tests inject a fake. Read-only only:
# no fetch, no checkout, nothing that moves a ref.
# ---------------------------------------------------------------------------

class ProbeError(Exception):
    pass


class ExternalProbe:
    def list_prs(self, owner_repo: str) -> list[dict]:
        raise NotImplementedError

    def commit_on_main(self, repo_path: Path, sha: str) -> bool | None:
        """True: sha is an ancestor of origin/main. False: resolvable commit
        that is not. None: sha not resolvable in this checkout."""
        raise NotImplementedError

    def branch_on_origin(self, repo_path: Path, branch: str) -> bool:
        raise NotImplementedError


class GhGitProbe(ExternalProbe):
    def __init__(self) -> None:
        self._pr_cache: dict[str, list[dict]] = {}

    def list_prs(self, owner_repo: str) -> list[dict]:
        if owner_repo in self._pr_cache:
            return self._pr_cache[owner_repo]
        try:
            proc = subprocess.run(
                ["gh", "pr", "list", "--repo", owner_repo, "--state", "all",
                 "--limit", str(GH_PR_LIMIT),
                 "--json", "number,state,title,body,headRefName,headRefOid,"
                           "mergedAt,url,author,mergedBy"],
                capture_output=True, text=True, timeout=120,
            )
        except (OSError, subprocess.SubprocessError) as e:
            raise ProbeError(f"gh pr list for {owner_repo} failed to run: {e}")
        if proc.returncode != 0:
            raise ProbeError(
                f"gh pr list for {owner_repo} failed: "
                f"{proc.stderr.strip()[:300]}")
        try:
            prs = json.loads(proc.stdout)
        except json.JSONDecodeError as e:
            raise ProbeError(f"gh pr list for {owner_repo} printed non-JSON: {e}")
        if not isinstance(prs, list):
            raise ProbeError(f"gh pr list for {owner_repo} returned no PR list")
        self._pr_cache[owner_repo] = prs
        return prs

    def _git(self, repo_path: Path, *args: str) -> subprocess.CompletedProcess:
        try:
            return subprocess.run(
                ["git", "-C", str(repo_path), *args],
                capture_output=True, text=True, timeout=30,
            )
        except (OSError, subprocess.SubprocessError) as e:
            raise ProbeError(f"git -C {repo_path} {' '.join(args)} failed: {e}")

    def commit_on_main(self, repo_path: Path, sha: str) -> bool | None:
        probe = self._git(repo_path, "cat-file", "-t", sha)
        if probe.returncode != 0 or probe.stdout.strip() != "commit":
            return None
        main_ref = "origin/main"
        if self._git(repo_path, "rev-parse", "--verify", "-q",
                     main_ref).returncode != 0:
            main_ref = "main"
            if self._git(repo_path, "rev-parse", "--verify", "-q",
                         main_ref).returncode != 0:
                raise ProbeError(f"{repo_path}: neither origin/main nor main resolves")
        anc = self._git(repo_path, "merge-base", "--is-ancestor", sha, main_ref)
        if anc.returncode == 0:
            return True
        if anc.returncode == 1:
            return False
        raise ProbeError(
            f"git merge-base in {repo_path} failed: {anc.stderr.strip()[:300]}")

    def branch_on_origin(self, repo_path: Path, branch: str) -> bool:
        proc = self._git(repo_path, "rev-parse", "--verify", "-q",
                         f"refs/remotes/origin/{branch}")
        return proc.returncode == 0


# ---------------------------------------------------------------------------
# External-claim extraction from a task record + its journal events
# ---------------------------------------------------------------------------

@dataclass
class TaskClaims:
    issue: int | None = None
    branches: set[str] = field(default_factory=set)
    pr_numbers: set[int] = field(default_factory=set)
    commits: set[str] = field(default_factory=set)

    def any(self) -> bool:
        return bool(self.issue or self.branches or self.pr_numbers or self.commits)


_BRANCH_RE = re.compile(r"branch\s+([A-Za-z0-9][A-Za-z0-9._/-]*)", re.IGNORECASE)
_PR_RE = re.compile(r"(?:\bPR\s*#|pull request\s*#|/pull/)(\d+)", re.IGNORECASE)
_COMMIT_RE = re.compile(r"\bcommit\s+([0-9a-f]{7,40})\b", re.IGNORECASE)
_ISSUE_URL_RE = re.compile(r"/issues/(\d+)")
_ISSUE_TEXT_RE = re.compile(r"\bissue\s+#(\d+)", re.IGNORECASE)


def _task_texts(task: dict, events: list[dict]) -> list[str]:
    texts: list[str] = []
    for key in ("intent_summary", "result_summary", "failure_reason",
                "approval_action"):
        val = task.get(key)
        if isinstance(val, str):
            texts.append(val)
    params = task.get("parameters")
    if params is not None:
        texts.append(json.dumps(params))
    for ev in events:
        payload = ev.get("payload")
        if isinstance(payload, dict):
            for key in ("action", "note", "reason"):
                val = payload.get(key)
                if isinstance(val, str):
                    texts.append(val)
    return texts


def extract_claims(task: dict, events: list[dict]) -> TaskClaims:
    claims = TaskClaims()
    params = task.get("parameters") or {}
    if isinstance(params, dict):
        issue = params.get("issue")
        if isinstance(issue, int):
            claims.issue = issue
        elif isinstance(issue, str) and issue.isdigit():
            claims.issue = int(issue)
        url = params.get("issue_url")
        if isinstance(url, str):
            m = _ISSUE_URL_RE.search(url)
            if m:
                claims.issue = int(m.group(1))
    own_suffix = task.get("task_id", "").removeprefix("task-")
    for text in _task_texts(task, events):
        if claims.issue is None:
            m = _ISSUE_TEXT_RE.search(text)
            if m:
                claims.issue = int(m.group(1))
        for m in _BRANCH_RE.finditer(text):
            claims.branches.add(m.group(1).rstrip(".,;:\"'"))
        for m in _PR_RE.finditer(text):
            claims.pr_numbers.add(int(m.group(1)))
        for m in _COMMIT_RE.finditer(text):
            token = m.group(1)
            if token != own_suffix:
                claims.commits.add(token)
    return claims


# ---------------------------------------------------------------------------
# fulfilled_externally records in the journal
# ---------------------------------------------------------------------------

@dataclass
class FulfillmentRecord:
    fulfiller: str
    pr: int | None
    raw: dict


_FE_TEXT_RE = re.compile(
    r"^fulfilled_externally:\s*(?P<rest>.*)$")
_FE_KV_RE = re.compile(r"(\w+)=([^\s]+)")


def find_fulfillment_records(events: list[dict]) -> list[FulfillmentRecord]:
    records: list[FulfillmentRecord] = []
    for ev in events:
        if ev.get("kind") != "note":
            continue
        payload = ev.get("payload")
        if not isinstance(payload, dict):
            continue
        fe = payload.get("fulfilled_externally")
        if isinstance(fe, dict) and fe.get("fulfiller"):
            pr = fe.get("pr")
            records.append(FulfillmentRecord(
                fulfiller=str(fe["fulfiller"]),
                pr=int(pr) if isinstance(pr, int) or (
                    isinstance(pr, str) and pr.lstrip("#").isdigit()) else None,
                raw=fe))
            continue
        note = payload.get("note")
        if isinstance(note, str):
            m = _FE_TEXT_RE.match(note.strip())
            if m:
                kv = dict(_FE_KV_RE.findall(m.group("rest")))
                if kv.get("fulfiller"):
                    pr_raw = kv.get("pr", "").lstrip("#")
                    records.append(FulfillmentRecord(
                        fulfiller=kv["fulfiller"],
                        pr=int(pr_raw) if pr_raw.isdigit() else None,
                        raw={"note": note}))
    return records


# ---------------------------------------------------------------------------
# Per-task reconciliation
# ---------------------------------------------------------------------------

@dataclass
class TaskRow:
    task_id: str
    ledger_state: str
    external: str            # human-readable external evidence summary
    verdict: str             # CONSISTENT / EXPLAINED / BLOCKED / NO_EVIDENCE /
                             # STALE / IN_PROGRESS / UNVERIFIED
    detail: str = ""


@dataclass
class Finding:
    task_id: str
    code: str
    line: str                # the honest-failure line, printed verbatim


def _pr_label(pr: dict) -> str:
    return f"PR #{pr['number']} ({pr.get('headRefName', '?')}) {pr.get('state', '?')}"


def _match_prs(prs: list[dict], claims: TaskClaims) -> list[dict]:
    matched: list[dict] = []
    for pr in prs:
        number = pr.get("number")
        branch = pr.get("headRefName") or ""
        haystack = f"{pr.get('title') or ''}\n{pr.get('body') or ''}"
        hit = False
        if isinstance(number, int) and number in claims.pr_numbers:
            hit = True
        if branch and branch in claims.branches:
            hit = True
        if claims.issue is not None:
            issue = claims.issue
            if re.search(rf"issue[-_/ ]{issue}\b", branch):
                hit = True
            elif re.search(rf"#\s*{issue}\b", haystack) or \
                    f"issues/{issue}" in haystack:
                hit = True
        if hit:
            matched.append(pr)
    return matched


def _ledger_evidence(task: dict) -> list[str]:
    evidence: list[str] = []
    if task.get("artifact_refs"):
        evidence.append(f"{len(task['artifact_refs'])} artifact ref(s)")
    if task.get("receipt_refs"):
        evidence.append(f"{len(task['receipt_refs'])} receipt ref(s)")
    if task.get("result_summary"):
        evidence.append("result_summary")
    if task.get("failure_reason"):
        evidence.append("failure_reason")
    return evidence


def _terminal_reason(task: dict, events: list[dict]) -> str | None:
    if task.get("failure_reason"):
        return str(task["failure_reason"])
    if task.get("result_summary"):
        return str(task["result_summary"])
    for trans in reversed(task.get("state_history") or []):
        if isinstance(trans, dict) and trans.get("reason"):
            return str(trans["reason"])
    for ev in reversed(events):
        if ev.get("kind") in ("failed", "cancelled", "completed"):
            payload = ev.get("payload")
            if isinstance(payload, dict) and payload.get("reason"):
                return str(payload["reason"])
    return None


def reconcile_task(
    task: dict,
    events: list[dict],
    registry: dict[str, RepoEntry],
    probe: ExternalProbe,
    now: float,
    stale_seconds: float,
) -> tuple[TaskRow, list[Finding], dict | None]:
    """Returns (row, findings, proposed fulfilled_externally record or None)."""
    task_id = task["task_id"]
    status = task["status"]
    findings: list[Finding] = []

    # -- non-terminal: staleness only -------------------------------------
    if status not in TERMINAL_STATES:
        last = float(task.get("updated_at") or task.get("created_at") or 0)
        for ev in events:
            at = ev.get("at")
            if isinstance(at, (int, float)):
                last = max(last, float(at))
        age = now - last
        if age > stale_seconds:
            line = (f"STALE_NONTERMINAL: task {task_id} ledger={status}, "
                    f"no activity for {age / 3600:.1f}h "
                    f"(window {stale_seconds / 3600:.1f}h) -- "
                    "state without an owner; NOT reconciled as live work")
            findings.append(Finding(task_id, "STALE_NONTERMINAL", line))
            return TaskRow(task_id, status, "not probed (non-terminal)",
                           "STALE", line), findings, None
        return TaskRow(task_id, status, "not probed (non-terminal)",
                       "IN_PROGRESS",
                       f"last activity {age / 3600:.1f}h ago"), findings, None

    claims = extract_claims(task, events)
    evidence = _ledger_evidence(task)

    # -- external probe (only when the task carries an external claim) ----
    matched_prs: list[dict] = []
    commit_results: dict[str, bool | None] = {}
    branch_results: dict[str, bool] = {}
    external_bits: list[str] = []
    if claims.any():
        identity = task.get("target_repo_identity")
        entry = registry.get(identity) if identity else None
        if entry is None:
            line = (f"UNVERIFIED: task {task_id} names repo "
                    f"{identity!r}, which is not in the repository registry "
                    "-- external reality cannot be probed; NOT reconciled")
            findings.append(Finding(task_id, "UNVERIFIED", line))
            return TaskRow(task_id, status, "probe impossible (unregistered repo)",
                           "UNVERIFIED", line), findings, None
        try:
            if entry.owner_repo and (claims.issue is not None or claims.branches
                                     or claims.pr_numbers):
                matched_prs = _match_prs(probe.list_prs(entry.owner_repo), claims)
            for sha in sorted(claims.commits):
                commit_results[sha] = probe.commit_on_main(entry.path, sha)
            for branch in sorted(claims.branches):
                branch_results[branch] = probe.branch_on_origin(entry.path, branch)
        except ProbeError as e:
            line = (f"UNVERIFIED: task {task_id} external probe failed: {e} "
                    "-- ledger NOT reconciled against external reality")
            findings.append(Finding(task_id, "UNVERIFIED", line))
            return TaskRow(task_id, status, "probe failed",
                           "UNVERIFIED", line), findings, None
        for pr in matched_prs:
            external_bits.append(_pr_label(pr))
        for sha, on_main in commit_results.items():
            if on_main is True:
                external_bits.append(f"commit {sha[:12]} on origin/main")
            elif on_main is False:
                external_bits.append(f"commit {sha[:12]} present, NOT on origin/main")
            else:
                external_bits.append(f"commit {sha[:12]} not resolvable in checkout")
        for branch, present in branch_results.items():
            if present and not any(
                    pr.get("headRefName") == branch for pr in matched_prs):
                external_bits.append(f"branch {branch} on origin")

    merged_prs = [pr for pr in matched_prs if pr.get("state") == "MERGED"]
    success_prs = merged_prs
    named_commit_on_main = any(v is True for v in commit_results.values())
    external_success = bool(success_prs) or named_commit_on_main
    external_presence = bool(matched_prs) or named_commit_on_main or \
        any(v is not None for v in commit_results.values()) or \
        any(branch_results.values())
    external_summary = "; ".join(external_bits) if external_bits else \
        ("no external trace found" if claims.any() else "no external claim")

    # -- failed / cancelled vs external success ---------------------------
    if status in ("failed", "cancelled") and external_success:
        records = find_fulfillment_records(events)
        success_numbers = {pr["number"] for pr in success_prs}
        explaining = [r for r in records
                      if r.pr is not None and r.pr in success_numbers]
        if not explaining and records and not success_numbers and named_commit_on_main:
            explaining = records
        pr_desc = "; ".join(_pr_label(pr) for pr in success_prs) or \
            "named commit on origin/main"
        if explaining:
            rec = explaining[0]
            detail = (f"ledger={status} but {pr_desc}; explained by appended "
                      f"fulfilled_externally record (fulfiller "
                      f"{rec.fulfiller}, PR #{rec.pr}) -- ledger itself "
                      "unchanged, not rewritten")
            return TaskRow(task_id, status, external_summary,
                           "EXPLAINED", detail), findings, None
        line = (f"RECONCILE_BLOCKED: task {task_id} ledger={status}, "
                f"{pr_desc} -- ledger NOT trusted for this task; append a "
                "fulfilled_externally record naming the fulfiller + PR, "
                "never a silent rewrite")
        findings.append(Finding(task_id, "RECONCILE_BLOCKED", line))
        proposed = None
        if success_prs:
            pr = success_prs[0]
            merged_by = pr.get("mergedBy") or pr.get("author") or {}
            fulfiller = (merged_by.get("login") or merged_by.get("name")
                         if isinstance(merged_by, dict) else None) or "unknown"
            proposed = {
                "fulfiller": str(fulfiller),
                "pr": pr["number"],
                "pr_url": pr.get("url"),
                "head_commit": pr.get("headRefOid"),
                "recorded_by": "reconcile-fabric-ledger",
                "at": int(now),
            }
        return TaskRow(task_id, status, external_summary,
                       "BLOCKED", line), findings, proposed

    # -- terminal with no evidence record anywhere ------------------------
    if not evidence and not external_presence:
        reason = _terminal_reason(task, events)
        if reason is None:
            line = (f"TERMINAL_WITHOUT_EVIDENCE: task {task_id} "
                    f"ledger={status} with no artifact/receipt refs, no "
                    "result or failure reason, and no external trace -- "
                    "terminal state has no matching evidence record")
            findings.append(Finding(task_id, "TERMINAL_WITHOUT_EVIDENCE", line))
            return TaskRow(task_id, status, external_summary,
                           "NO_EVIDENCE", line), findings, None

    detail_bits = []
    if evidence:
        detail_bits.append("ledger evidence: " + ", ".join(evidence))
    if external_presence:
        detail_bits.append("external trace present")
    if status in ("failed", "cancelled") and claims.any() and not external_success:
        detail_bits.append("no external success contradicts the ledger")
    return TaskRow(task_id, status, external_summary, "CONSISTENT",
                   "; ".join(detail_bits)), findings, None


# ---------------------------------------------------------------------------
# Pending-approval card aging
# ---------------------------------------------------------------------------

@dataclass
class CardRow:
    file: str
    task_id: str | None
    verdict: str             # PENDING_OK / STALE_CARD / ORPHAN_CARD / OUT_OF_SCOPE
    detail: str = ""


def age_pending_cards(
    pending_dir: Path,
    ledger: LedgerData,
    now: float,
) -> tuple[list[CardRow], list[Finding]]:
    rows: list[CardRow] = []
    findings: list[Finding] = []
    if not pending_dir.is_dir():
        return rows, findings
    for card_path in sorted(pending_dir.glob("*.json")):
        card = _load_json_file(card_path, "pending-approval card")
        request_id = card.get("agent_request_id") or ""
        app = card.get("app") or ""
        task_id = None
        if isinstance(request_id, str) and request_id.startswith("task-fabric:"):
            task_id = request_id.split(":", 1)[1]
        if app != "task-fabric" and task_id is None:
            rows.append(CardRow(card_path.name, None, "OUT_OF_SCOPE",
                                f"app={app or '?'} -- not a task-fabric card"))
            continue
        created = card.get("created_at")
        age_desc = ""
        if isinstance(created, (int, float)):
            age_desc = f", created {(now - float(created)) / 86400:.1f}d ago"
        task = ledger.tasks.get(task_id) if task_id else None
        if task is None:
            line = (f"ORPHAN_CARD: {card_path.name} names task {task_id}, "
                    "which the ledger does not hold -- card file is "
                    "ownerless state; NOT a live approval")
            findings.append(Finding(task_id or card_path.name,
                                    "ORPHAN_CARD", line))
            rows.append(CardRow(card_path.name, task_id, "ORPHAN_CARD", line))
        elif task["status"] in TERMINAL_STATES:
            line = (f"STALE_CARD: {card_path.name} still sits in "
                    f"pending-approvals/ but task {task_id} is terminal "
                    f"(ledger={task['status']}{age_desc}) -- resolved; "
                    "age this file out")
            findings.append(Finding(task_id, "STALE_CARD", line))
            rows.append(CardRow(card_path.name, task_id, "STALE_CARD", line))
        else:
            rows.append(CardRow(card_path.name, task_id, "PENDING_OK",
                                f"task {task_id} ledger={task['status']}{age_desc}"))
    return rows, findings


# ---------------------------------------------------------------------------
# Driver
# ---------------------------------------------------------------------------

def default_state_dir() -> Path:
    import os
    env = os.environ.get("TASK_FABRIC_SVN_WORK_STATE")
    if env:
        return Path(env)
    xdg = os.environ.get("XDG_STATE_HOME")
    if xdg:
        return Path(xdg) / "task-fabric-svn"
    return Path.home() / ".local" / "state" / "task-fabric-svn"


def run(args: argparse.Namespace, probe: ExternalProbe) -> int:
    now = float(args.now) if args.now is not None else time.time()
    stale_seconds = args.stale_hours * 3600.0

    try:
        ledger = load_ledger(args.state_dir)
        registry = load_registry(args.registry)
    except Unverified as e:
        print(f"UNVERIFIED: {e}")
        print("RECONCILIATION NOT PERFORMED -- ledger NOT verified, "
              "no state was trusted or changed.")
        return 2

    rows: list[TaskRow] = []
    findings: list[Finding] = []
    proposed_records: dict[str, dict] = {}
    unverified = False
    for task_id in sorted(ledger.tasks):
        task = ledger.tasks[task_id]
        row, task_findings, proposed = reconcile_task(
            task, ledger.events.get(task_id, []), registry, probe,
            now, stale_seconds)
        rows.append(row)
        findings.extend(task_findings)
        if row.verdict == "UNVERIFIED":
            unverified = True
        if proposed is not None:
            proposed_records[task_id] = proposed

    card_rows: list[CardRow] = []
    if args.pending_approvals_dir is not None:
        try:
            card_rows, card_findings = age_pending_cards(
                args.pending_approvals_dir, ledger, now)
            findings.extend(card_findings)
        except Unverified as e:
            print(f"UNVERIFIED: {e}")
            unverified = True

    blocked = [f for f in findings if f.code != "UNVERIFIED"]

    # -- report ------------------------------------------------------------
    if args.json:
        report = {
            "state_dir": str(args.state_dir),
            "now": int(now),
            "tasks": [
                {"task_id": r.task_id, "ledger": r.ledger_state,
                 "external": r.external, "verdict": r.verdict,
                 "detail": r.detail}
                for r in rows
            ],
            "pending_cards": [
                {"file": c.file, "task_id": c.task_id,
                 "verdict": c.verdict, "detail": c.detail}
                for c in card_rows
            ],
            "findings": [
                {"task_id": f.task_id, "code": f.code, "line": f.line}
                for f in findings
            ],
            "verdict": ("UNVERIFIED" if unverified else
                        "NOT_RECONCILED" if blocked else "RECONCILED"),
        }
        print(json.dumps(report, indent=2, sort_keys=True))
    else:
        print("task | ledger | external | verdict")
        for r in rows:
            print(f"{r.task_id} | {r.ledger_state} | {r.external} | "
                  f"{r.verdict} -- {r.detail}")
        for c in card_rows:
            print(f"card {c.file} | task {c.task_id or '-'} | "
                  f"{c.verdict} -- {c.detail}")
        for f in findings:
            print(f.line)
        if unverified:
            print("RECONCILIATION UNVERIFIED -- at least one task could "
                  "not be checked; ledger NOT declared reconciled.")
        elif blocked:
            print(f"NOT RECONCILED -- {len(blocked)} finding(s); the "
                  "ledger is not trusted where named above.")
        else:
            print("RECONCILED -- every terminal state agrees with its "
                  "evidence; no silent rewrites were made or needed.")

    # -- proposed fulfilled_externally records (never into the state dir) --
    if args.emit_records:
        emit_path: Path = args.emit_records
        try:
            emit_path.resolve().relative_to(args.state_dir.resolve())
            print("UNVERIFIED: --emit-records path is inside the ledger "
                  "state dir; refusing to write records there. The ledger "
                  "is appended by its owner, never by this verifier.",
                  file=sys.stderr)
            return 2
        except ValueError:
            pass
        lines = []
        for task_id, rec in sorted(proposed_records.items()):
            lines.append(json.dumps({
                "schema_version": 1,
                "task_id": task_id,
                "kind": "note",
                "at": int(now),
                "payload": {"fulfilled_externally": rec},
            }, sort_keys=True))
        emit_path.write_text("\n".join(lines) + ("\n" if lines else ""))
        print(f"proposed fulfilled_externally records for "
              f"{len(lines)} task(s) written to {emit_path} -- the live "
              "journal was NOT modified; appending is an operator act.")

    if unverified:
        return 2
    if blocked:
        return 1
    return 0


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        description="Reconcile fabric ledger terminal states against "
                    "external reality (branch/commit/PR), read-only.")
    parser.add_argument("--state-dir", type=Path, default=default_state_dir(),
                        help="ledger state dir (default: TASK_FABRIC_SVN_WORK_STATE "
                             "or ~/.local/state/task-fabric-svn)")
    parser.add_argument("--registry", type=Path,
                        default=Path(__file__).resolve().parents[1]
                        / "config" / "task-repositories.toml",
                        help="repository registry TOML")
    parser.add_argument("--pending-approvals-dir", type=Path,
                        default=Path.home() / ".local" / "state"
                        / "heron-workstation" / "pending-approvals",
                        help="pending-approval card dir to age out "
                             "(pass an empty value to skip)")
    parser.add_argument("--stale-hours", type=float, default=DEFAULT_STALE_HOURS,
                        help="non-terminal tasks with no activity for this "
                             "long are STALE (default 24)")
    parser.add_argument("--emit-records", type=Path, default=None,
                        help="write proposed fulfilled_externally journal "
                             "lines for blocked mismatches to this file "
                             "(must be outside the state dir)")
    parser.add_argument("--json", action="store_true",
                        help="emit the report as JSON")
    parser.add_argument("--now", type=float, default=None,
                        help="epoch seconds to treat as now (tests)")
    args = parser.parse_args(argv)
    if args.pending_approvals_dir is not None and \
            str(args.pending_approvals_dir) == "":
        args.pending_approvals_dir = None
    return run(args, GhGitProbe())


if __name__ == "__main__":
    sys.exit(main())
