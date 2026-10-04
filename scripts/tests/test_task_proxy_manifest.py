"""Tests for task_proxy_manifest.py: per-task manifest generation and the
proxy-ready handshake.

No live ledger, no real workers, no provider requests, no credentials.
The integration class starts a real proxy process against a fake upstream
and proves the actual task_containment.load_config accepts the generated
manifest -- the same acceptance the worker performs at launch.
"""
import hashlib
import http.server
import json
import os
import stat
import subprocess
import sys
import tempfile
import threading
import time
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from task_proxy_manifest import (
    DEFAULT_CONSUMER,
    DEFAULT_MODEL,
    DESTINATION,
    ManifestError,
    build_manifest,
    generate_manifest,
    manifest_paths,
    proxy_ready,
)
from task_containment import load_config
import task_inference_proxy as proxy_mod


SECRET = "test-secret-for-unit-tests-only"


class FakeUpstreamHandler(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.0"

    def log_message(self, fmt, *args):
        pass

    def do_POST(self):
        length = int(self.headers.get("Content-Length") or 0)
        if length:
            self.rfile.read(length)
        payload = json.dumps({"id": "chatcmpl-test", "choices": []}).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)


class FakeUpstream(http.server.HTTPServer):
    allow_reuse_address = True

    def __init__(self):
        super().__init__(("127.0.0.1", 0), FakeUpstreamHandler)
        self.thread = threading.Thread(target=self.serve_forever, daemon=True)
        self.thread.start()

    @property
    def url(self):
        host, port = self.server_address
        return f"http://{host}:{port}/v1/chat/completions"

    def shutdown(self):
        super().shutdown()
        self.thread.join(timeout=5)


class ManifestGeneratorTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.tmpdir = Path(self.tmp.name)
        self.manifest_dir = self.tmpdir / "manifests"
        self.socket_dir = self.tmpdir / "sockets"

    def _gen(self, task_id="task-m1", trace_id="trace-m1", model=DEFAULT_MODEL):
        return generate_manifest(
            task_id, trace_id, model,
            manifest_dir=self.manifest_dir, socket_dir=self.socket_dir,
            harness_executable=sys.executable)

    def test_manifest_paths_deterministic_and_nested_inside_dirs(self):
        manifest, sock = manifest_paths("task-m1", self.manifest_dir,
                                       self.socket_dir)
        self.assertEqual(manifest, self.manifest_dir / "task-m1.json")
        self.assertEqual(sock, self.socket_dir / "task-m1.socket")

    def test_manifest_paths_reject_unsafe_task_ids(self):
        for bad in ("../evil", "task-/x", "TASK-M1", "task-", "task m1",
                    "task-m1.socket", "task-m1.json"):
            with self.subTest(task_id=bad):
                with self.assertRaises(ManifestError):
                    manifest_paths(bad, self.manifest_dir, self.socket_dir)

    def test_generate_writes_private_manifest_with_exact_contract(self):
        path = self._gen()
        self.assertTrue(path.is_file())
        st = path.stat()
        self.assertEqual(st.st_uid, os.getuid())
        self.assertEqual(stat.S_IMODE(st.st_mode), 0o600)
        self.assertEqual(stat.S_IMODE(self.manifest_dir.stat().st_mode), 0o700)
        self.assertEqual(stat.S_IMODE(self.socket_dir.stat().st_mode), 0o700)
        doc = json.loads(path.read_text(encoding="utf-8"))
        self.assertEqual(doc["schema"], 1)
        self.assertEqual(doc["harness_executable"], sys.executable)
        self.assertEqual(doc["proxy_socket"],
                         str(self.socket_dir / "task-m1.socket"))
        contract = doc["contract"]
        self.assertEqual(set(contract), {
            "task_id", "trace_id", "model", "consumer", "destination",
            "max_request_bytes", "max_requests"})
        self.assertEqual(contract["task_id"], "task-m1")
        self.assertEqual(contract["trace_id"], "trace-m1")
        self.assertEqual(contract["model"], DEFAULT_MODEL)
        self.assertEqual(contract["consumer"], DEFAULT_CONSUMER)
        self.assertEqual(contract["destination"], DESTINATION)

    def test_generate_idempotent_for_same_contract(self):
        first = self._gen()
        second = self._gen()
        self.assertEqual(first, second)

    def test_generate_refuses_overwrite_with_different_contract(self):
        self._gen(model="qwen/qwen3.8-27b:free")
        with self.assertRaises(ManifestError):
            self._gen(model="nvidia/nemotron-3-super-120b-a12b:free")

    def test_generate_refuses_empty_trace_id(self):
        with self.assertRaises(ManifestError):
            generate_manifest("task-m1", "", DEFAULT_MODEL,
                              manifest_dir=self.manifest_dir,
                              socket_dir=self.socket_dir,
                              harness_executable=sys.executable)

    def test_build_manifest_uses_defaults(self):
        doc = build_manifest("task-m9", "trace-9", "some-model",
                             harness_executable="/bin/true",
                             proxy_socket="/tmp/x.socket")
        self.assertEqual(doc["contract"]["consumer"], DEFAULT_CONSUMER)
        self.assertEqual(doc["contract"]["destination"], DESTINATION)
        self.assertEqual(doc["contract"]["model"], "some-model")


class ProxyReadyTests(unittest.TestCase):
    """Integration: generated manifest + real proxy process -> handshake."""

    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.tmpdir = Path(self.tmp.name)
        self.manifest_dir = self.tmpdir / "manifests"
        self.socket_dir = self.tmpdir / "sockets"
        self.task_id = "task-ready1"
        self.trace_id = "trace-ready1"
        self.manifest = generate_manifest(
            self.task_id, self.trace_id, DEFAULT_MODEL,
            manifest_dir=self.manifest_dir, socket_dir=self.socket_dir,
            harness_executable=sys.executable)
        self.upstream = FakeUpstream()
        self.proc = None

    def tearDown(self):
        if self.proc is not None:
            self.proc.terminate()
            try:
                self.proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                self.proc.kill()
        self.upstream.shutdown()

    def _start_proxy(self):
        env = dict(os.environ, TASK_FABRIC_SVN_PROXY_HMAC_SECRET=SECRET,
                     TASK_FABRIC_SVN_9ROUTER_API_KEY="router-key-test-value")
        self.proc = subprocess.Popen(
            [sys.executable, str(Path(proxy_mod.__file__)),
             "--config", str(self.manifest), "--upstream", self.upstream.url],
            env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE)

    def test_proxy_ready_true_with_real_proxy(self):
        self._start_proxy()
        self.assertTrue(proxy_ready(self.manifest, timeout_s=15))

    def test_proxy_ready_false_when_no_proxy_listening(self):
        self.assertFalse(proxy_ready(self.manifest, timeout_s=1))

    def test_full_chain_generated_manifest_passes_real_load_config(self):
        # The acceptance the worker performs at launch: real proxy serving
        # the generated manifest's contract, verified by the real
        # task_containment verifier with the same (task_id, trace_id, model).
        self._start_proxy()
        self.assertTrue(proxy_ready(self.manifest, timeout_s=15))
        config = load_config(str(self.manifest), self.task_id, self.trace_id,
                             DEFAULT_MODEL)
        self.assertEqual(config["contract"]["task_id"], self.task_id)
        self.assertEqual(config["contract"]["trace_id"], self.trace_id)
        self.assertEqual(config["contract"]["model"], DEFAULT_MODEL)

    def test_full_chain_identity_mismatch_refused(self):
        self._start_proxy()
        self.assertTrue(proxy_ready(self.manifest, timeout_s=15))
        from task_containment import ContainmentError
        with self.assertRaises(ContainmentError):
            load_config(str(self.manifest), self.task_id, "wrong-trace",
                        DEFAULT_MODEL)


if __name__ == "__main__":
    unittest.main()
