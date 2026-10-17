"""Actual OS-boundary tests for the trusted task inference proxy.

No provider requests, credentials or live state. A fake upstream records
what the proxy forwards; the real task_containment verifier cross-checks
the contract endpoint.
"""
import hashlib
import hmac
import http.client
import http.server
import json
import os
import socketserver
import subprocess
import sys
import tempfile
import threading
import time
import unittest
from pathlib import Path
from urllib.parse import urlparse

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from task_containment import load_config
import task_inference_proxy as proxy_mod


SECRET = "test-only-proxy-hmac-secret-0123456789"
ROUTER_KEY = "router-key-test-value"


class FakeUpstreamHandler(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.0"

    def log_message(self, fmt, *args):
        pass

    def do_POST(self):
        length = int(self.headers.get("Content-Length") or 0)
        raw = self.rfile.read(length) if length else b""
        try:
            body = json.loads(raw.decode()) if raw else {}
        except ValueError:
            body = {"_unparseable": True}
        self.server.requests.append({
            "path": self.path,
            "headers": {k.lower(): v for k, v in self.headers.items()},
            "body": body,
        })
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
        self.requests = []
        self.thread = threading.Thread(target=self.serve_forever, daemon=True)
        self.thread.start()

    @property
    def url(self):
        host, port = self.server_address
        return f"http://{host}:{port}/v1/chat/completions"


CONTRACT = {
    "task_id": "task-test1",
    "trace_id": "trace-1",
    "model": "qwen/qwen3.8-27b:free",
    "consumer": "task-fabric-svn-harness-opencode",
    "destination": "http://127.0.0.1:20128/v1/chat/completions",
    "max_request_bytes": 4096,
    "max_requests": 3,
}


def expected_attestation(consumer, trace_id, work_id, secret, ts):
    msg = "\n".join(["v1", consumer, trace_id, work_id, ts])
    return "v1,%s,%s" % (ts, hmac.new(secret.encode(), msg.encode(), hashlib.sha256).hexdigest())


class ProxyConfigShapeTests(unittest.TestCase):
    """load_proxy_config must tolerate the optional codemode_executable key.

    Regression: the codemode integration added the key to generated
    manifests, and the proxy's own strict shape check rejected every new
    manifest (proxy refusing to start: containment_config_invalid),
    wedging all task dispatch. The proxy only enforces the spend-gate
    keys; the optional key must stay in sync with
    task_containment.load_config's allowed set.
    """

    def _write_manifest(self, extra):
        tmp = Path(tempfile.mkdtemp(prefix="proxy-shape-"))
        self.addCleanup(lambda: __import__("shutil").rmtree(tmp, ignore_errors=True))
        manifest = {
            "schema": 1,
            "harness_executable": "/bin/true",
            "proxy_socket": str(tmp / "x.socket"),
            "contract": {
                "task_id": "task-x", "trace_id": "trace-x", "model": "m",
                "consumer": "c", "destination": "http://127.0.0.1:20128/v1/chat/completions",
                "max_request_bytes": 100, "max_requests": 10,
            },
        }
        manifest.update(extra)
        p = tmp / "manifest.json"
        p.write_text(json.dumps(manifest))
        os.chmod(p, 0o600)
        return p

    def test_accepts_manifest_without_codemode_key(self):
        cfg = proxy_mod.load_proxy_config(str(self._write_manifest({})))
        self.assertNotIn("codemode_executable", cfg)

    def test_accepts_manifest_with_codemode_key(self):
        cfg = proxy_mod.load_proxy_config(
            str(self._write_manifest({"codemode_executable": "/bin/true"})))
        self.assertEqual(cfg["codemode_executable"], "/bin/true")

    def test_rejects_unknown_key(self):
        with self.assertRaises(Exception):
            proxy_mod.load_proxy_config(
                str(self._write_manifest({"evil_key": "x"})))


class ProxyTest(unittest.TestCase):
    def setUp(self):
        self.tmp = Path(tempfile.mkdtemp(prefix="proxy-test-"))
        self.upstream = FakeUpstream()
        self.sock_path = self.tmp / "inference.sock"
        manifest = {
            "schema": 1,
            "harness_executable": "/bin/true",
            "proxy_socket": str(self.sock_path),
            "contract": dict(CONTRACT),
        }
        self.manifest_path = self.tmp / "manifest.json"
        self.manifest_path.write_text(json.dumps(manifest))
        os.chmod(self.manifest_path, 0o600)
        env = dict(os.environ, TASK_FABRIC_SVN_PROXY_HMAC_SECRET=SECRET,
                     TASK_FABRIC_SVN_9ROUTER_API_KEY=ROUTER_KEY)
        self.proc = subprocess.Popen(
            [sys.executable, str(Path(proxy_mod.__file__)), "--config", str(self.manifest_path),
             "--upstream", self.upstream.url],
            env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        deadline = time.time() + 10
        while time.time() < deadline:
            if self.sock_path.exists():
                break
            time.sleep(0.05)
        self.assertTrue(self.sock_path.exists(), "proxy did not bind its socket")

    def tearDown(self):
        self.proc.terminate()
        try:
            self.proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            self.proc.kill()
        self.upstream.shutdown()

    # -- helpers ------------------------------------------------------
    def _unix_request(self, method, path, headers=None, body=None):
        import socket as sockmod
        if isinstance(body, dict):
            raw = json.dumps(body).encode()
        elif isinstance(body, bytes):
            raw = body
        elif body is None:
            raw = b""
        else:
            raw = str(body).encode()
        lines = [f"{method} {path} HTTP/1.0", "Host: localhost"]
        for k, v in (headers or {}).items():
            lines.append(f"{k}: {v}")
        if raw:
            lines.append(f"Content-Length: {len(raw)}")
        head = ("\r\n".join(lines) + "\r\n\r\n").encode()
        with sockmod.socket(sockmod.AF_UNIX, sockmod.SOCK_STREAM) as c:
            c.settimeout(5)
            c.connect(str(self.sock_path))
            c.sendall(head + raw)
            out = bytearray()
            while True:
                chunk = c.recv(65536)
                if not chunk:
                    break
                out.extend(chunk)
        head_b, _, body_b = bytes(out).partition(b"\r\n\r\n")
        status = head_b.split(b"\r\n", 1)[0]
        return status, body_b

    def _post(self, body, headers=None):
        return self._unix_request("POST", "/v1/chat/completions", headers=headers, body=body)

    # -- tests --------------------------------------------------------
    def test_contract_endpoint_satisfies_real_verifier(self):
        # The actual worker-side verifier from task_containment accepts us.
        config = load_config(str(self.manifest_path), "task-test1", "trace-1",
                             "qwen/qwen3.8-27b:free")
        self.assertEqual(config["contract"], CONTRACT)

    def test_spoofed_consumer_header_is_replaced(self):
        status, _ = self._post(
            {"model": "qwen/qwen3.8-27b:free", "messages": []},
            headers={"X-Task-Fabric-Svn-Consumer": "homenode-voice",
                     "X-Task-Fabric-Svn-Trace-Id": "spoofed",
                     "Authorization": "Bearer worker-supplied"})
        self.assertTrue(status.endswith(b"200 OK"), status)
        self.assertEqual(len(self.upstream.requests), 1)
        seen = self.upstream.requests[0]["headers"]
        self.assertEqual(seen.get("x-task-fabric-svn-consumer"), "task-fabric-svn-harness-opencode")
        self.assertEqual(seen.get("x-task-fabric-svn-trace-id"), "trace-1")
        self.assertEqual(seen.get("x-task-fabric-svn-work-id"), "task-test1")
        # The sandbox's spoofed key must never reach upstream; the proxy
        # injects the owner-provisioned 9router client key instead.
        self.assertEqual(seen.get("authorization"), "Bearer " + ROUTER_KEY)

    def test_proxy_refuses_without_router_key(self):
        env = dict(os.environ, TASK_FABRIC_SVN_PROXY_HMAC_SECRET=SECRET)
        env.pop("TASK_FABRIC_SVN_9ROUTER_API_KEY", None)
        proc = subprocess.Popen(
            [sys.executable, str(Path(proxy_mod.__file__)), "--config", str(self.manifest_path),
             "--upstream", self.upstream.url],
            env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        try:
            _, err = proc.communicate(timeout=10)
        finally:
            if proc.poll() is None:
                proc.kill()
        self.assertEqual(proc.returncode, 2)
        self.assertIn(b"TASK_FABRIC_SVN_9ROUTER_API_KEY is not set", err)

    def test_attestation_is_valid(self):
        self._post({"model": "qwen/qwen3.8-27b:free", "messages": []})
        seen = self.upstream.requests[0]["headers"]
        att = seen.get("x-task-fabric-svn-consumer-attestation")
        self.assertIsNotNone(att)
        ver, ts, mac = att.split(",")
        self.assertEqual(ver, "v1")
        self.assertEqual(att, expected_attestation(
            "task-fabric-svn-harness-opencode", "trace-1", "task-test1", SECRET, ts))
        self.assertLess(abs(time.time() - int(ts)), 60)

    def test_spoofed_body_metadata_is_stripped(self):
        self._post({"model": "qwen/qwen3.8-27b:free", "messages": [],
                    "metadata": {"task_fabric_svn_consumer": "homenode-voice",
                                 "task_fabric_svnTraceId": "spoofed",
                                 "user_tag": "keep-me"}})
        seen_body = self.upstream.requests[0]["body"]
        metadata = seen_body.get("metadata", {})
        self.assertNotIn("task_fabric_svn_consumer", metadata)
        self.assertNotIn("task_fabric_svnTraceId", metadata)
        self.assertEqual(metadata.get("user_tag"), "keep-me")

    def test_model_override_is_rejected(self):
        status, body = self._post({"model": "deepseek/deepseek-v4-flash", "messages": []})
        self.assertTrue(status.endswith(b"400 Bad Request"), status)
        self.assertIn(b"task_proxy_model_mismatch", body)
        self.assertEqual(len(self.upstream.requests), 0)

    def test_max_requests_enforced(self):
        for _ in range(3):
            status, _ = self._post({"model": "qwen/qwen3.8-27b:free", "messages": []})
            self.assertTrue(status.endswith(b"200 OK"), status)
        status, body = self._post({"model": "qwen/qwen3.8-27b:free", "messages": []})
        self.assertTrue(status.endswith(b"429 Too Many Requests"), status)
        self.assertIn(b"task_proxy_limit_exceeded", body)
        self.assertEqual(len(self.upstream.requests), 3)

    def test_oversized_body_rejected(self):
        big = {"model": "qwen/qwen3.8-27b:free", "messages": [{"role": "user", "content": "x" * 9000}]}
        status, body = self._post(big)
        self.assertIn(b"413", status)
        self.assertIn(b"task_proxy_body_too_large", body)
        self.assertEqual(len(self.upstream.requests), 0)

    def test_unknown_route_404(self):
        status, _ = self._unix_request("GET", "/v1/models")
        self.assertTrue(status.endswith(b"404 Not Found"), status)

    def test_refuses_to_start_without_secret(self):
        env = {k: v for k, v in os.environ.items() if k != "TASK_FABRIC_SVN_PROXY_HMAC_SECRET"}
        proc = subprocess.Popen(
            [sys.executable, str(Path(proxy_mod.__file__)), "--config", str(self.manifest_path)],
            env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        try:
            rc = proc.wait(timeout=10)
        finally:
            if proc.poll() is None:
                proc.kill()
        self.assertNotEqual(rc, 0)
        _, err = proc.communicate()
        self.assertIn(b"TASK_FABRIC_SVN_PROXY_HMAC_SECRET", err)


if __name__ == "__main__":
    unittest.main()
