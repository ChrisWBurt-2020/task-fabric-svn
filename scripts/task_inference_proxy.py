"""Trusted task inference proxy (server side of the task_containment contract).

The contained worker (scripts/task_containment.py) expects an owner-controlled
Unix-socket proxy that:

  - serves GET /_task-fabric-svn/contract with the exact owner-approved contract,
  - exposes ONLY the fixed chat-completions route,
  - ignores client auth and routing overrides,
  - enforces the contract's max_request_bytes / max_requests.

This proxy is additionally the binding point for spend-gate consumer
identity (Phase-1 production grant, consumer "task-fabric-svn-harness-opencode"):

  - every inbound x-task-fabric-svn-* header and every task_fabric_svn_* body metadata key is
    STRIPPED (a contained worker cannot spoof another consumer),
  - the proxy injects exactly one canonical set from the owner-controlled
    contract: x-task-fabric-svn-consumer, x-task-fabric-svn-trace-id, x-task-fabric-svn-work-id,
  - the proxy signs X-Task-Fabric-Svn-Consumer-Attestation (HMAC-SHA256, key from
    TASK_FABRIC_SVN_PROXY_HMAC_SECRET) so 9router can verify the claim came from this
    proxy and not from arbitrary localhost traffic.

The shared secret is deployment config: it is read from the environment,
never logged, never written anywhere. The proxy refuses to start without it
(fail closed).

Upstream is the contract destination (http://127.0.0.1:20128/v1/chat/completions);
--upstream overrides it (tests only).
"""

import argparse
import hashlib
import hmac
import http.client
import http.server
import json
import os
import socket
import socketserver
import stat
import sys
import threading
import time
from pathlib import Path
from urllib.parse import urlparse

sys.path.insert(0, str(Path(__file__).resolve().parent))
from task_containment import ContainmentError, trusted_file

HMAC_SECRET_ENV = "TASK_FABRIC_SVN_PROXY_HMAC_SECRET"
ROUTER_KEY_ENV = "TASK_FABRIC_SVN_9ROUTER_API_KEY"  # 9router client key, deployment config
ATTESTATION_VERSION = "v1"
INFERENCE_PATH = "/v1/chat/completions"

# Wire format shared with 9router's verifyConsumerAttestation:
# header "v1,<unix_ts>,<hex_hmac_sha256>" over "v1\n<consumer>\n<trace_id>\n<work_id>\n<ts>".


def sign_attestation(consumer, trace_id, work_id, secret, now_s=None):
    ts = str(int(now_s if now_s is not None else time.time()))
    msg = "\n".join([ATTESTATION_VERSION, consumer, trace_id or "", work_id or "", ts])
    mac = hmac.new(secret.encode("utf-8"), msg.encode("utf-8"), hashlib.sha256).hexdigest()
    return f"{ATTESTATION_VERSION},{ts},{mac}"


def load_proxy_config(path):
    """Validate the owner-controlled manifest (shape only; no network)."""
    raw = trusted_file(path).read_text()
    try:
        config = json.loads(raw)
    except ValueError as e:
        raise ContainmentError("containment_config_invalid") from e
    if set(config) != {"schema", "harness_executable", "proxy_socket", "contract"} or config["schema"] != 1:
        raise ContainmentError("containment_config_invalid")
    contract = config["contract"]
    if set(contract) != {"task_id", "trace_id", "model", "consumer", "destination",
                         "max_request_bytes", "max_requests"}:
        raise ContainmentError("containment_config_invalid")
    if not contract["trace_id"] or not isinstance(contract["consumer"], str) or not contract["consumer"]:
        raise ContainmentError("containment_config_invalid")
    if contract["destination"] != "http://127.0.0.1:20128/v1/chat/completions":
        raise ContainmentError("unsupported_inference_route")
    for field, maximum in (("max_request_bytes", 1_048_576), ("max_requests", 64)):
        if type(contract[field]) is not int or not 1 <= contract[field] <= maximum:
            raise ContainmentError("containment_config_invalid")
    binary = Path(config["harness_executable"])
    if not binary.is_absolute() or not binary.is_file() or not os.access(binary, os.X_OK):
        raise ContainmentError("missing_explicit_harness_executable")
    return config


_HOP_BY_HOP = {"connection", "keep-alive", "proxy-authenticate", "proxy-authorization",
               "te", "trailer", "transfer-encoding", "upgrade"}


def _is_task_fabric_svn_header(name):
    return name.lower().startswith("x-task-fabric-svn-")


def _strip_task_fabric_svn_metadata(body):
    """Remove task_fabric_svn_* correlation keys from an OpenAI metadata object."""
    metadata = body.get("metadata")
    if not isinstance(metadata, dict):
        return body
    cleaned = {k: v for k, v in metadata.items() if not k.lower().startswith("task_fabric_svn")}
    if len(cleaned) == len(metadata):
        return body
    out = dict(body)
    if cleaned:
        out["metadata"] = cleaned
    else:
        out.pop("metadata", None)
    return out


class ProxyHandler(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.0"  # close-delimited bodies; status line matches verify_proxy
    server_version = "TaskFabricSvnTaskInferenceProxy/1"

    def log_message(self, fmt, *args):  # keep stderr quiet; no header/body logging
        pass

    # -- helpers ---------------------------------------------------------
    def _send_json(self, status, payload):
        body = json.dumps(payload).encode("utf-8")
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def _contract(self):
        return self.server.proxy_contract

    # -- routes ----------------------------------------------------------
    def do_GET(self):
        if self.path.split("?", 1)[0] == "/_task-fabric-svn/contract":
            body = json.dumps(self._contract()).encode("utf-8")
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
        else:
            self._send_json(404, {"error": {"code": "not_found", "message": "unknown route"}})

    def do_POST(self):
        if self.path.split("?", 1)[0] != INFERENCE_PATH:
            self._send_json(404, {"error": {"code": "not_found", "message": "unknown route"}})
            return
        self._handle_inference()

    def do_PUT(self):  # only the fixed route exists; everything else is 404/405
        self._send_json(404, {"error": {"code": "not_found", "message": "unknown route"}})

    do_DELETE = do_PUT
    do_PATCH = do_PUT
    do_HEAD = do_PUT

    # -- inference -------------------------------------------------------
    def _handle_inference(self):
        contract = self._contract()
        with self.server.counter_lock:
            self.server.request_count += 1
            count = self.server.request_count
        if count > contract["max_requests"]:
            self._send_json(429, {"error": {
                "code": "task_proxy_limit_exceeded", "retryable": False,
                "message": f"task inference request limit reached ({contract['max_requests']})"}})
            return

        try:
            length = int(self.headers.get("Content-Length") or 0)
        except ValueError:
            length = 0
        if length > contract["max_request_bytes"]:
            self._send_json(413, {"error": {"code": "task_proxy_body_too_large", "retryable": False,
                                            "message": "request body exceeds contract limit"}})
            return
        raw = self.rfile.read(length) if length else b""
        if len(raw) > contract["max_request_bytes"]:
            self._send_json(413, {"error": {"code": "task_proxy_body_too_large", "retryable": False,
                                            "message": "request body exceeds contract limit"}})
            return
        try:
            body = json.loads(raw.decode("utf-8")) if raw else {}
        except ValueError:
            self._send_json(400, {"error": {"code": "task_proxy_bad_request", "retryable": False,
                                            "message": "request body is not JSON"}})
            return
        if not isinstance(body, dict) or body.get("model") != contract["model"]:
            # Routing overrides are ignored: the contract pins the model.
            self._send_json(400, {"error": {
                "code": "task_proxy_model_mismatch", "retryable": False,
                "message": "request model does not match the task contract"}})
            return
        body = _strip_task_fabric_svn_metadata(body)
        upstream_body = json.dumps(body).encode("utf-8")

        # Strip client auth + any client-supplied task_fabric_svn identity; inject the
        # single canonical set from the owner-controlled contract.
        upstream_headers = {}
        for name, value in self.headers.items():
            low = name.lower()
            if low in ("host", "content-length", "authorization") or low in _HOP_BY_HOP or _is_task_fabric_svn_header(name):
                continue
            upstream_headers[name] = value
        upstream_headers["X-Task-Fabric-Svn-Consumer"] = contract["consumer"]
        upstream_headers["X-Task-Fabric-Svn-Trace-Id"] = contract["trace_id"]
        upstream_headers["X-Task-Fabric-Svn-Work-Id"] = contract["task_id"]
        upstream_headers["X-Task-Fabric-Svn-Consumer-Attestation"] = sign_attestation(
            contract["consumer"], contract["trace_id"], contract["task_id"],
            self.server.hmac_secret)
        # The client-supplied Authorization was stripped above: only the proxy
        # authenticates to 9router, using the owner-provisioned client key from
        # the deployment environment. Never the sandbox's credentials.
        upstream_headers["Authorization"] = "Bearer " + self.server.router_api_key
        upstream_headers["Content-Length"] = str(len(upstream_body))

        upstream = self.server.upstream  # parsed destination
        try:
            conn = http.client.HTTPConnection(upstream.hostname, upstream.port, timeout=300)
            conn.request("POST", upstream.path or INFERENCE_PATH,
                         body=upstream_body, headers=upstream_headers)
            resp = conn.getresponse()
        except (OSError, http.client.HTTPException) as e:
            self._send_json(502, {"error": {"code": "task_proxy_upstream_unavailable",
                                            "retryable": False,
                                            "message": f"inference upstream unreachable: {type(e).__name__}"}})
            return

        try:
            self.send_response(resp.status)
            passthrough_length = None
            for name, value in resp.getheaders():
                low = name.lower()
                if low in _HOP_BY_HOP or low == "content-length":
                    if low == "content-length":
                        try:
                            passthrough_length = int(value)
                        except ValueError:
                            passthrough_length = None
                    continue
                self.send_header(name, value)
            if passthrough_length is not None:
                self.send_header("Content-Length", str(passthrough_length))
            self.end_headers()  # else close-delimited (HTTP/1.0)
            while True:
                chunk = resp.read(65536)
                if not chunk:
                    break
                self.wfile.write(chunk)
        except (BrokenPipeError, ConnectionResetError):
            pass
        finally:
            try:
                conn.close()
            except Exception:
                pass


class UnixProxyServer(socketserver.ThreadingMixIn, http.server.HTTPServer):
    address_family = socket.AF_UNIX
    daemon_threads = True


def build_server(config, upstream_url, hmac_secret, router_api_key):
    socket_path = Path(config["proxy_socket"])
    if socket_path.exists() or socket_path.is_symlink():
        try:
            info = socket_path.lstat()
        except FileNotFoundError:
            pass
        else:
            if stat.S_ISSOCK(info.st_mode):
                socket_path.unlink()  # stale socket from a previous run
            else:
                raise ContainmentError("proxy_socket_path_blocked")
    server = UnixProxyServer(str(socket_path), ProxyHandler)
    os.chmod(socket_path, 0o600)
    server.proxy_contract = config["contract"]
    server.upstream = urlparse(upstream_url)
    if not server.upstream.hostname or not server.upstream.port:
        raise ContainmentError("invalid_upstream")
    server.hmac_secret = hmac_secret
    server.router_api_key = router_api_key
    server.request_count = 0
    server.counter_lock = threading.Lock()
    return server


def main(argv=None):
    ap = argparse.ArgumentParser(description="Task Fabric SVN trusted task inference proxy")
    ap.add_argument("--config", required=True, help="owner-controlled containment manifest")
    ap.add_argument("--upstream", default=None,
                    help="override upstream URL (tests only; default: contract destination)")
    args = ap.parse_args(argv)
    try:
        config = load_proxy_config(args.config)
    except ContainmentError as e:
        print(f"proxy refusing to start: {e}", file=sys.stderr)
        return 2
    secret = os.environ.get(HMAC_SECRET_ENV, "")
    if not secret:
        # Fail closed: without the secret no attestation can be signed and
        # 9router would deny every request for attested consumers anyway.
        print(f"proxy refusing to start: {HMAC_SECRET_ENV} is not set", file=sys.stderr)
        return 2
    router_api_key = os.environ.get(ROUTER_KEY_ENV, "")
    if not router_api_key:
        # Fail closed: without the 9router client key every upstream request
        # would 401, so refuse to run a proxy that can only fail.
        print(f"proxy refusing to start: {ROUTER_KEY_ENV} is not set", file=sys.stderr)
        return 2
    upstream = args.upstream or config["contract"]["destination"]
    try:
        server = build_server(config, upstream, secret, router_api_key)
    except ContainmentError as e:
        print(f"proxy refusing to start: {e}", file=sys.stderr)
        return 2
    print(f"task inference proxy listening on {config['proxy_socket']} "
          f"for task {config['contract']['task_id']}", file=sys.stderr, flush=True)
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        pass
    return 0


if __name__ == "__main__":
    sys.exit(main())
