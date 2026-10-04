"""Per-task inference-proxy manifest generator.

The containment contract (task_containment.load_config) binds a manifest to
exactly one task: contract.task_id / contract.trace_id / contract.model must
equal the task's identity at launch, and the proxy behind
config["proxy_socket"] must serve that exact contract on /_task-fabric-svn/contract.

A single static manifest cannot satisfy this for more than one task, so the
supervisor generates one manifest per dispatched task:

    <manifest_dir>/<task_id>.json          (owner-controlled, mode 0600)
    <socket_dir>/<task_id>.socket          (bound by the per-task proxy)

and starts task-fabric-svn-task-inference-proxy@<task_id>.service, whose ExecStart
points at that manifest. The worker then receives --containment-config
<manifest> and --model <model>; load_config's identity check passes because
all three were generated from the same task.

This module never touches the network except through task_containment's
verify_proxy handshake, never reads the HMAC secret (the proxy reads it from
its own EnvironmentFile), and never logs secret material (there is none here
to log).
"""

from __future__ import annotations

import json
import os
import re
import stat
import sys
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from task_containment import ContainmentError, trusted_file, verify_proxy

# ---------------------------------------------------------------------------
# Fixed Phase-1 policy. The model default is the first entry of the
# task-fabric-svn-harness-opencode grant allowlist
# (governor/governor/spend-gate/TASK_FABRIC_SVN_HARNESS_OPENCODE.md); the destination is
# the only inference route load_config accepts.
# ---------------------------------------------------------------------------

MANIFEST_SCHEMA = 1
DEFAULT_CONSUMER = "task-fabric-svn-harness-opencode"
DEFAULT_MODEL = "openrouter/qwen/qwen3.8-27b:free"
DESTINATION = "http://127.0.0.1:20128/v1/chat/completions"
MAX_REQUEST_BYTES = 1_048_576
MAX_REQUESTS = 64

# Same shape TaskContainment enforces for task identity: no path separators,
# no traversal, bounded length.
TASK_ID_RE = re.compile(r"task-[A-Za-z0-9_-]{1,100}\Z")


class ManifestError(RuntimeError):
    pass


def validate_task_id(task_id: str) -> str:
    if not isinstance(task_id, str) or not TASK_ID_RE.fullmatch(task_id):
        raise ManifestError(f"invalid task identity for manifest paths: {task_id!r}")
    return task_id


def manifest_paths(task_id: str, manifest_dir: Path, socket_dir: Path
                   ) -> tuple[Path, Path]:
    """Deterministic per-task paths. The task id is validated so it cannot
    escape the configured directories."""
    validate_task_id(task_id)
    return (Path(manifest_dir) / f"{task_id}.json",
            Path(socket_dir) / f"{task_id}.socket")


def _ensure_private_dir(path: Path) -> None:
    path.mkdir(parents=True, exist_ok=True)
    try:
        os.chmod(path, 0o700)
    except OSError as e:
        raise ManifestError(f"cannot secure directory {path}: {e}") from e
    st = path.stat()
    if st.st_uid != os.getuid() or st.st_mode & 0o077:
        raise ManifestError(f"directory not owner-private: {path}")


def build_manifest(task_id: str, trace_id: str, model: str, *,
                   harness_executable: str,
                   proxy_socket: str,
                   consumer: str = DEFAULT_CONSUMER,
                   destination: str = DESTINATION,
                   max_request_bytes: int = MAX_REQUEST_BYTES,
                   max_requests: int = MAX_REQUESTS) -> dict:
    """Build (but do not write) the manifest document load_config accepts."""
    validate_task_id(task_id)
    if not trace_id or not isinstance(trace_id, str):
        raise ManifestError("manifest requires a non-empty trace_id")
    if not model or not isinstance(model, str):
        raise ManifestError("manifest requires a non-empty model")
    if not consumer or not isinstance(consumer, str):
        raise ManifestError("manifest requires a non-empty consumer")
    return {
        "schema": MANIFEST_SCHEMA,
        "harness_executable": harness_executable,
        "proxy_socket": proxy_socket,
        "contract": {
            "task_id": task_id,
            "trace_id": trace_id,
            "model": model,
            "consumer": consumer,
            "destination": destination,
            "max_request_bytes": max_request_bytes,
            "max_requests": max_requests,
        },
    }


def generate_manifest(task_id: str, trace_id: str, model: str, *,
                      manifest_dir: Path, socket_dir: Path,
                      harness_executable: str,
                      consumer: str = DEFAULT_CONSUMER,
                      destination: str = DESTINATION,
                      max_request_bytes: int = MAX_REQUEST_BYTES,
                      max_requests: int = MAX_REQUESTS) -> Path:
    """Write the per-task manifest atomically (tmp + rename, mode 0600) and
    return its path.

    Idempotent: if a manifest already exists for the task and is byte-identical
    to what we would write, it is reused. If it exists but differs, we refuse
    -- silently swapping a live task's contract would break the proxy's
    /_task-fabric-svn/contract handshake mid-flight.
    """
    manifest_path, socket_path = manifest_paths(task_id, manifest_dir, socket_dir)
    _ensure_private_dir(manifest_path.parent)
    _ensure_private_dir(socket_path.parent)
    document = build_manifest(
        task_id, trace_id, model,
        harness_executable=harness_executable,
        proxy_socket=str(socket_path),
        consumer=consumer, destination=destination,
        max_request_bytes=max_request_bytes, max_requests=max_requests,
    )
    encoded = (json.dumps(document, indent=2, sort_keys=True) + "\n").encode("utf-8")
    if manifest_path.is_file():
        try:
            existing = trusted_file(manifest_path).read_text(encoding="utf-8")
        except ContainmentError as e:
            raise ManifestError(f"existing manifest not owner-controlled: {e}") from e
        if existing.encode("utf-8") == encoded:
            return manifest_path
        raise ManifestError(
            f"manifest for {task_id} already exists with a different contract; "
            "refusing to overwrite a live task's identity binding")
    tmp = manifest_path.with_name(manifest_path.name + ".tmp")
    try:
        with open(tmp, "wb") as f:
            f.write(encoded)
        os.chmod(tmp, 0o600)
        os.replace(tmp, manifest_path)
    except OSError as e:
        try:
            tmp.unlink()
        except OSError:
            pass
        raise ManifestError(f"cannot write manifest {manifest_path}: {e}") from e
    st = manifest_path.stat()
    if st.st_uid != os.getuid() or stat.S_IMODE(st.st_mode) != 0o600:
        raise ManifestError(f"manifest permissions wrong after write: {manifest_path}")
    return manifest_path


def proxy_ready(manifest_path: Path, timeout_s: float = 15.0) -> bool:
    """Poll the task's proxy until /_task-fabric-svn/contract serves the manifest's
    contract (the same handshake load_config performs), or the timeout
    expires. A contract *mismatch* is permanent: return False immediately
    rather than waiting out the clock."""
    deadline = time.monotonic() + timeout_s
    while True:
        try:
            raw = trusted_file(manifest_path).read_text(encoding="utf-8")
            config = json.loads(raw)
            verify_proxy(config)
            return True
        except ContainmentError:
            # Wrong contract served, unreadable manifest, or proxy refusing:
            # none of these heal by waiting.
            return False
        except (OSError, ValueError):
            # Socket not bound yet / proxy still starting: transient.
            pass
        if time.monotonic() >= deadline:
            return False
        time.sleep(0.5)
