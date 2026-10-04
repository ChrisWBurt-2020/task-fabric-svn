"""Actual OS-boundary tests. No provider requests, credentials or live state."""
import importlib.util
import json
import os
from pathlib import Path
import shutil
import socket
import subprocess
import sys
import tempfile
import threading
import time
import unittest
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from task_containment import (ContainmentError, TaskContainment, load_config,
                              run_process, verify_proxy)
import task_containment as containment_module


class ProxyFixture:
    def __init__(self, path, contract):
        self.contract = contract
        self.requests = []
        self.server = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.server.bind(str(path))
        path.chmod(0o600)
        self.server.listen()
        self.server.settimeout(0.05)
        self.stopped = False
        self.thread = threading.Thread(target=self.serve, daemon=True)
        self.thread.start()

    def serve(self):
        while not self.stopped:
            try:
                client, _ = self.server.accept()
            except socket.timeout:
                continue
            with client:
                request = client.recv(8192)
                self.requests.append(request)
                body = json.dumps(self.contract).encode()
                client.sendall(b"HTTP/1.0 200 OK\r\nContent-Length: " + str(len(body)).encode() + b"\r\n\r\n" + body)

    def close(self):
        self.stopped = True
        self.thread.join(timeout=1)
        self.server.close()


class ContainmentTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.root = Path(self.tmp.name)
        self.source = self.root / "source"
        self.source.mkdir()
        subprocess.run(["git", "init", "-q", str(self.source)], check=True)
        (self.source / "hello.txt").write_text("original")
        (self.source / ".env").write_text("SENTINEL_SECRET=never-copy")
        self.contract = {"task_id": "task-fixture", "trace_id": "trace-fixture",
                         "model": "fixture/free", "consumer": "fixture-only-not-registered",
                         "destination": "http://127.0.0.1:20128/v1/chat/completions",
                         "max_request_bytes": 4096, "max_requests": 1}
        self.proxy = ProxyFixture(self.root / "proxy.sock", self.contract)
        self.config = {"schema": 1, "harness_executable": "/usr/bin/true",
                       "proxy_socket": str(self.root / "proxy.sock"), "contract": self.contract}
        self.config_path = self.root / "config.json"
        self.config_path.write_text(json.dumps(self.config))
        self.config_path.chmod(0o600)
        self.containments = []

    def tearDown(self):
        for boundary in self.containments:
            boundary.lock.close()
        self.proxy.close()
        self.tmp.cleanup()

    def boundary(self):
        boundary = TaskContainment(self.source, self.root / "state", "task-fixture", self.config)
        self.containments.append(boundary)
        return boundary

    def test_explicit_proxy_contract_required(self):
        with self.assertRaises(ContainmentError):
            load_config(None, "task-fixture", "trace-fixture", "fixture/free")
        self.assertEqual(load_config(self.config_path, "task-fixture", "trace-fixture", "fixture/free"), self.config)
        with self.assertRaises(ContainmentError):
            load_config(self.config_path, "task-other", "trace-fixture", "fixture/free")
        with self.assertRaises(ContainmentError):
            load_config(self.config_path, "task-fixture", "trace-fixture", "paid/model")
        self.proxy.contract = {**self.contract, "consumer": "wrong"}
        with self.assertRaises(ContainmentError):
            verify_proxy(self.config)
        self.assertTrue(all(request.startswith(b"GET /_task-fabric-svn/contract ") for request in self.proxy.requests))

    def test_untrusted_manifest_or_missing_proxy_refused(self):
        self.config_path.chmod(0o666)
        with self.assertRaises(ContainmentError):
            load_config(self.config_path, "task-fixture", "trace-fixture", "fixture/free")
        self.config_path.chmod(0o600)
        self.config["proxy_socket"] = str(self.root / "missing")
        self.config_path.write_text(json.dumps(self.config))
        with self.assertRaises(ContainmentError):
            load_config(self.config_path, "task-fixture", "trace-fixture", "fixture/free")

    def test_no_source_secrets_gitdir_or_write_through(self):
        boundary = self.boundary()
        self.assertFalse((boundary.workspace / ".env").exists())
        self.assertFalse((boundary.workspace / ".git").exists())
        (boundary.workspace / "hello.txt").write_text("changed")
        manifest, changes = boundary.finish()
        self.assertEqual(changes, ["hello.txt"])
        self.assertFalse(json.loads(manifest.read_text())["applied"])
        self.assertEqual((self.source / "hello.txt").read_text(), "original")

    def test_source_and_result_symlink_escape_refused(self):
        (self.source / "escape").symlink_to(self.root / "config.json")
        with self.assertRaises(ContainmentError):
            self.boundary()
        (self.source / "escape").unlink()
        # Use a new task because a failed snapshot must never be reused silently.
        boundary = TaskContainment(self.source, self.root / "state", "task-result", self.config)
        self.containments.append(boundary)
        (boundary.workspace / "escape").symlink_to(self.root / "config.json")
        with self.assertRaises(ContainmentError):
            boundary.finish()

    def test_protected_outputs_rejected(self):
        boundary = self.boundary()
        (boundary.workspace / "opencode.json").write_text('{}')
        with self.assertRaises(ContainmentError):
            boundary.finish()

    def test_checked_patch_applies_only_unchanged_source_and_is_idempotent(self):
        (self.source / "remove.txt").write_text("remove me")
        boundary = self.boundary()
        (boundary.workspace / "hello.txt").write_text("reviewed result")
        (boundary.workspace / "new.txt").write_text("new file")
        (boundary.workspace / "remove.txt").unlink()
        boundary.finish()
        changes = boundary.apply_result()
        self.assertEqual(changes, ["hello.txt", "new.txt", "remove.txt"])
        self.assertEqual((self.source / "hello.txt").read_text(), "reviewed result")
        self.assertEqual((self.source / "new.txt").read_text(), "new file")
        self.assertFalse((self.source / "remove.txt").exists())
        self.assertEqual(boundary.apply_result(), changes)

    def test_checked_patch_rejects_any_source_conflict_without_partial_apply(self):
        boundary = self.boundary()
        (boundary.workspace / "hello.txt").write_text("worker result")
        boundary.finish()
        (self.source / "unrelated.txt").write_text("changed after snapshot")
        with self.assertRaisesRegex(ContainmentError, "patch_source_conflict"):
            boundary.apply_result()
        self.assertEqual((self.source / "hello.txt").read_text(), "original")

    def test_checked_patch_rolls_back_and_retries_after_replace_failure(self):
        boundary = self.boundary()
        (boundary.workspace / "hello.txt").write_text("worker result")
        (boundary.workspace / "new.txt").write_text("worker addition")
        boundary.finish()
        original_replace = containment_module.os.replace

        def fail_second_file(source, destination, **kwargs):
            if str(destination) == "new.txt":
                raise OSError("injected replace failure")
            return original_replace(source, destination, **kwargs)

        with mock.patch.object(containment_module.os, "replace", side_effect=fail_second_file):
            with self.assertRaisesRegex(OSError, "injected replace failure"):
                boundary.apply_result()
        self.assertEqual((self.source / "hello.txt").read_text(), "original")
        self.assertFalse((self.source / "new.txt").exists())
        self.assertEqual(boundary.apply_result(), ["hello.txt", "new.txt"])
        self.assertEqual((self.source / "hello.txt").read_text(), "worker result")
        self.assertEqual((self.source / "new.txt").read_text(), "worker addition")

    def test_checked_patch_rolls_back_and_retries_after_directory_fsync_failure(self):
        boundary = self.boundary()
        (boundary.workspace / "hello.txt").write_text("worker result")
        (boundary.workspace / "new.txt").write_text("worker addition")
        boundary.finish()
        original_fsync_directory = containment_module._fsync_directory_fd
        source_directory_syncs = 0

        def fail_after_first_replace(directory_fd):
            nonlocal source_directory_syncs
            if Path(os.readlink(f"/proc/self/fd/{directory_fd}")) == self.source:
                source_directory_syncs += 1
                if source_directory_syncs == 3:
                    raise OSError("injected directory fsync failure")
            return original_fsync_directory(directory_fd)

        with mock.patch.object(containment_module, "_fsync_directory_fd",
                               side_effect=fail_after_first_replace):
            with self.assertRaisesRegex(OSError, "injected directory fsync failure"):
                boundary.apply_result()
        self.assertEqual((self.source / "hello.txt").read_text(), "original")
        self.assertFalse((self.source / "new.txt").exists())
        self.assertEqual(boundary.apply_result(), ["hello.txt", "new.txt"])

    def test_staged_files_are_fsynced_after_final_mode_and_failure_precedes_mutation(self):
        boundary = self.boundary()
        (boundary.workspace / "hello.txt").write_text("worker result")
        (boundary.workspace / "hello.txt").chmod(0o755)
        boundary.finish()
        original_fsync_file = containment_module._fsync_file_at
        observed_modes = []

        def fail_after_mode_is_set(directory_fd, name):
            mode = os.stat(name, dir_fd=directory_fd, follow_symlinks=False).st_mode & 0o777
            observed_modes.append(mode)
            if mode == 0o755:
                raise OSError("injected post-chmod file fsync failure")
            return original_fsync_file(directory_fd, name)

        with mock.patch.object(containment_module, "_fsync_file_at",
                               side_effect=fail_after_mode_is_set):
            with self.assertRaisesRegex(OSError, "post-chmod file fsync failure"):
                boundary.apply_result()
        self.assertEqual(observed_modes, [0o644, 0o755])
        self.assertEqual((self.source / "hello.txt").read_text(), "original")
        self.assertFalse((boundary.root / "apply-receipt.json").exists())
        self.assertFalse(list(self.source.glob(".task-fabric-svn-task-backup-*")))
        self.assertFalse(list(self.source.glob(".task-fabric-svn-task-patch-*")))
        self.assertEqual(boundary.apply_result(), ["hello.txt"])
        self.assertEqual((self.source / "hello.txt").stat().st_mode & 0o777, 0o755)

    def test_interrupted_patch_recovers_from_durable_journal_before_retry(self):
        boundary = self.boundary()
        (boundary.workspace / "hello.txt").write_text("worker result")
        (boundary.workspace / "new.txt").write_text("worker addition")
        boundary.finish()
        original_replace = containment_module.os.replace
        interrupted = False

        def interrupt_after_first_replace(source, destination, **kwargs):
            nonlocal interrupted
            result = original_replace(source, destination, **kwargs)
            if str(destination) == "hello.txt" and not interrupted:
                interrupted = True
                raise KeyboardInterrupt("simulated process interruption")
            return result

        with mock.patch.object(containment_module.os, "replace", side_effect=interrupt_after_first_replace):
            with self.assertRaisesRegex(KeyboardInterrupt, "simulated process interruption"):
                boundary.apply_result()
        # The interrupted process left the first replacement visible. The
        # next caller recovers it before retrying; a second injected failure
        # proves that recovery leaves the whole checkout at baseline.
        self.assertEqual((self.source / "hello.txt").read_text(), "worker result")
        self.assertFalse((self.source / "new.txt").exists())
        original_replace = containment_module.os.replace

        def fail_retry_addition(source, destination, **kwargs):
            if str(destination) == "new.txt":
                raise OSError("injected retry failure")
            return original_replace(source, destination, **kwargs)

        with mock.patch.object(containment_module.os, "replace", side_effect=fail_retry_addition):
            with self.assertRaisesRegex(OSError, "injected retry failure"):
                boundary.apply_result()
        self.assertEqual((self.source / "hello.txt").read_text(), "original")
        self.assertFalse((self.source / "new.txt").exists())
        self.assertEqual(boundary.apply_result(), ["hello.txt", "new.txt"])
        self.assertEqual((self.source / "hello.txt").read_text(), "worker result")
        self.assertEqual((self.source / "new.txt").read_text(), "worker addition")

    def test_failed_restore_keeps_journal_and_recovery_continues_on_retry(self):
        (self.source / "aaa.txt").write_text("original first file")
        boundary = self.boundary()
        (boundary.workspace / "aaa.txt").write_text("worker first file")
        (boundary.workspace / "hello.txt").write_text("worker result")
        (boundary.workspace / "new.txt").write_text("worker addition")
        boundary.finish()
        original_replace = containment_module.os.replace
        failed = set()

        def fail_apply_and_restore_once(source, destination, **kwargs):
            source_path = Path(source)
            destination_name = str(destination)
            if destination_name == "new.txt" and "apply" not in failed:
                failed.add("apply")
                raise OSError("injected apply failure")
            if (destination_name == "hello.txt" and
                    source_path.name.startswith(".task-fabric-svn-task-backup-") and "restore" not in failed):
                failed.add("restore")
                raise OSError("injected restore failure")
            return original_replace(source, destination, **kwargs)

        with mock.patch.object(containment_module.os, "replace", side_effect=fail_apply_and_restore_once):
            with self.assertRaisesRegex(ContainmentError, "patch_rollback_incomplete"):
                boundary.apply_result()
        # Recovery continued restoring aaa.txt after hello.txt restoration
        # failed, and retained the journal and backup for retry.
        self.assertEqual((self.source / "aaa.txt").read_text(), "original first file")
        self.assertEqual((self.source / "hello.txt").read_text(), "worker result")
        self.assertFalse((self.source / "new.txt").exists())
        self.assertTrue((boundary.root / "apply-receipt.json").exists())
        self.assertEqual(boundary.apply_result(), ["aaa.txt", "hello.txt", "new.txt"])
        self.assertEqual((self.source / "aaa.txt").read_text(), "worker first file")
        self.assertEqual((self.source / "hello.txt").read_text(), "worker result")
        self.assertEqual((self.source / "new.txt").read_text(), "worker addition")

    def test_checked_patch_rejects_traversal_protected_paths_and_symlink_escape(self):
        for task_id, relative in (("task-traversal", "../outside"),
                                  ("task-protected", ".ssh/authorized_keys")):
            boundary = TaskContainment(self.source, self.root / "state", task_id, self.config)
            self.containments.append(boundary)
            (boundary.workspace / "hello.txt").write_text("changed")
            manifest, changes = boundary.finish()
            data = json.loads(manifest.read_text())
            data["files"][relative] = {"sha256": "0" * 64, "mode": 0o644}
            data["changes"] = sorted(set(changes + [relative]))
            manifest.write_text(json.dumps(data))
            with self.assertRaises(ContainmentError):
                boundary.apply_result()
            self.assertEqual((self.source / "hello.txt").read_text(), "original")

        boundary = TaskContainment(self.source, self.root / "state", "task-link", self.config)
        self.containments.append(boundary)
        (boundary.workspace / "hello.txt").write_text("changed")
        (boundary.workspace / "link").symlink_to(self.root)
        with self.assertRaisesRegex(ContainmentError, "unsupported_snapshot_entry"):
            boundary.finish()
        self.assertEqual((self.source / "hello.txt").read_text(), "original")

    def test_checked_patch_rejects_replaced_source_directory(self):
        boundary = self.boundary()
        (boundary.workspace / "hello.txt").write_text("worker result")
        boundary.finish()
        moved = self.root / "old-source"
        self.source.rename(moved)
        self.source.mkdir()
        try:
            with self.assertRaisesRegex(ContainmentError, "patch_source_identity_mismatch"):
                boundary.apply_result()
        finally:
            self.source.rmdir()
            moved.rename(self.source)

    def test_parent_symlink_swap_after_open_cannot_redirect_patch_outside(self):
        nested = self.source / "nested"
        nested.mkdir()
        (nested / "file.txt").write_text("original nested file")
        boundary = self.boundary()
        (boundary.workspace / "nested" / "file.txt").write_text("worker nested result")
        boundary.finish()
        outside = self.root / "outside"
        outside.mkdir()
        (outside / "file.txt").write_text("outside sentinel")
        moved = self.source / "nested-held"
        original_open_parent = containment_module._open_parent_at
        swapped = False

        def swap_parent_after_open(root_fd, relative, *, create=False, allow_missing=False):
            nonlocal swapped
            result = original_open_parent(root_fd, relative, create=create,
                                          allow_missing=allow_missing)
            if create and relative == "nested/file.txt" and not swapped:
                nested.rename(moved)
                nested.symlink_to(outside, target_is_directory=True)
                swapped = True
            return result

        try:
            with mock.patch.object(containment_module, "_open_parent_at",
                                   side_effect=swap_parent_after_open):
                with self.assertRaisesRegex(ContainmentError, "patch_parent_changed"):
                    boundary.apply_result()
            self.assertEqual((outside / "file.txt").read_text(), "outside sentinel")
            self.assertEqual((moved / "file.txt").read_text(), "original nested file")
        finally:
            if nested.is_symlink():
                nested.unlink()
            if moved.exists():
                moved.rename(nested)

    def test_inference_bind_mounts_are_present_in_sandbox(self):
        # A live dispatch died with rc=1 before any inference because bwrap
        # silently skips a bind whose destination parent dir is missing.
        # Every bind destination must be materialized in a real sandbox.
        boundary = self.boundary()
        self.require_bwrap(boundary)
        script = ("import os, sys; "
                  "want = [\"/opt/task_fabric_svn/opencode\", "
                  "\"/run/task_fabric_svn/inference.sock\", "
                  "\"/run/task_fabric_svn/opencode.json\"]; "
                  "missing = [p for p in want if not os.path.exists(p)]; "
                  "print(\"missing:\", missing); "
                  "sys.exit(1 if missing else 0)")
        rc, out, err = run_process(
            boundary.argv(["/usr/bin/python3", "-c", script], inference=True), 15)
        self.assertEqual(
            rc, 0,
            f"sandbox bind mounts missing (out={out[:200]} err={err[:200]})")

    def require_bwrap(self, boundary):
        if not shutil.which("bwrap"):
            self.skipTest("bubblewrap missing: physical containment NOT QUALIFIED")
        rc, _, err = run_process(boundary.argv(["/bin/true"], inference=False), 5)
        if rc:
            self.fail("bubblewrap namespace unavailable; containment must remain disabled: " + err[:200])

    def test_real_malicious_process_cannot_reach_host(self):
        boundary = self.boundary()
        self.require_bwrap(boundary)
        sentinel = self.root / "host-secret"
        sentinel.write_text("must-not-read-or-write")
        with socket.socket() as host:
            host.bind(("127.0.0.1", 0))
            host.listen()
            script = '''import os, pathlib, socket, json
results = {}
p = pathlib.Path(%r)
for action in ("read", "write"):
    try:
        p.read_text() if action == "read" else p.write_text("hacked")
        results[action] = False
    except OSError:
        results[action] = True
results["secret_env"] = "TASK_FABRIC_SVN_TEST_SECRET" not in os.environ
results["host_home"] = not pathlib.Path(%r).exists()
results["no_git"] = not pathlib.Path("/workspace/.git").exists()
for label, address in [("host_network", ("127.0.0.1", %d)), ("internet", ("1.1.1.1", 443))]:
    with socket.socket() as s:
        s.settimeout(.2)
        try:
            s.connect(address)
            results[label] = False
        except OSError:
            results[label] = True
try:
    pathlib.Path("/run/task_fabric_svn/opencode.json").write_text("{}")
    results["config_readonly"] = False
except OSError:
    results["config_readonly"] = True
pathlib.Path("/workspace/allowed.txt").write_text("allowed")
print(json.dumps(results))
''' % (str(sentinel), str(Path.home()), host.getsockname()[1])
            with mock.patch.dict(os.environ, {"TASK_FABRIC_SVN_TEST_SECRET": "must-not-inherit"}):
                rc, out, err = run_process(boundary.argv(["/usr/bin/python3", "-c", script], inference=False), 5)
        self.assertEqual(rc, 0, err)
        results = json.loads(out)
        self.assertTrue(all(results.values()), results)
        self.assertEqual(sentinel.read_text(), "must-not-read-or-write")
        self.assertTrue((boundary.workspace / "allowed.txt").exists())
        self.assertFalse((self.source / "allowed.txt").exists())

    def test_real_timeout_and_cancellation_kill_descendants(self):
        boundary = self.boundary()
        self.require_bwrap(boundary)
        script = 'import os,time,pathlib; pid=os.fork(); time.sleep(.7); pathlib.Path("/workspace/orphan").write_text(str(pid)); time.sleep(10)'
        rc, _, _ = run_process(boundary.argv(["/usr/bin/python3", "-c", script], inference=False), .15)
        self.assertEqual(rc, 124)
        time.sleep(.8)
        self.assertFalse((boundary.workspace / "orphan").exists())
        started = time.monotonic()
        rc, _, _ = run_process(boundary.argv(["/usr/bin/python3", "-c", script], inference=False), 5,
                               lambda: time.monotonic() - started > .15)
        self.assertEqual(rc, 125)
        time.sleep(.8)
        self.assertFalse((boundary.workspace / "orphan").exists())


if __name__ == "__main__":
    unittest.main()
