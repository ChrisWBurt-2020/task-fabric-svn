"""Tests for reconcile_fabric_ledger.py: terminal-state reconciliation of
the fabric ledger against external reality.

No live ledger, no network, no gh: state dirs are fixture trees in temp
dirs and the external probe is a fake. The centre of the battery is the
mined 2026-10-16 incident replayed byte-for-byte in shape: ledger says
`failed`, the work succeeded externally (PR #80 merged) -- the verifier
must refuse to trust the ledger until an appended fulfilled_externally
record explains the mismatch.
"""
import argparse
import contextlib
import io
import json
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import reconcile_fabric_ledger as rfl

NOW = 1_800_000_000.0
DAY = 86400.0

REGISTRY_TOML = """\
version = 1

[repositories.heron-field]
path = "/nonexistent/HeronField"
fingerprint = "https://github.com/ChrisWBurt-2020/heron-field.git"
"""

PR80 = {
    "number": 80,
    "state": "MERGED",
    "title": "docs(node): why-comment on runAlignWords",
    "body": "",
    "headRefName": "nightshift/issue-79-runalignwords",
    "headRefOid": "d450925c9b4d64db87884d81a62a86f6b5458d48",
    "mergedAt": "2026-10-17T14:11:06Z",
    "url": "https://github.com/ChrisWBurt-2020/heron-field/pull/80",
    "author": {"login": "nightshift-worker"},
    "mergedBy": {"login": "ChrisWBurt-2020"},
}


def make_task(task_id, status, **overrides):
    task = {
        "schema_version": 1,
        "task_id": task_id,
        "conversation_id": "conv-test",
        "target_space_id": "space-test",
        "target_repo_identity": "heron-field",
        "intent_summary": "test task",
        "risk_class": "write-repo",
        "status": status,
        "parameters": {},
        "artifact_refs": [],
        "receipt_refs": [],
        "result_summary": None,
        "failure_reason": None,
        "approval_action": None,
        "created_at": int(NOW - DAY),
        "updated_at": int(NOW - 60),
        "state_history": [],
    }
    task.update(overrides)
    return task


def incident_task():
    """The mined incident: issue #79, approved in chat, ledger failed."""
    return make_task(
        "task-7992bae48730", "failed",
        intent_summary="heron-field issue #79: docs(node) why-comment on "
                       "runAlignWords plus timeout error-path fix.",
        failure_reason="unsupported_escalation: approval cannot widen the "
                       "contained worker boundary",
        parameters={"issue": "79",
                    "issue_url": "https://github.com/ChrisWBurt-2020/"
                                 "heron-field/issues/79"},
    )


def incident_events():
    return [
        {"schema_version": 1, "seq": 46, "task_id": "task-7992bae48730",
         "kind": "submitted", "at": int(NOW - DAY),
         "payload": {"conversation_id": "conv-nightshift-20261016"}},
        {"schema_version": 1, "seq": 53, "task_id": "task-7992bae48730",
         "kind": "approval_required", "at": int(NOW - DAY + 600),
         "payload": {"action": "apply the staged runAlignWords fix, commit "
                               "on branch nightshift/issue-79-runalignwords, "
                               "push to origin, and open a draft PR"}},
        {"schema_version": 1, "seq": 55, "task_id": "task-7992bae48730",
         "kind": "failed", "at": int(NOW - DAY + 900),
         "payload": {"reason": "unsupported_escalation: approval cannot "
                               "widen the contained worker boundary"}},
    ]


class FakeProbe(rfl.ExternalProbe):
    def __init__(self, prs=(), commits=None, branches=(), fail_prs=False):
        self.prs = list(prs)
        self.commits = dict(commits or {})
        self.branches = set(branches)
        self.fail_prs = fail_prs

    def list_prs(self, owner_repo):
        if self.fail_prs:
            raise rfl.ProbeError("gh pr list failed: auth token expired (fake)")
        return list(self.prs)

    def commit_on_main(self, repo_path, sha):
        return self.commits.get(sha)

    def branch_on_origin(self, repo_path, branch):
        return branch in self.branches


class Fixture:
    """A temp ledger state dir + registry + pending-approvals dir."""

    def __init__(self, test, tasks=(), events=(), cards=()):
        self._tmp = tempfile.TemporaryDirectory()
        test.addCleanup(self._tmp.cleanup)
        self.root = Path(self._tmp.name)
        self.state_dir = self.root / "state"
        (self.state_dir / "tasks").mkdir(parents=True)
        for task in tasks:
            (self.state_dir / "tasks" / f"{task['task_id']}.json").write_text(
                json.dumps(task))
        self.journal = self.state_dir / "task-events.jsonl"
        self.journal.write_text(
            "".join(json.dumps(e) + "\n" for e in events))
        self.registry = self.root / "task-repositories.toml"
        self.registry.write_text(REGISTRY_TOML)
        self.pending_dir = self.root / "pending-approvals"
        self.pending_dir.mkdir()
        for name, card in cards:
            (self.pending_dir / name).write_text(json.dumps(card))

    def args(self, **overrides):
        ns = argparse.Namespace(
            state_dir=self.state_dir,
            registry=self.registry,
            pending_approvals_dir=self.pending_dir,
            stale_hours=24.0,
            emit_records=None,
            json=False,
            now=NOW,
        )
        for key, value in overrides.items():
            setattr(ns, key, value)
        return ns


def run_captured(args, probe):
    buf = io.StringIO()
    with contextlib.redirect_stdout(buf):
        code = rfl.run(args, probe)
    return code, buf.getvalue()


def approval_card(task_id, created_days_ago=3.0):
    return {
        "trace_id": None,
        "event_id": "ff400713-f569-439a-a0e5-1512ead547fe",
        "app": "task-fabric",
        "agent_request_id": f"task-fabric:{task_id}",
        "approval_generation": "d706d592afda490e81ac7ec17c1a4f81",
        "title": "Caution: task-fabric needs approval",
        "created_at": int(NOW - created_days_ago * DAY),
        "expires_at": int(NOW - created_days_ago * DAY + 3600),
    }


class IncidentReplayTests(unittest.TestCase):
    def test_failed_but_merged_pr_is_blocked(self):
        fx = Fixture(self, tasks=[incident_task()], events=incident_events())
        journal_before = fx.journal.read_bytes()
        code, out = run_captured(fx.args(), FakeProbe(prs=[PR80]))
        self.assertEqual(code, 1)
        self.assertIn(
            "RECONCILE_BLOCKED: task task-7992bae48730 ledger=failed, "
            "PR #80 (nightshift/issue-79-runalignwords) MERGED", out)
        self.assertIn("ledger NOT trusted for this task", out)
        # The verifier is read-only: the live journal is byte-identical.
        self.assertEqual(fx.journal.read_bytes(), journal_before)

    def test_blocked_mismatch_emits_fulfilled_externally_record(self):
        fx = Fixture(self, tasks=[incident_task()], events=incident_events())
        emit = fx.root / "proposed-records.jsonl"
        code, out = run_captured(
            fx.args(emit_records=emit), FakeProbe(prs=[PR80]))
        self.assertEqual(code, 1)
        lines = emit.read_text().strip().splitlines()
        self.assertEqual(len(lines), 1)
        rec = json.loads(lines[0])
        self.assertEqual(rec["kind"], "note")
        self.assertEqual(rec["task_id"], "task-7992bae48730")
        fe = rec["payload"]["fulfilled_externally"]
        self.assertEqual(fe["pr"], 80)
        self.assertEqual(fe["fulfiller"], "ChrisWBurt-2020")
        self.assertEqual(
            fe["head_commit"], "d450925c9b4d64db87884d81a62a86f6b5458d48")

    def test_appended_record_explains_the_mismatch(self):
        events = incident_events() + [{
            "schema_version": 1, "seq": 60, "task_id": "task-7992bae48730",
            "kind": "note", "at": int(NOW - 3600),
            "payload": {"fulfilled_externally": {
                "fulfiller": "ChrisWBurt-2020", "pr": 80,
                "head_commit": "d450925c9b4d64db87884d81a62a86f6b5458d48"}},
        }]
        fx = Fixture(self, tasks=[incident_task()], events=events)
        code, out = run_captured(fx.args(), FakeProbe(prs=[PR80]))
        self.assertEqual(code, 0)
        self.assertIn("EXPLAINED", out)
        self.assertIn("RECONCILED", out)

    def test_text_form_record_explains_the_mismatch(self):
        events = incident_events() + [{
            "schema_version": 1, "seq": 60, "task_id": "task-7992bae48730",
            "kind": "note", "at": int(NOW - 3600),
            "payload": {"note": "fulfilled_externally: "
                                "fulfiller=ChrisWBurt-2020 pr=#80"},
        }]
        fx = Fixture(self, tasks=[incident_task()], events=events)
        code, _ = run_captured(fx.args(), FakeProbe(prs=[PR80]))
        self.assertEqual(code, 0)

    def test_record_naming_a_different_pr_does_not_explain(self):
        events = incident_events() + [{
            "schema_version": 1, "seq": 60, "task_id": "task-7992bae48730",
            "kind": "note", "at": int(NOW - 3600),
            "payload": {"fulfilled_externally": {
                "fulfiller": "ChrisWBurt-2020", "pr": 81}},
        }]
        fx = Fixture(self, tasks=[incident_task()], events=events)
        code, out = run_captured(fx.args(), FakeProbe(prs=[PR80]))
        self.assertEqual(code, 1)
        self.assertIn("RECONCILE_BLOCKED", out)

    def test_emit_records_refused_inside_state_dir(self):
        fx = Fixture(self, tasks=[incident_task()], events=incident_events())
        emit = fx.state_dir / "sneaky.jsonl"
        code, _ = run_captured(
            fx.args(emit_records=emit), FakeProbe(prs=[PR80]))
        self.assertEqual(code, 2)
        self.assertFalse(emit.exists())


class ConsistencyTests(unittest.TestCase):
    def test_failed_with_only_open_pr_is_consistent(self):
        open_pr = dict(PR80, state="OPEN", mergedAt=None)
        fx = Fixture(self, tasks=[incident_task()], events=incident_events())
        code, out = run_captured(fx.args(), FakeProbe(prs=[open_pr]))
        self.assertEqual(code, 0)
        self.assertIn("CONSISTENT", out)

    def test_cancelled_with_merged_pr_is_blocked(self):
        task = make_task("task-c1", "cancelled",
                         parameters={"issue": 79})
        fx = Fixture(self, tasks=[task])
        code, out = run_captured(fx.args(), FakeProbe(prs=[PR80]))
        self.assertEqual(code, 1)
        self.assertIn("ledger=cancelled", out)

    def test_completed_contained_result_is_consistent(self):
        task = make_task(
            "task-c2", "completed",
            artifact_refs=["contained:task-c2:result"],
            result_summary="Contained result staged; survey completed.")
        fx = Fixture(self, tasks=[task])
        code, out = run_captured(fx.args(), FakeProbe())
        self.assertEqual(code, 0)
        self.assertIn("CONSISTENT", out)

    def test_completed_with_named_commit_on_main_is_consistent(self):
        task = make_task(
            "task-c3", "completed",
            intent_summary="land the fix; commit abc1234def5678 carries it")
        fx = Fixture(self, tasks=[task])
        probe = FakeProbe(commits={"abc1234def5678": True})
        code, out = run_captured(fx.args(), probe)
        self.assertEqual(code, 0)
        self.assertIn("on origin/main", out)

    def test_completed_without_any_evidence_fails_named(self):
        task = make_task("task-c4", "completed", parameters={"issue": 99})
        fx = Fixture(self, tasks=[task])
        code, out = run_captured(fx.args(), FakeProbe(prs=[]))
        self.assertEqual(code, 1)
        self.assertIn("TERMINAL_WITHOUT_EVIDENCE: task task-c4", out)

    def test_failed_without_reason_or_evidence_fails_named(self):
        task = make_task("task-c5", "failed")
        events = [{"schema_version": 1, "seq": 1, "task_id": "task-c5",
                   "kind": "failed", "at": int(NOW - 60), "payload": {}}]
        fx = Fixture(self, tasks=[task], events=events)
        code, out = run_captured(fx.args(), FakeProbe())
        self.assertEqual(code, 1)
        self.assertIn("TERMINAL_WITHOUT_EVIDENCE: task task-c5", out)


class StalenessTests(unittest.TestCase):
    def test_stuck_working_task_fails_named(self):
        task = make_task("task-s1", "working",
                         updated_at=int(NOW - 48 * 3600))
        fx = Fixture(self, tasks=[task])
        code, out = run_captured(fx.args(), FakeProbe())
        self.assertEqual(code, 1)
        self.assertIn("STALE_NONTERMINAL: task task-s1 ledger=working", out)

    def test_fresh_working_task_is_not_a_finding(self):
        task = make_task("task-s2", "working", updated_at=int(NOW - 300))
        fx = Fixture(self, tasks=[task])
        code, out = run_captured(fx.args(), FakeProbe())
        self.assertEqual(code, 0)
        self.assertIn("IN_PROGRESS", out)

    def test_journal_activity_refreshes_staleness(self):
        task = make_task("task-s3", "approval_required",
                         updated_at=int(NOW - 72 * 3600))
        events = [{"schema_version": 1, "seq": 9, "task_id": "task-s3",
                   "kind": "progress", "at": int(NOW - 120),
                   "payload": {"note": "still attended"}}]
        fx = Fixture(self, tasks=[task], events=events)
        code, _ = run_captured(fx.args(), FakeProbe())
        self.assertEqual(code, 0)


class PendingCardTests(unittest.TestCase):
    def test_card_for_terminal_task_is_stale(self):
        task = make_task(
            "task-p1", "completed",
            artifact_refs=["contained:task-p1:result"],
            result_summary="done")
        fx = Fixture(self, tasks=[task],
                     cards=[("card-a.json", approval_card("task-p1"))])
        code, out = run_captured(fx.args(), FakeProbe())
        self.assertEqual(code, 1)
        self.assertIn("STALE_CARD: card-a.json", out)
        self.assertIn("ledger=completed", out)

    def test_incident_card_is_stale_against_failed_task(self):
        cards = [("ff400713-f569-439a-a0e5-1512ead547fe.json",
                  approval_card("task-7992bae48730"))]
        fx = Fixture(self, tasks=[incident_task()],
                     events=incident_events(), cards=cards)
        # The failed task itself is consistent (no PR probed here); only
        # the stale card may fire.
        code, out = run_captured(fx.args(), FakeProbe(prs=[]))
        self.assertEqual(code, 1)
        self.assertIn("STALE_CARD", out)
        self.assertNotIn("RECONCILE_BLOCKED", out)

    def test_card_for_unknown_task_is_orphan(self):
        fx = Fixture(self, tasks=[],
                     cards=[("card-b.json", approval_card("task-ghost"))])
        code, out = run_captured(fx.args(), FakeProbe())
        self.assertEqual(code, 1)
        self.assertIn("ORPHAN_CARD: card-b.json names task task-ghost", out)

    def test_card_for_live_task_is_fine(self):
        task = make_task("task-p2", "approval_required",
                         updated_at=int(NOW - 300))
        fx = Fixture(self, tasks=[task],
                     cards=[("card-c.json", approval_card("task-p2", 0.01))])
        code, out = run_captured(fx.args(), FakeProbe())
        self.assertEqual(code, 0)
        self.assertIn("PENDING_OK", out)

    def test_unrelated_card_is_out_of_scope(self):
        card = {"app": "someone-else", "agent_request_id": "x:1",
                "created_at": int(NOW - DAY)}
        fx = Fixture(self, tasks=[], cards=[("card-d.json", card)])
        code, out = run_captured(fx.args(), FakeProbe())
        self.assertEqual(code, 0)
        self.assertIn("OUT_OF_SCOPE", out)


class UnverifiedTests(unittest.TestCase):
    def test_unparseable_snapshot_is_unverified_never_reconciled(self):
        fx = Fixture(self, tasks=[incident_task()], events=incident_events())
        (fx.state_dir / "tasks" / "task-broken.json").write_text("{not json")
        code, out = run_captured(fx.args(), FakeProbe(prs=[PR80]))
        self.assertEqual(code, 2)
        self.assertIn("UNVERIFIED", out)
        self.assertNotIn("RECONCILED --", out)

    def test_unknown_status_is_unverified(self):
        fx = Fixture(self, tasks=[make_task("task-u1", "exploded")])
        code, out = run_captured(fx.args(), FakeProbe())
        self.assertEqual(code, 2)
        self.assertIn("unknown status", out)

    def test_missing_state_dir_is_unverified(self):
        fx = Fixture(self)
        args = fx.args(state_dir=fx.root / "no-such-dir")
        code, out = run_captured(args, FakeProbe())
        self.assertEqual(code, 2)
        self.assertIn("UNVERIFIED", out)

    def test_probe_failure_for_claimed_task_is_unverified(self):
        fx = Fixture(self, tasks=[incident_task()], events=incident_events())
        code, out = run_captured(fx.args(), FakeProbe(fail_prs=True))
        self.assertEqual(code, 2)
        self.assertIn("external probe failed", out)

    def test_unregistered_repo_is_unverified(self):
        task = make_task("task-u2", "failed", failure_reason="boom",
                         target_repo_identity="no-such-repo",
                         parameters={"issue": 5})
        fx = Fixture(self, tasks=[task])
        code, out = run_captured(fx.args(), FakeProbe())
        self.assertEqual(code, 2)
        self.assertIn("not in the repository registry", out)

    def test_unverified_dominates_blocked(self):
        fx = Fixture(self, tasks=[incident_task()], events=incident_events())
        (fx.state_dir / "tasks" / "task-broken.json").write_text("{nope")
        code, _ = run_captured(fx.args(), FakeProbe(prs=[PR80]))
        self.assertEqual(code, 2)

    def test_task_level_unverified_dominates_a_blocked_task(self):
        unregistered = make_task(
            "task-u3", "failed", failure_reason="boom",
            target_repo_identity="no-such-repo", parameters={"issue": 5})
        fx = Fixture(self, tasks=[incident_task(), unregistered],
                     events=incident_events())
        code, out = run_captured(fx.args(), FakeProbe(prs=[PR80]))
        self.assertEqual(code, 2)
        self.assertIn("RECONCILE_BLOCKED: task task-7992bae48730", out)
        self.assertIn("UNVERIFIED: task task-u3", out)


if __name__ == "__main__":
    unittest.main()
