#!/usr/bin/env python3
"""Unit tests for scripts/codemode (the batched-inspection wrapper).

Covers the validation gate (path allowlist, traversal rejection, size
caps, missing deno) without executing anything. Execution-path tests
(require Deno) are skipped when deno is absent.

Run from the repo root:
    python3 -m pytest scripts/tests/test_codemode.py
"""

import os
import shutil
import subprocess
import tempfile
import unittest
from pathlib import Path

WRAPPER = Path(__file__).resolve().parent.parent / "codemode"
DENO = shutil.which("deno") or "/usr/bin/deno"
DENO_PRESENT = Path(DENO).is_file() and os.access(DENO, os.X_OK)


def run_wrapper(*args, env_extra=None):
    env = dict(os.environ)
    env.update(env_extra or {})
    return subprocess.run(
        ["sh", str(WRAPPER), *args],
        capture_output=True, text=True, timeout=30, env=env,
    )


class WrapperValidationTests(unittest.TestCase):
    def test_rejects_path_outside_allowlist(self):
        r = run_wrapper("/etc/passwd")
        self.assertEqual(r.returncode, 2)
        self.assertIn("refusing script outside", r.stderr)

    def test_rejects_traversal(self):
        r = run_wrapper("/tmp/../etc/x.js")
        self.assertEqual(r.returncode, 2)
        self.assertIn("refusing", r.stderr)

    def test_rejects_missing_file(self):
        r = run_wrapper("/tmp/codemode-does-not-exist-12345.js")
        self.assertEqual(r.returncode, 2)
        self.assertIn("not found", r.stderr)

    def test_rejects_empty_file(self):
        with tempfile.NamedTemporaryFile(suffix=".js", dir="/tmp", delete=False) as f:
            name = f.name
        try:
            r = run_wrapper(name)
            self.assertEqual(r.returncode, 2)
            self.assertIn("empty", r.stderr)
        finally:
            os.unlink(name)

    def test_rejects_oversize_file(self):
        with tempfile.NamedTemporaryFile(suffix=".js", dir="/tmp", delete=False) as f:
            f.write(b"x" * (32768 + 1))
            name = f.name
        try:
            r = run_wrapper(name)
            self.assertEqual(r.returncode, 2)
            self.assertIn("too large", r.stderr)
        finally:
            os.unlink(name)

    def test_rejects_wrong_extension(self):
        with tempfile.NamedTemporaryFile(suffix=".txt", dir="/tmp", delete=False) as f:
            f.write(b"console.log(1)")
            name = f.name
        try:
            r = run_wrapper(name)
            self.assertEqual(r.returncode, 2)
        finally:
            os.unlink(name)

    def test_missing_deno_fails_honest(self):
        with tempfile.NamedTemporaryFile(suffix=".js", dir="/tmp", delete=False) as f:
            f.write(b"console.log(1)")
            name = f.name
        try:
            r = run_wrapper(name, env_extra={"CODEMODE_DENO_BIN": "/nonexistent/deno"})
            self.assertEqual(r.returncode, 2)
            self.assertIn("deno not executable", r.stderr)
        finally:
            os.unlink(name)

    def test_usage_without_args(self):
        r = run_wrapper()
        self.assertEqual(r.returncode, 2)
        self.assertIn("usage", r.stderr)


@unittest.skipUnless(DENO_PRESENT, "deno not installed")
class WrapperExecutionTests(unittest.TestCase):
    def test_runs_read_only_script(self):
        with tempfile.NamedTemporaryFile(suffix=".js", dir="/tmp", delete=False, mode="w") as f:
            f.write('const d = await Deno.readTextFile("/tmp");\n')
            f.write('console.log(JSON.stringify({ok: true}));\n')
            name = f.name
        try:
            r = run_wrapper(name)
            # reading /tmp as a file fails; the point is deno executed at all
            self.assertIn(r.returncode, (0, 1))
        finally:
            os.unlink(name)

    def test_network_denied(self):
        with tempfile.NamedTemporaryFile(suffix=".js", dir="/tmp", delete=False, mode="w") as f:
            f.write('await fetch("http://127.0.0.1:9/");\nconsole.log("reached");\n')
            name = f.name
        try:
            r = run_wrapper(name)
            self.assertNotEqual(r.returncode, 0)
            self.assertNotIn("reached", r.stdout)
        finally:
            os.unlink(name)

    def test_env_denied(self):
        with tempfile.NamedTemporaryFile(suffix=".js", dir="/tmp", delete=False, mode="w") as f:
            f.write('console.log(JSON.stringify({home: Deno.env.get("HOME")}));\n')
            name = f.name
        try:
            r = run_wrapper(name)
            self.assertNotEqual(r.returncode, 0)
        finally:
            os.unlink(name)


if __name__ == "__main__":
    unittest.main()
