#!/usr/bin/env python3
"""Unit tests for scripts/task-fabric-svn-task-worker.py (Task Fabric steps 4 and 6).

Covers the pure worker<->harness plumbing: marker extraction (including
markdown bold wrappers), turn classification and marker precedence
(question > approval > done), the TASK_FABRIC_SVN_APPROVAL gate (resolution parsing,
resume-text construction), prompt construction reading the serialized
`parameters` field, env loading and credential precedence, opencode JSON
event/text/session parsing, initial vs continuation session argv behavior,
missing-marker failure, artifact-ref sanitization, and resume/session
adoption.

Run from the repo root with either:
    python3 -m pytest scripts/tests/test_task_fabric_svn_task_worker.py
    python3 -m unittest discover -s scripts/tests
"""

import importlib.util
import json
import os
import tempfile
import unittest
from pathlib import Path
from unittest import mock

WORKER_PATH = Path(__file__).resolve().parent.parent / "task-fabric-svn-task-worker.py"


def load_worker():
    spec = importlib.util.spec_from_file_location("task_fabric_svn_task_worker", WORKER_PATH)
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


worker = load_worker()


class HarnessRouteTests(unittest.TestCase):
    def test_worker_accepts_only_explicit_opencode_route(self):
        worker = load_worker()
        worker.validate_task_harness({"harness": "opencode"})

    def test_worker_rejects_missing_or_unsupported_route(self):
        worker = load_worker()
        for task in ({}, {"harness": None}, {"harness": "codex"}, {"harness": "claude"}):
            with self.subTest(task=task), self.assertRaisesRegex(ValueError, "unsupported_harness"):
                worker.validate_task_harness(task)


class MarkerExtractionTests(unittest.TestCase):
    def test_plain_question_marker(self):
        out = "some work\nTASK_FABRIC_SVN_QUESTION: Should I create the doc?\n"
        self.assertEqual(
            worker.extract_marker(out, worker.QUESTION_MARKER),
            "Should I create the doc?",
        )

    def test_bold_wrapped_question_marker(self):
        out = "**TASK_FABRIC_SVN_QUESTION:** Should I create the doc?"
        self.assertEqual(
            worker.extract_marker(out, worker.QUESTION_MARKER),
            "Should I create the doc?",
        )

    def test_bold_wrapped_done_marker(self):
        out = "work work work\n**TASK_FABRIC_SVN_DONE:** All finished."
        self.assertEqual(
            worker.extract_marker(out, worker.DONE_MARKER), "All finished."
        )

    def test_bold_wrapped_done_with_trailing_closer(self):
        out = "**TASK_FABRIC_SVN_DONE:** All finished **"
        self.assertEqual(
            worker.extract_marker(out, worker.DONE_MARKER), "All finished"
        )

    def test_last_marker_line_wins(self):
        out = "TASK_FABRIC_SVN_QUESTION: first?\nTASK_FABRIC_SVN_QUESTION: second?"
        self.assertEqual(
            worker.extract_marker(out, worker.QUESTION_MARKER), "second?"
        )

    def test_no_marker_returns_none(self):
        self.assertIsNone(
            worker.extract_marker("just some transcript text", worker.QUESTION_MARKER)
        )

    def test_question_marker_does_not_match_done_line(self):
        self.assertIsNone(
            worker.extract_marker("TASK_FABRIC_SVN_DONE: finished", worker.QUESTION_MARKER)
        )

    def test_legitimate_trailing_asterisk_preserved(self):
        # A trailing "*" with no leading opener is not paired emphasis.
        out = "TASK_FABRIC_SVN_QUESTION: is 2 * 3 the answer*"
        self.assertEqual(
            worker.extract_marker(out, worker.QUESTION_MARKER),
            "is 2 * 3 the answer*",
        )


class ClassifyTurnTests(unittest.TestCase):
    def test_question_only(self):
        self.assertEqual(
            worker.classify_turn("q?", None, None), ("question", "q?"))

    def test_approval_only(self):
        self.assertEqual(
            worker.classify_turn(None, "do X", None), ("approval", "do X"))

    def test_done_only(self):
        self.assertEqual(
            worker.classify_turn(None, None, "done"), ("done", "done"))

    def test_question_takes_precedence_over_approval_and_done(self):
        action, payload = worker.classify_turn("q?", "do X", "all done")
        self.assertEqual(action, "question")
        self.assertEqual(payload, "q?")

    def test_approval_takes_precedence_over_done(self):
        action, payload = worker.classify_turn(None, "do X", "all done")
        self.assertEqual(action, "approval")
        self.assertEqual(payload, "do X")

    def test_missing_marker_when_none_present(self):
        self.assertEqual(
            worker.classify_turn(None, None, None), ("missing_marker", None))


class ApprovalMarkerTests(unittest.TestCase):
    def test_plain_approval_marker(self):
        out = "some work\nTASK_FABRIC_SVN_APPROVAL: delete the scratch branch\n"
        self.assertEqual(
            worker.extract_marker(out, worker.APPROVAL_MARKER),
            "delete the scratch branch",
        )

    def test_bold_wrapped_approval_marker(self):
        out = "**TASK_FABRIC_SVN_APPROVAL:** push the docs commit"
        self.assertEqual(
            worker.extract_marker(out, worker.APPROVAL_MARKER),
            "push the docs commit",
        )

    def test_approval_marker_does_not_match_question_line(self):
        self.assertIsNone(
            worker.extract_marker("TASK_FABRIC_SVN_QUESTION: which?", worker.APPROVAL_MARKER)
        )

    def test_question_marker_does_not_match_approval_line(self):
        self.assertIsNone(
            worker.extract_marker(
                "TASK_FABRIC_SVN_APPROVAL: do it?", worker.QUESTION_MARKER)
        )

    def test_last_approval_marker_line_wins(self):
        out = "TASK_FABRIC_SVN_APPROVAL: first\nTASK_FABRIC_SVN_APPROVAL: second"
        self.assertEqual(
            worker.extract_marker(out, worker.APPROVAL_MARKER), "second")


class ApprovalResumeTests(unittest.TestCase):
    def test_approval_without_write_in(self):
        text = worker.build_approval_resume_text("restart the service", None)
        self.assertIn("APPROVED", text)
        self.assertIn("restart the service", text)
        self.assertIn("Proceed with the action", text)

    def test_approval_with_write_in_becomes_hard_constraint(self):
        text = worker.build_approval_resume_text(
            "restart the service", "only the docs branch, no force-push")
        self.assertIn("APPROVED", text)
        self.assertIn("restart the service", text)
        self.assertIn("only the docs branch, no force-push", text)
        self.assertIn("hard constraint", text)

    def test_latest_approval_resolution_parses_gate_cycle(self):
        hist = {"events": [
            {"kind": "approval_required",
             "payload": {"action": "push to origin", "risk_class": "write"}},
            {"kind": "approval_resolved",
             "payload": {"approved": True, "reason": "ship it"}},
        ]}
        with mock.patch.object(worker, "run_task_fabric_svn_work", return_value=hist):
            self.assertEqual(
                worker.latest_approval_resolution("task-x"),
                (True, "push to origin", "ship it"),
            )

    def test_latest_approval_resolution_denial(self):
        hist = {"events": [
            {"kind": "approval_required", "payload": {"action": "rm -rf /tmp/x"}},
            {"kind": "approval_resolved",
             "payload": {"approved": False,
                         "reason": "approval_denied: too risky"}},
        ]}
        with mock.patch.object(worker, "run_task_fabric_svn_work", return_value=hist):
            res = worker.latest_approval_resolution("task-x")
            self.assertEqual(res[0], False)
            self.assertEqual(res[1], "rm -rf /tmp/x")
            self.assertEqual(res[2], "approval_denied: too risky")

    def test_latest_approval_resolution_none_when_unresolved(self):
        hist = {"events": [
            {"kind": "approval_required", "payload": {"action": "push"}},
        ]}
        with mock.patch.object(worker, "run_task_fabric_svn_work", return_value=hist):
            self.assertIsNone(worker.latest_approval_resolution("task-x"))

    def test_latest_approval_resolution_uses_latest_cycle(self):
        hist = {"events": [
            {"kind": "approval_required", "payload": {"action": "first"}},
            {"kind": "approval_resolved",
             "payload": {"approved": True, "reason": None}},
            {"kind": "approval_required", "payload": {"action": "second"}},
            {"kind": "approval_resolved",
             "payload": {"approved": False, "reason": "no"}},
        ]}
        with mock.patch.object(worker, "run_task_fabric_svn_work", return_value=hist):
            self.assertEqual(
                worker.latest_approval_resolution("task-x"),
                (False, "second", "no"),
            )

    def test_latest_approval_resolution_history_failure_returns_none(self):
        with mock.patch.object(worker, "run_task_fabric_svn_work",
                               side_effect=RuntimeError("boom")):
            self.assertIsNone(worker.latest_approval_resolution("task-x"))


class BuildPromptTests(unittest.TestCase):
    def test_reads_serialized_parameters_field(self):
        task = {
            "task_id": "task-abc",
            "intent_summary": "do the thing",
            "parameters": {"path": "docs/x.md", "flag": True},
        }
        prompt = worker.build_prompt(task, None, resumed=False)
        self.assertIn('"path": "docs/x.md"', prompt)
        self.assertIn('"flag": true', prompt)

    def test_typed_parameters_kept_as_compatibility_fallback(self):
        task = {
            "task_id": "task-abc",
            "intent_summary": "do the thing",
            "typed_parameters": {"path": "docs/x.md"},
        }
        prompt = worker.build_prompt(task, None, resumed=False)
        self.assertIn('"path": "docs/x.md"', prompt)

    def test_parameters_wins_over_typed_parameters(self):
        task = {
            "task_id": "task-abc",
            "intent_summary": "do the thing",
            "parameters": {"a": 1},
            "typed_parameters": {"a": 2},
        }
        prompt = worker.build_prompt(task, None, resumed=False)
        self.assertIn('"a": 1', prompt)
        self.assertNotIn('"a": 2', prompt)

    def test_resumed_prompt_carries_answer_and_markers(self):
        task = {"task_id": "task-abc", "intent_summary": "i", "parameters": {}}
        prompt = worker.build_prompt(task, "yes, do it", resumed=True)
        self.assertIn("yes, do it", prompt)
        self.assertIn(worker.QUESTION_MARKER, prompt)
        self.assertIn(worker.DONE_MARKER, prompt)

    def test_initial_prompt_documents_approval_marker(self):
        task = {"task_id": "task-abc", "intent_summary": "i", "parameters": {}}
        prompt = worker.build_prompt(task, None, resumed=False)
        self.assertIn(worker.APPROVAL_MARKER, prompt)
        self.assertIn(worker.QUESTION_MARKER, prompt)
        self.assertIn(worker.DONE_MARKER, prompt)

    def test_approval_resume_prompt_carries_approval_text_and_markers(self):
        task = {"task_id": "task-abc", "intent_summary": "i", "parameters": {}}
        prompt = worker.build_prompt(
            task, "The human APPROVED your proposed action: X.", resumed=True,
            resume_kind="approval")
        self.assertIn("APPROVED", prompt)
        self.assertIn(worker.APPROVAL_MARKER, prompt)
        self.assertIn(worker.QUESTION_MARKER, prompt)
        self.assertIn(worker.DONE_MARKER, prompt)

    def test_approval_resume_does_not_use_question_template(self):
        task = {"task_id": "task-abc", "intent_summary": "i", "parameters": {}}
        prompt = worker.build_prompt(
            task, "The human APPROVED your proposed action: X.", resumed=True,
            resume_kind="approval")
        self.assertNotIn("answered your question", prompt)


class EnvTests(unittest.TestCase):
    def test_load_env_file_parses_key_value_lines(self):
        with tempfile.NamedTemporaryFile("w", suffix=".env", delete=False) as f:
            f.write(
                "# a comment\n\nFOO=bar\nQUOTED=\"baz qux\"\nSINGLE='one'\n"
                "EMPTY=\nNOEQUALS\nSPACED = spaced value\n"
            )
            path = f.name
        try:
            env = worker.load_env_file(path)
        finally:
            os.unlink(path)
        self.assertEqual(
            env,
            {"FOO": "bar", "QUOTED": "baz qux", "SINGLE": "one",
             "SPACED": "spaced value"},
        )

    def test_load_env_file_missing_returns_empty(self):
        self.assertEqual(worker.load_env_file("/nonexistent/task-fabric-svn-test.env"), {})

    def test_load_env_file_none_returns_empty(self):
        self.assertEqual(worker.load_env_file(None), {})

    def test_master_key_replaces_stale_openai_key(self):
        env = {"OPENAI_API_KEY": "stale-value", "LITELLM_MASTER_KEY": "master-value"}
        applied = worker.apply_harness_env(env)
        self.assertTrue(applied)
        self.assertEqual(env["OPENAI_API_KEY"], "master-value")
        self.assertEqual(env["LITELLM_MASTER_KEY"], "master-value")

    def test_no_master_key_leaves_env_untouched(self):
        env = {"OPENAI_API_KEY": "keep-me"}
        applied = worker.apply_harness_env(env)
        self.assertFalse(applied)
        self.assertEqual(env, {"OPENAI_API_KEY": "keep-me"})


class ParseOpencodeStdoutTests(unittest.TestCase):
    def test_session_text_and_errors_parsed(self):
        stdout = "\n".join(
            [
                "not json",
                json.dumps({"sessionID": "ses_real123", "type": "session"}),
                json.dumps(
                    {"sessionID": "ses_real123", "type": "text",
                     "part": {"text": "hello "}}
                ),
                json.dumps(
                    {"sessionID": "ses_real123", "type": "text",
                     "part": {"text": "world"}}
                ),
                json.dumps(
                    {"type": "error",
                     "error": {"name": "ProviderError",
                              "data": {"message": "boom"}}}
                ),
                "{invalid json",
            ]
        )
        sid, text, errors = worker.parse_opencode_stdout(stdout)
        self.assertEqual(sid, "ses_real123")
        self.assertEqual(text, "hello world")
        self.assertEqual(errors, ["ProviderError: boom"])

    def test_no_text_events_falls_back_to_raw_stdout(self):
        stdout = "some plain output\nTASK_FABRIC_SVN_DONE: finished"
        sid, text, errors = worker.parse_opencode_stdout(stdout)
        self.assertIsNone(sid)
        self.assertEqual(text, stdout)
        self.assertEqual(errors, [])

    def test_parse_session_id(self):
        stdout = json.dumps(
            {"sessionID": "ses_abc", "type": "text", "part": {"text": "x"}}
        )
        self.assertEqual(worker.parse_session_id(stdout), "ses_abc")
        self.assertIsNone(worker.parse_session_id("no json here"))


class OpencodeCmdTests(unittest.TestCase):
    def test_initial_turn_omits_session(self):
        cmd = worker.build_opencode_cmd(
            "/usr/bin/opencode", Path("/repo"), "m/model", "prompt", None, True
        )
        self.assertNotIn("--session", cmd)
        self.assertIn("--pure", cmd)
        self.assertIn("--format", cmd)
        self.assertIn("json", cmd)
        self.assertEqual(cmd[-1], "prompt")

    def test_continuation_turn_passes_real_session(self):
        cmd = worker.build_opencode_cmd(
            "/usr/bin/opencode", Path("/repo"), "m/model", "prompt",
            "ses_real123", True,
        )
        self.assertIn("--session", cmd)
        self.assertEqual(cmd[cmd.index("--session") + 1], "ses_real123")

    def test_no_pure_omits_flag(self):
        cmd = worker.build_opencode_cmd(
            "/usr/bin/opencode", Path("/repo"), "m/model", "prompt", None, False
        )
        self.assertNotIn("--pure", cmd)

    def test_run_opencode_session_flow(self):
        """First turn omits --session and stdin is DEVNULL; the real
        sessionID from the event envelope is returned and passed back on
        the next turn."""
        events = json.dumps(
            {"sessionID": "ses_adopted9", "type": "text",
             "part": {"text": "TASK_FABRIC_SVN_DONE: all good"}}
        )
        class FakeContainment:
            def __init__(self):
                self.commands = []

            def run(self, command, timeout, cancelled):
                self.commands.append((command, timeout))
                return 0, events, ""

        boundary = FakeContainment()
        with mock.patch.object(worker.shutil, "which", return_value="/usr/bin/opencode"):
            rc, sid, text, errors = worker.run_opencode(
                Path("/repo"), "m/model", "prompt", 60, containment=boundary
            )
            self.assertEqual(rc, 0)
            self.assertEqual(sid, "ses_adopted9")
            self.assertIn("TASK_FABRIC_SVN_DONE", text)
            self.assertEqual(errors, [])
            cmd0, timeout0 = boundary.commands[0]
            self.assertNotIn("--session", cmd0)
            self.assertEqual(timeout0, 60)
            self.assertEqual(cmd0[0], "/opt/task_fabric_svn/opencode")
            self.assertEqual(cmd0[cmd0.index("--dir") + 1], "/workspace")

            worker.run_opencode(
                Path("/repo"), "m/model", "prompt", 60, session_id=sid,
                containment=boundary,
            )
            cmd1, _ = boundary.commands[1]
            self.assertIn("--session", cmd1)
            self.assertEqual(cmd1[cmd1.index("--session") + 1], "ses_adopted9")


class SanitizeArtifactRefTests(unittest.TestCase):
    def test_path_separators_become_colons(self):
        self.assertEqual(
            worker.sanitize_artifact_ref("scripts/foo.py"), "repo:scripts:foo.py"
        )

    def test_backslashes_become_colons(self):
        self.assertEqual(
            worker.sanitize_artifact_ref("a\\b\\c.txt"), "repo:a:b:c.txt"
        )

    def test_no_path_separators_or_dotdot_in_result(self):
        ref = worker.sanitize_artifact_ref("../../etc/passwd")
        self.assertNotIn("/", ref)
        self.assertNotIn("\\", ref)
        self.assertNotIn("..", ref)
        self.assertTrue(ref.startswith("repo:"))

    def test_empty_path_becomes_unknown(self):
        self.assertEqual(worker.sanitize_artifact_ref(""), "repo:unknown")


class ResumeAdoptionTests(unittest.TestCase):
    def _history(self, notes):
        return {
            "events": [{"kind": "progress", "payload": {"note": n}} for n in notes]
        }

    def test_adopts_latest_session_from_notes(self):
        hist = self._history(
            [
                "worker claimed task; opencode session ses_first1 in /repo",
                "opencode session ses_second2",
            ]
        )
        with mock.patch.object(worker, "run_task_fabric_svn_work", return_value=hist):
            self.assertEqual(worker.adopted_session_id("task-x"), "ses_second2")

    def test_no_session_notes_returns_none(self):
        with mock.patch.object(
            worker, "run_task_fabric_svn_work", return_value={"events": []}
        ):
            self.assertIsNone(worker.adopted_session_id("task-x"))

    def test_history_failure_returns_none(self):
        with mock.patch.object(
            worker, "run_task_fabric_svn_work", side_effect=RuntimeError("boom")
        ):
            self.assertIsNone(worker.adopted_session_id("task-x"))

    def test_note_if_working_skips_non_working_states(self):
        calls = []

        def fake_run_task_fabric_svn_work(*args):
            calls.append(args)
            return {"ok": True}

        with mock.patch.object(worker, "run_task_fabric_svn_work",
                               side_effect=fake_run_task_fabric_svn_work):
            worker.note_if_working("task-x", {"status": "input_required"}, "hello")
            worker.note_if_working("task-x", {"status": "starting"}, "hello")
            self.assertEqual(calls, [])
            worker.note_if_working("task-x", {"status": "working"}, "hello")
            self.assertEqual(len(calls), 1)
            self.assertEqual(calls[0][:2], ("note-progress", "task-x"))


if __name__ == "__main__":
    unittest.main()
