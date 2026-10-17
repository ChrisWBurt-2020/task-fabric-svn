"""Fail-closed task process boundary. No credentials or live configuration discovery.

The caller must supply an owner-controlled manifest and an already running,
restricted inference proxy. This module does not create budgets or proxies.
"""
import hashlib
import errno
import json
import os
import re
import secrets
import shutil
import signal
import socket
import stat
import subprocess
import tempfile
import time
from pathlib import Path


class ContainmentError(RuntimeError):
    pass


def trusted_file(path):
    path = Path(path)
    info = path.lstat()
    if not stat.S_ISREG(info.st_mode) or info.st_uid != os.getuid() or info.st_mode & 0o022:
        raise ContainmentError("containment manifest must be an owner-controlled regular file")
    return path.resolve()


def load_config(path, task_id, trace_id, model):
    if not path:
        raise ContainmentError("containment_config_required: no unrestricted worker fallback")
    try:
        config = json.loads(trusted_file(path).read_text())
        required = {"schema", "harness_executable", "proxy_socket", "contract"}
        optional = {"codemode_executable"}
        if not required <= set(config) <= required | optional or config["schema"] != 1:
            raise ValueError("invalid config")
        contract = config["contract"]
        if set(contract) != {"task_id", "trace_id", "model", "consumer", "destination", "max_request_bytes", "max_requests"}:
            raise ValueError("invalid proxy contract")
        if (contract["task_id"], contract["trace_id"], contract["model"]) != (task_id, trace_id, model):
            raise ValueError("task identity mismatch")
        if not trace_id or not contract["consumer"] or not isinstance(contract["consumer"], str):
            raise ValueError("missing approved consumer")
        if contract["destination"] != "http://127.0.0.1:20128/v1/chat/completions":
            raise ValueError("unsupported inference route")
        for field, maximum in (("max_request_bytes", 1_048_576), ("max_requests", 64)):
            if type(contract[field]) is not int or not 1 <= contract[field] <= maximum:
                raise ValueError("invalid inference limits")
        binary = Path(config["harness_executable"])
        if not binary.is_absolute() or not binary.is_file() or not os.access(binary, os.X_OK):
            raise ValueError("missing explicit harness executable")
        config["harness_executable"] = str(binary.resolve())
        if config.get("codemode_executable"):
            ce = Path(config["codemode_executable"])
            if not ce.is_absolute() or not ce.is_file() or not os.access(ce, os.X_OK):
                raise ValueError("invalid codemode executable")
            config["codemode_executable"] = str(ce.resolve())
        proxy = Path(config["proxy_socket"])
        info = proxy.lstat()
        if not proxy.is_absolute() or not stat.S_ISSOCK(info.st_mode) or info.st_uid != os.getuid() or info.st_mode & 0o022:
            raise ValueError("missing owner-controlled inference socket")
        verify_proxy(config)
        return config
    except (OSError, ValueError, TypeError, KeyError) as e:
        raise ContainmentError("containment_config_invalid_or_proxy_unavailable") from e


def verify_proxy(config):
    """Local control handshake only; never a provider or paid model request.

    The trusted proxy must enforce the exact contract, ignore client auth and
    routing overrides, and only expose the fixed chat-completions route.
    """
    with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as client:
        client.settimeout(2)
        client.connect(config["proxy_socket"])
        client.sendall(b"GET /_task-fabric-svn/contract HTTP/1.0\r\nHost: localhost\r\n\r\n")
        response = bytearray()
        while len(response) <= 8192:
            chunk = client.recv(4096)
            if not chunk:
                break
            response.extend(chunk)
        head, body = bytes(response).split(b"\r\n\r\n", 1)
        if len(response) > 8192 or head.split(b"\r\n", 1)[0] != b"HTTP/1.0 200 OK":
            raise ContainmentError("inference_proxy_contract_unavailable")
        if json.loads(body) != config["contract"]:
            raise ContainmentError("inference_proxy_contract_mismatch")


def protected(relative):
    parts = Path(relative).parts
    return any(p in {".git", ".opencode", ".config", ".ssh", ".gnupg"} or
               p == ".env" or p.startswith(".env.") or
               p in {"opencode.json", "opencode.jsonc"} for p in parts)


def tree_manifest(root):
    result = {}
    for path in sorted(Path(root).rglob("*")):
        rel = str(path.relative_to(root))
        info = path.lstat()
        if stat.S_ISLNK(info.st_mode) or not (stat.S_ISREG(info.st_mode) or stat.S_ISDIR(info.st_mode)):
            raise ContainmentError("unsupported_snapshot_entry: symlinks and special files cannot be exported")
        if path.is_file():
            result[rel] = {"sha256": hashlib.sha256(path.read_bytes()).hexdigest(), "mode": info.st_mode & 0o777}
    return result


def source_identity(root):
    """Stable identity for the checkout directory, not merely its pathname."""
    info = Path(root).stat()
    return {"path": str(Path(root).resolve(strict=True)), "device": info.st_dev,
            "inode": info.st_ino}


def repository_identity(root):
    root = Path(root).resolve(strict=True)

    def git_value(*args):
        proc = subprocess.run(["git", *args], cwd=root, capture_output=True,
                              text=True, timeout=10)
        if proc.returncode:
            return None
        return proc.stdout.strip()

    top = git_value("rev-parse", "--show-toplevel")
    common = git_value("rev-parse", "--git-common-dir")
    head = git_value("rev-parse", "--verify", "HEAD")
    index_path = git_value("rev-parse", "--git-path", "index")
    if top is None or common is None or index_path is None:
        raise ContainmentError("source_repository_identity_unavailable")
    common_path = Path(common)
    if not common_path.is_absolute():
        common_path = root / common_path
    index = Path(index_path)
    if not index.is_absolute():
        index = root / index
    try:
        index_hash = hashlib.sha256(index.read_bytes()).hexdigest()
    except FileNotFoundError:
        index_hash = None
    return {"top": str(Path(top).resolve(strict=True)),
            "common_git_dir": str(common_path.resolve(strict=True)),
            "head": head, "index_sha256": index_hash}


def source_files(root):
    """Return the same safe, non-protected file set exported to a snapshot."""
    root = Path(root).resolve(strict=True)
    listing = subprocess.run(["git", "-c", "core.fsmonitor=false", "ls-files", "--cached",
                              "--others", "--exclude-standard", "-z"], cwd=root,
                             capture_output=True, check=True, timeout=30)
    files = {}
    for raw in set(listing.stdout.split(b"\0")) - {b""}:
        relative = os.fsdecode(raw)
        if protected(relative):
            continue
        path = _checked_relative(root, relative, allow_missing=True)
        if not path.exists():
            continue  # tracked deletion
        info = path.lstat()
        if not stat.S_ISREG(info.st_mode):
            raise ContainmentError("source_contains_unsupported_entry")
        files[relative] = {"sha256": hashlib.sha256(path.read_bytes()).hexdigest(),
                           "mode": info.st_mode & 0o777}
    return files


def _validated_relative(relative):
    if not isinstance(relative, str) or not relative or "\\" in relative:
        raise ContainmentError("unsafe_patch_path")
    candidate = Path(relative)
    if (candidate.is_absolute() or candidate.as_posix() != relative or
            any(part in {"", ".", ".."} for part in candidate.parts)):
        raise ContainmentError("unsafe_patch_path")
    return candidate


def _checked_relative(root, relative, allow_missing=False):
    """Resolve an untrusted relative path without traversing symlinks."""
    candidate = _validated_relative(relative)
    root = Path(root).resolve(strict=True)
    cursor = root
    for part in candidate.parts:
        cursor = cursor / part
        try:
            info = cursor.lstat()
        except FileNotFoundError:
            if not allow_missing:
                raise ContainmentError("unsafe_patch_path")
            continue
        if stat.S_ISLNK(info.st_mode):
            raise ContainmentError("unsafe_patch_symlink")
        if cursor != root and cursor.exists() and not cursor.resolve(strict=True).is_relative_to(root):
            raise ContainmentError("unsafe_patch_path")
    resolved = cursor.resolve(strict=not allow_missing)
    if not resolved.is_relative_to(root):
        raise ContainmentError("unsafe_patch_path")
    return cursor


def _atomic_json(path, value):
    path = Path(path)
    fd, temporary = tempfile.mkstemp(prefix=".task-fabric-svn-task-", dir=path.parent)
    try:
        with os.fdopen(fd, "w") as stream:
            json.dump(value, stream, sort_keys=True)
            stream.flush()
            os.fsync(stream.fileno())
        os.chmod(temporary, 0o600)
        os.replace(temporary, path)
        _fsync_directory(path.parent)
    finally:
        try:
            os.unlink(temporary)
        except FileNotFoundError:
            pass


def _fsync_directory(path):
    fd = os.open(path, os.O_RDONLY | getattr(os, "O_DIRECTORY", 0))
    try:
        os.fsync(fd)
    finally:
        os.close(fd)


def _fsync_directory_fd(directory_fd):
    os.fsync(directory_fd)


def _fsync_file_at(directory_fd, name):
    fd = os.open(name, os.O_RDONLY | os.O_NOFOLLOW, dir_fd=directory_fd)
    try:
        os.fsync(fd)
    finally:
        os.close(fd)


def _open_parent_at(root_fd, relative, *, create=False, allow_missing=False):
    parts = Path(relative).parts
    if not parts or Path(relative).is_absolute() or any(part in {"", ".", ".."} for part in parts):
        raise ContainmentError("unsafe_patch_path")
    directory_fd = os.dup(root_fd)
    try:
        for component in parts[:-1]:
            try:
                child_fd = os.open(component, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW,
                                   dir_fd=directory_fd)
            except FileNotFoundError:
                if create:
                    os.mkdir(component, mode=0o755, dir_fd=directory_fd)
                    os.fsync(directory_fd)
                    child_fd = os.open(component, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW,
                                       dir_fd=directory_fd)
                elif allow_missing:
                    return None, parts[-1]
                else:
                    raise ContainmentError("unsafe_patch_path")
            except OSError as exc:
                if exc.errno in {errno.ELOOP, errno.ENOTDIR}:
                    raise ContainmentError("unsafe_patch_symlink") from exc
                raise
            os.close(directory_fd)
            directory_fd = child_fd
        result = directory_fd
        directory_fd = -1
        return result, parts[-1]
    finally:
        if directory_fd >= 0:
            os.close(directory_fd)


def _fd_parent_path(directory_fd):
    try:
        value = os.readlink(f"/proc/self/fd/{directory_fd}")
    except OSError as exc:
        raise ContainmentError("patch_parent_identity_unavailable") from exc
    return value.removesuffix(" (deleted)")


def _verify_fd_parent(directory_fd, root, relative):
    expected = Path(root) / Path(relative).parent
    expected = expected.resolve(strict=True)
    actual = Path(_fd_parent_path(directory_fd))
    if actual != expected or not actual.is_relative_to(Path(root).resolve(strict=True)):
        raise ContainmentError("patch_parent_changed")


def _signature_at(directory_fd, name):
    try:
        file_fd = os.open(name, os.O_RDONLY | os.O_NOFOLLOW, dir_fd=directory_fd)
    except FileNotFoundError:
        return None
    except OSError as exc:
        if exc.errno in {errno.ELOOP, errno.ENOTDIR}:
            raise ContainmentError("unsafe_patch_symlink") from exc
        raise
    try:
        info = os.fstat(file_fd)
        if not stat.S_ISREG(info.st_mode):
            raise ContainmentError("unsafe_patch_target")
        digest = hashlib.sha256()
        while True:
            block = os.read(file_fd, 1024 * 1024)
            if not block:
                break
            digest.update(block)
        return {"sha256": digest.hexdigest(), "mode": info.st_mode & 0o777}
    finally:
        os.close(file_fd)


def _read_file_at(directory_fd, name):
    file_fd = os.open(name, os.O_RDONLY | os.O_NOFOLLOW, dir_fd=directory_fd)
    try:
        info = os.fstat(file_fd)
        if not stat.S_ISREG(info.st_mode):
            raise ContainmentError("unsafe_patch_target")
        data = bytearray()
        while True:
            block = os.read(file_fd, 1024 * 1024)
            if not block:
                break
            data.extend(block)
        return bytes(data)
    finally:
        os.close(file_fd)


def _create_temp_at(directory_fd, prefix):
    for _ in range(16):
        name = prefix + secrets.token_hex(12)
        try:
            fd = os.open(name, os.O_RDWR | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW,
                         0o600, dir_fd=directory_fd)
            return name, fd
        except FileExistsError:
            continue
    raise ContainmentError("patch_staging_name_exhausted")


def stop_group(proc):
    """Kill the entire child session, including background grandchildren."""
    try:
        os.killpg(proc.pid, signal.SIGKILL)
    except ProcessLookupError:
        pass
    proc.wait()


def run_process(argv, timeout, cancelled=lambda: False):
    proc = subprocess.Popen(argv, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE,
                            stderr=subprocess.PIPE, text=True, start_new_session=True,
                            env={"PATH": "/usr/bin:/bin", "LANG": "C.UTF-8"})
    deadline = time.monotonic() + timeout
    try:
        while True:
            if cancelled():
                stop_group(proc)
                return 125, "", "task_cancelled"
            if time.monotonic() >= deadline:
                stop_group(proc)
                return 124, "", "contained_harness_timeout"
            try:
                out, err = proc.communicate(timeout=min(0.2, max(0.001, deadline - time.monotonic())))
                # Kill any orphan remaining after the group leader exited too.
                stop_group(proc)
                return proc.returncode, out, err
            except subprocess.TimeoutExpired:
                continue
    finally:
        stop_group(proc)
        if proc.stdout:
            proc.stdout.close()
        if proc.stderr:
            proc.stderr.close()


class TaskContainment:
    def __init__(self, source, state_root, task_id, config, resume=False):
        if not re.fullmatch(r"task-[A-Za-z0-9_-]{1,100}", task_id):
            raise ContainmentError("invalid_task_identity")
        self.config = config
        self.source = Path(source).resolve(strict=True)
        self.root = Path(state_root).resolve() / task_id
        self.workspace = self.root / "workspace"
        self.home = self.root / "home"
        self.bwrap = shutil.which("bwrap")
        self.socat = shutil.which("socat")
        if not self.bwrap or not self.socat:
            raise ContainmentError("containment_runtime_unavailable")
        if self.root.is_relative_to(self.source):
            raise ContainmentError("task_state_must_be_outside_source")
        self.root.mkdir(parents=True, exist_ok=True, mode=0o700)
        if self.root.is_symlink() or self.root.stat().st_mode & 0o077:
            raise ContainmentError("task_state_must_be_private")
        import fcntl
        self.lock = (self.root / "worker.lock").open("a")
        try:
            fcntl.flock(self.lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except OSError as e:
            raise ContainmentError("task_worker_already_running") from e
        if resume:
            metadata = json.loads((self.root / "baseline.json").read_text())
            if (metadata.get("source_identity") != source_identity(self.source) or
                    metadata.get("repository_identity") != repository_identity(self.source) or
                    metadata.get("source") != str(self.source) or
                    metadata.get("contract") != config["contract"]):
                raise ContainmentError("snapshot_resume_identity_mismatch")
            self.baseline = metadata["files"]
            if not self.workspace.is_dir() or self.workspace.is_symlink() or self.home.is_symlink():
                raise ContainmentError("snapshot_resume_invalid")
        else:
            if self.workspace.exists():
                raise ContainmentError("snapshot_already_exists_use_resume")
            self.workspace.mkdir(mode=0o700)
            self.home.mkdir(mode=0o700)
            snapshot_source_identity = source_identity(self.source)
            snapshot_repository_identity = repository_identity(self.source)
            snapshot_source_files = source_files(self.source)
            # Use the repository index only to choose paths; do not clone hooks,
            # credentials, ignored local secrets, config, or the shared gitdir.
            listing = subprocess.run(["git", "-c", "core.fsmonitor=false", "ls-files", "--cached",
                                      "--others", "--exclude-standard", "-z"], cwd=self.source,
                                     capture_output=True, check=True, timeout=30)
            for raw in set(listing.stdout.split(b"\0")) - {b""}:
                relative = os.fsdecode(raw)
                if protected(relative):
                    continue
                src = self.source / relative
                if not src.exists() and not src.is_symlink():
                    continue  # tracked deletion
                if src.is_symlink() or not src.resolve().is_relative_to(self.source) or not src.is_file():
                    raise ContainmentError("source_contains_unsupported_entry")
                dest = self.workspace / relative
                dest.parent.mkdir(parents=True, exist_ok=True)
                shutil.copyfile(src, dest, follow_symlinks=False)
                dest.chmod(0o755 if src.stat().st_mode & 0o111 else 0o644)
            self.baseline = tree_manifest(self.workspace)
            if (source_identity(self.source) != snapshot_source_identity or
                    repository_identity(self.source) != snapshot_repository_identity or
                    source_files(self.source) != snapshot_source_files):
                raise ContainmentError("snapshot_source_changed_during_copy")
            metadata = {"source": str(self.source), "source_identity": snapshot_source_identity,
                        "repository_identity": snapshot_repository_identity,
                        "source_files": snapshot_source_files, "contract": config["contract"],
                        "files": self.baseline}
            _atomic_json(self.root / "baseline.json", metadata)
        self.settings = self.root / "opencode.json"
        self.settings.write_text(json.dumps({
            "$schema": "https://opencode.ai/config.json", "autoupdate": False,
            "share": "disabled", "permission": {"*": "deny", "read": "allow", "edit": "allow",
                "glob": "allow", "grep": "allow", "list": "allow", "bash": "allow",
                "external_directory": "deny"},
            "provider": {"task-fabric-svn-contained": {"npm": "@ai-sdk/openai-compatible",
                "options": {"baseURL": "http://127.0.0.1:19876/v1", "apiKey": "contained-no-credential"},
                "models": {config["contract"]["model"]: {"name": config["contract"]["model"]}}}},
        }))

    def argv(self, command, inference=True):
        args = [self.bwrap, "--unshare-all", "--die-with-parent", "--new-session", "--clearenv",
                "--ro-bind", "/usr", "/usr", "--symlink", "usr/bin", "/bin",
                "--symlink", "usr/lib", "/lib", "--symlink", "usr/lib", "/lib64",
                "--proc", "/proc", "--dev", "/dev", "--tmpfs", "/tmp", "--tmpfs", "/run",
                "--bind", str(self.workspace), "/workspace", "--bind", str(self.home), "/home/worker",
                "--dir", "/run/task_fabric_svn",
                "--ro-bind", str(self.settings), "/run/task_fabric_svn/opencode.json",
                "--setenv", "HOME", "/home/worker", "--setenv", "PATH", "/usr/bin:/bin",
                "--setenv", "LANG", "C.UTF-8", "--setenv", "OPENCODE_CONFIG", "/run/task_fabric_svn/opencode.json",
                "--setenv", "OPENCODE_DISABLE_PROJECT_CONFIG", "true", "--chdir", "/workspace"]
        if inference:
            # --dir first: bwrap silently leaves a bind unmounted when the
            # destination's parent directory does not exist in the sandbox,
            # which made the harness/socket/settings files vanish (a live
            # dispatch died with rc=1 before any inference).
            args += ["--dir", "/opt/task_fabric_svn",
                     "--ro-bind", self.config["harness_executable"], "/opt/task_fabric_svn/opencode",
                     "--ro-bind", self.config["proxy_socket"], "/run/task_fabric_svn/inference.sock"]
            # Codemode (experimental): bind the wrapper read-only when the
            # manifest carries it. load_config validated the path; absent
            # means an older supervisor generated the manifest, and the
            # worker's prompt degrades gracefully (see codemode_available).
            codemode_src = self.config.get("codemode_executable")
            if codemode_src:
                args += ["--ro-bind", codemode_src, "/opt/task_fabric_svn/codemode"]
            # Literal script; no prompt, path or model is interpolated into shell.
            args += ["--", "/bin/sh", "-c",
                     "socat TCP-LISTEN:19876,bind=127.0.0.1,reuseaddr,fork UNIX-CONNECT:/run/task_fabric_svn/inference.sock &\n"
                     "relay=$!\ntrap 'kill $relay 2>/dev/null || true' EXIT\n\"$@\"", "task-fabric-svn-contained", *command]
        else:
            args += ["--", *command]
        return args

    def run(self, command, timeout, cancelled=lambda: False):
        try:
            verify_proxy(self.config)  # resumed turns cannot outlive their proxy authorization
        except (OSError, ValueError) as e:
            raise ContainmentError("inference_proxy_unavailable") from e
        return run_process(self.argv(command), timeout, cancelled)

    def finish(self):
        after = tree_manifest(self.workspace)
        if any(protected(name) for name in after):
            raise ContainmentError("protected_output_unavailable")
        changes = sorted(name for name in self.baseline.keys() | after.keys()
                         if self.baseline.get(name) != after.get(name))
        manifest = self.root / "result.json"
        manifest.write_text(json.dumps({"schema": 1, "applied": False, "changes": changes,
                                        "files": after}, sort_keys=True))
        return manifest, changes

    def apply_result(self):
        """Apply a finished snapshot only if the exact source baseline still holds.

        This is deliberately separate from ``finish`` and worker completion:
        staging a model-produced result never mutates the checkout by itself.
        Descriptor-relative O_NOFOLLOW operations stop parent-symlink swaps
        from redirecting writes. A concurrent process with this same OS user
        can still rename an already-open parent directory after validation;
        the worker cannot do that because its sandbox mounts only workspace.
        """
        import fcntl
        directory_fd = os.open(self.source, os.O_RDONLY | getattr(os, "O_DIRECTORY", 0) |
                                os.O_NOFOLLOW)
        try:
            fcntl.flock(directory_fd, fcntl.LOCK_EX)
            return self._apply_result_locked(directory_fd)
        finally:
            fcntl.flock(directory_fd, fcntl.LOCK_UN)
            os.close(directory_fd)

    def _apply_result_locked(self, source_fd):
        baseline_path = self.root / "baseline.json"
        result_path = self.root / "result.json"
        receipt_path = self.root / "apply-receipt.json"
        for path in (baseline_path, result_path):
            info = path.lstat()
            if (not stat.S_ISREG(info.st_mode) or info.st_uid != os.getuid() or
                    info.st_mode & 0o022):
                raise ContainmentError("unsafe_patch_metadata")
        if receipt_path.exists():
            receipt_info = receipt_path.lstat()
            if (not stat.S_ISREG(receipt_info.st_mode) or receipt_info.st_uid != os.getuid() or
                    receipt_info.st_mode & 0o022):
                raise ContainmentError("unsafe_patch_metadata")
            receipt = json.loads(receipt_path.read_text())
            if (receipt.get("source_identity") != source_identity(self.source) or
                    receipt.get("repository_identity") != repository_identity(self.source)):
                raise ContainmentError("patch_source_identity_mismatch")
            if receipt.get("state") == "applied" and receipt.get("task_id") == self.root.name:
                self._cleanup_apply_backups(receipt, source_fd)
                return list(receipt.get("changes", []))
            if receipt.get("state") == "applying" and receipt.get("task_id") == self.root.name:
                self._recover_apply(receipt_path, receipt, source_fd)
                receipt_path.unlink()
                _fsync_directory(self.root)
            elif receipt.get("state") == "recovered" and receipt.get("task_id") == self.root.name:
                receipt_path.unlink()
                _fsync_directory(self.root)
            else:
                raise ContainmentError("patch_application_already_started")

        try:
            baseline = json.loads(baseline_path.read_text())
            result = json.loads(result_path.read_text())
        except (OSError, ValueError) as exc:
            raise ContainmentError("invalid_patch_metadata") from exc
        if not isinstance(baseline, dict) or not isinstance(result, dict):
            raise ContainmentError("invalid_patch_metadata")
        if (baseline.get("source_identity") != source_identity(self.source) or
                baseline.get("repository_identity") != repository_identity(self.source) or
                baseline.get("source") != str(self.source) or
                baseline.get("contract") != self.config["contract"]):
            raise ContainmentError("patch_source_identity_mismatch")
        if set(result) != {"schema", "applied", "changes", "files"} or result["schema"] != 1:
            raise ContainmentError("invalid_patch_manifest")
        if result["applied"] is not False:
            raise ContainmentError("patch_already_applied_or_invalid")

        snapshot_baseline = baseline.get("files")
        source_baseline = baseline.get("source_files")
        output = result.get("files")
        changes = result.get("changes")
        if not all(isinstance(v, dict) for v in (snapshot_baseline, source_baseline, output)):
            raise ContainmentError("invalid_patch_manifest")
        if not isinstance(changes, list) or any(not isinstance(p, str) for p in changes):
            raise ContainmentError("invalid_patch_manifest")
        for files in (snapshot_baseline, source_baseline, output):
            for relative, entry in files.items():
                if not isinstance(relative, str):
                    raise ContainmentError("unsafe_patch_path")
                if protected(relative):
                    raise ContainmentError("protected_output_unavailable")
                _checked_relative(self.workspace if files is not source_baseline else self.source,
                                  relative, allow_missing=True)
                if (not isinstance(entry, dict) or set(entry) != {"sha256", "mode"} or
                        not isinstance(entry["sha256"], str) or
                        not re.fullmatch(r"[0-9a-f]{64}", entry["sha256"]) or
                        type(entry["mode"]) is not int or not 0 <= entry["mode"] <= 0o777):
                    raise ContainmentError("invalid_patch_manifest")

        actual_output = tree_manifest(self.workspace)
        if actual_output != output:
            raise ContainmentError("snapshot_result_changed")
        actual_changes = sorted(name for name in snapshot_baseline.keys() | output.keys()
                                if snapshot_baseline.get(name) != output.get(name))
        if changes != actual_changes:
            raise ContainmentError("patch_change_list_mismatch")
        if source_files(self.source) != source_baseline:
            raise ContainmentError("patch_source_conflict")

        # Prepare every replacement and rollback copy before touching source.
        staged = {}
        created_dirs = []
        created_dir_names = set()
        journal_operations = []
        journal_written = False
        try:
            for relative in changes:
                target = _checked_relative(self.source, relative, allow_missing=True)
                if target.exists() and not target.is_file():
                    raise ContainmentError("unsafe_patch_target")
                parent = target.parent
                missing = []
                cursor = parent
                while not cursor.exists():
                    missing.append(cursor)
                    cursor = cursor.parent
                if cursor.is_symlink() or not cursor.resolve(strict=True).is_relative_to(self.source):
                    raise ContainmentError("unsafe_patch_path")
                for directory in reversed(missing):
                    relative_directory = directory.relative_to(self.source).as_posix()
                    if relative_directory not in created_dir_names:
                        created_dirs.append(directory)
                        created_dir_names.add(relative_directory)
                if parent.exists() and (parent.is_symlink() or
                                        not parent.resolve(strict=True).is_relative_to(self.source)):
                    raise ContainmentError("unsafe_patch_path")
                old = source_baseline.get(relative)
                source_parent_fd, target_name = _open_parent_at(
                    source_fd, relative, allow_missing=True)
                staged[relative] = {"target": target, "backup": None,
                                    "replacement": None, "old": old, "parent_fd": None,
                                    "source_read_fd": source_parent_fd,
                                    "target_name": target_name}
                if source_parent_fd is None:
                    current = None
                else:
                    _verify_fd_parent(source_parent_fd, self.source, relative)
                    current = _signature_at(source_parent_fd, target_name)
                if current != old:
                    if source_parent_fd is not None:
                        os.close(source_parent_fd)
                        staged[relative]["source_read_fd"] = None
                    raise ContainmentError("patch_source_conflict")
                backup = None
                staged[relative]["source_read_fd"] = source_parent_fd
                if old is not None:
                    backup, fd = _create_temp_at(source_fd, ".task-fabric-svn-task-backup-")
                    staged[relative]["backup"] = backup
                    with os.fdopen(fd, "wb") as stream:
                        _verify_fd_parent(source_parent_fd, self.source, relative)
                        stream.write(_read_file_at(source_parent_fd, target_name))
                        _verify_fd_parent(source_parent_fd, self.source, relative)
                        stream.flush()
                        os.fsync(stream.fileno())
                    os.chmod(backup, old["mode"], dir_fd=source_fd, follow_symlinks=False)
                    _fsync_file_at(source_fd, backup)
                replacement = None
                new = output.get(relative)
                if new is not None:
                    replacement, fd = _create_temp_at(source_fd, ".task-fabric-svn-task-patch-")
                    staged[relative]["replacement"] = replacement
                    with os.fdopen(fd, "wb") as stream:
                        workspace_target = _checked_relative(self.workspace, relative)
                        stream.write(workspace_target.read_bytes())
                        stream.flush()
                        os.fsync(stream.fileno())
                    os.chmod(replacement, new["mode"], dir_fd=source_fd, follow_symlinks=False)
                    _fsync_file_at(source_fd, replacement)
                if source_parent_fd is not None:
                    os.close(source_parent_fd)
                    staged[relative]["source_read_fd"] = None
                journal_operations.append({"path": relative, "existed": old is not None,
                                           "backup": backup,
                                           "old": old, "new": new})
                _fsync_directory_fd(source_fd)

            journal = {"schema": 1, "task_id": self.root.name,
                       "source_identity": baseline["source_identity"],
                       "repository_identity": baseline["repository_identity"], "state": "applying",
                       "changes": changes, "operations": journal_operations,
                       "created_dirs": [p.relative_to(self.source).as_posix() for p in created_dirs]}
            _atomic_json(receipt_path, journal)
            journal_written = True
            for relative in changes:
                item = staged[relative]
                target = item["target"]
                if (source_identity(self.source) != baseline["source_identity"] or
                        repository_identity(self.source) != baseline["repository_identity"]):
                    raise ContainmentError("patch_source_identity_mismatch")
                parent_fd, target_name = _open_parent_at(source_fd, relative, create=True)
                item["parent_fd"] = parent_fd
                item["target_name"] = target_name
                _verify_fd_parent(parent_fd, self.source, relative)
                if _signature_at(parent_fd, target_name) != item["old"]:
                    raise ContainmentError("patch_source_conflict")
                # The journal and its backup are durable before any mutation;
                # recovery can restore this path even if the next syscall fails.
                if item["replacement"] is None:
                    os.unlink(target_name, dir_fd=parent_fd)
                else:
                    os.replace(item["replacement"], target_name,
                               src_dir_fd=source_fd, dst_dir_fd=parent_fd)
                    item["replacement"] = None
                _fsync_directory_fd(parent_fd)
                _verify_fd_parent(parent_fd, self.source, relative)
                os.close(parent_fd)
                item["parent_fd"] = None
            journal["state"] = "applied"
            _atomic_json(receipt_path, journal)
            journal_written = False
            self._cleanup_apply_backups(journal, source_fd)
            return changes
        except Exception as original:
            if receipt_path.exists() or journal_written:
                try:
                    durable_journal = (json.loads(receipt_path.read_text()) if receipt_path.exists()
                                       else journal)
                    if durable_journal.get("state") == "applied":
                        journal_written = False
                        return changes
                    if durable_journal.get("state") == "applying":
                        self._recover_apply(receipt_path, durable_journal, source_fd, staged)
                        if receipt_path.exists():
                            receipt_path.unlink()
                        _fsync_directory(self.root)
                        journal_written = False
                except Exception as recovery_error:
                    raise ContainmentError("patch_rollback_incomplete; recovery journal retained") from recovery_error
            raise
        finally:
            for item in staged.values():
                parent_fd = item.get("parent_fd")
                if parent_fd is not None:
                    os.close(parent_fd)
                    item["parent_fd"] = None
                source_read_fd = item.get("source_read_fd")
                if source_read_fd is not None:
                    os.close(source_read_fd)
                    item["source_read_fd"] = None
                for key in (("replacement",) if journal_written else ("backup", "replacement")):
                    temporary = item[key]
                    if temporary:
                        try:
                            os.unlink(temporary, dir_fd=source_fd)
                        except FileNotFoundError:
                            pass

    def _recover_apply(self, receipt_path, receipt, source_fd, staged=None):
        if (not isinstance(receipt, dict) or receipt.get("schema") != 1 or
                receipt.get("task_id") != self.root.name or receipt.get("state") != "applying" or
                receipt.get("source_identity") != source_identity(self.source) or
                receipt.get("repository_identity") != repository_identity(self.source) or
                not isinstance(receipt.get("operations"), list)):
            raise ContainmentError("invalid_patch_recovery_journal")
        seen = set()
        recovery_errors = []
        for operation in reversed(receipt["operations"]):
            if not isinstance(operation, dict) or set(operation) != {"path", "existed", "backup", "old", "new"}:
                raise ContainmentError("invalid_patch_recovery_journal")
            relative = operation["path"]
            if not isinstance(relative, str) or relative in seen or protected(relative):
                raise ContainmentError("invalid_patch_recovery_journal")
            seen.add(relative)
            retained_fd = (staged or {}).get(relative, {}).get("parent_fd")
            if retained_fd is None:
                _checked_relative(self.source, relative, allow_missing=True)
            else:
                _validated_relative(relative)
            old, new, existed = operation["old"], operation["new"], operation["existed"]
            if type(existed) is not bool or existed != (old is not None):
                raise ContainmentError("invalid_patch_recovery_journal")
            for signature in (old, new):
                if signature is not None and (not isinstance(signature, dict) or
                        set(signature) != {"sha256", "mode"} or
                        not isinstance(signature.get("sha256"), str) or
                        not re.fullmatch(r"[0-9a-f]{64}", signature["sha256"]) or
                        type(signature.get("mode")) is not int or
                        not 0 <= signature["mode"] <= 0o777):
                    raise ContainmentError("invalid_patch_recovery_journal")
            backup_name = operation["backup"]
            if existed:
                if (not isinstance(backup_name, str) or "/" in backup_name or
                        not backup_name.startswith(".task-fabric-svn-task-backup-")):
                    raise ContainmentError("invalid_patch_recovery_journal")
            elif backup_name is not None:
                raise ContainmentError("invalid_patch_recovery_journal")

            opened_fd = None
            try:
                if retained_fd is not None:
                    if not Path(_fd_parent_path(retained_fd)).is_relative_to(self.source):
                        raise ContainmentError("patch_parent_escaped_checkout")
                    parent_fd = retained_fd
                else:
                    try:
                        parent_fd, _ = _open_parent_at(source_fd, relative, allow_missing=True)
                    except ContainmentError:
                        if existed:
                            raise
                        parent_fd = None
                    opened_fd = parent_fd
                name = Path(relative).name
                current = _signature_at(parent_fd, name) if parent_fd is not None else None
                if existed:
                    backup_signature = _signature_at(source_fd, backup_name)
                    if backup_signature is not None:
                        backup_info = os.stat(backup_name, dir_fd=source_fd, follow_symlinks=False)
                        if (not stat.S_ISREG(backup_info.st_mode) or backup_info.st_uid != os.getuid() or
                                backup_signature != old):
                            raise ContainmentError("invalid_patch_recovery_backup")
                        if current not in (None, old, new):
                            raise ContainmentError("patch_recovery_conflict")
                        if parent_fd is None:
                            raise ContainmentError("patch_recovery_parent_missing")
                        if retained_fd is None:
                            _verify_fd_parent(parent_fd, self.source, relative)
                        os.replace(backup_name, name, src_dir_fd=source_fd, dst_dir_fd=parent_fd)
                        _fsync_directory_fd(parent_fd)
                    elif current != old:
                        raise ContainmentError("patch_recovery_backup_missing")
                elif current is not None:
                    if current != new:
                        raise ContainmentError("patch_recovery_conflict")
                    if parent_fd is None:
                        raise ContainmentError("patch_recovery_parent_missing")
                    if retained_fd is None:
                        _verify_fd_parent(parent_fd, self.source, relative)
                    os.unlink(name, dir_fd=parent_fd)
                    _fsync_directory_fd(parent_fd)
            except (OSError, ContainmentError) as recovery_error:
                recovery_errors.append(recovery_error)
            finally:
                if opened_fd is not None:
                    os.close(opened_fd)

        if recovery_errors:
            raise ContainmentError("patch_rollback_incomplete; recovery journal retained") from recovery_errors[0]
        directories = receipt.get("created_dirs", [])
        if not isinstance(directories, list) or any(not isinstance(p, str) for p in directories):
            raise ContainmentError("invalid_patch_recovery_journal")
        # Empty directories are harmless; avoid path-based cleanup after a rename.
        receipt["state"] = "recovered"
        _atomic_json(receipt_path, receipt)

    def _cleanup_apply_backups(self, receipt, source_fd):
        for operation in receipt.get("operations", []):
            backup_name = operation.get("backup")
            relative = operation.get("path")
            if not backup_name or not isinstance(relative, str):
                continue
            _checked_relative(self.source, relative, allow_missing=True)
            if "/" in backup_name or not backup_name.startswith(".task-fabric-svn-task-backup-"):
                raise ContainmentError("invalid_patch_recovery_journal")
            try:
                info = os.stat(backup_name, dir_fd=source_fd, follow_symlinks=False)
            except FileNotFoundError:
                continue
            if not stat.S_ISREG(info.st_mode) or info.st_uid != os.getuid():
                raise ContainmentError("invalid_patch_recovery_backup")
            os.unlink(backup_name, dir_fd=source_fd)
