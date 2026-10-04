"""Unit tests for task-fabric-svn-task-supervisor.py. No live ledger, no real workers,
no credentials. Everything OS-bound (leases, locks) runs in temp dirs with
injectable pid/boot-id functions."""

import importlib.util
import json
import os
import sys
import tempfile
import unittest
from pathlib import Path
from types import SimpleNamespace

SUPERVISOR_PATH = Path(__file__).resolve().parent.parent / "task-fabric-svn-task-supervisor.py"


def load_supervisor():
    spec = importlib.util.spec_from_file_location("task_fabric_svn_task_supervisor",
                                                  SUPERVISOR_PATH)
    module = importlib.util.module_from_spec(spec)
    # dataclasses resolve types via sys.modules[cls.__module__]; the module
    # must be registered before exec on Python 3.14+.
    sys.modules["task_fabric_svn_task_supervisor"] = module
    spec.loader.exec_module(module)
    return module


_mod = load_supervisor()

TaskLedger = _mod.TaskLedger
LedgerError = _mod.LedgerError
RepositoryRegistry = _mod.RepositoryRegistry
RegistryError = _mod.RegistryError
UnregisteredRepo = _mod.UnregisteredRepo
LeaseStore = _mod.LeaseStore
LeaseError = _mod.LeaseError
SupervisorAlreadyRunning = _mod.SupervisorAlreadyRunning
Supervisor = _mod.Supervisor
SupervisorConfig = _mod.SupervisorConfig
MAX_LAUNCH_ATTEMPTS = _mod.MAX_LAUNCH_ATTEMPTS


# ---------------------------------------------------------------------------
# Fakes
# ---------------------------------------------------------------------------

class FakeLedger(TaskLedger):
    def __init__(self, tasks):
        self.tasks = {t["task_id"]: dict(t) for t in tasks}
        self.fail_calls = []
        self.inspect_down = False

    def list_tasks(self):
        if self.inspect_down:
            raise LedgerError("ledger down")
        return [dict(t) for t in self.tasks.values()]

    def inspect(self, task_id):
        if self.inspect_down:
            raise LedgerError("ledger down")
        if task_id not in self.tasks:
            raise LedgerError(f"task not found: {task_id}")
        return dict(self.tasks[task_id])

    def fail(self, task_id, reason):
        self.fail_calls.append((task_id, reason))
        t = self.tasks[task_id]
        t["status"] = "failed"
        t["failure_reason"] = reason
        return {"ok": True}

    def note(self, task_id, text):
        pass

    def set_status(self, task_id, status):
        self.tasks[task_id]["status"] = status


class FakeProc:
    """Popen stand-in. Stays 'alive' until terminate()/kill() or exit_now()."""

    def __init__(self, pid=4242):
        self._pid = pid
        self._rc = None
        self.terminated = False
        self.killed = False

    @property
    def pid(self):
        return self._pid

    def poll(self):
        return self._rc

    def exit_now(self, rc):
        self._rc = rc

    def terminate(self):
        self.terminated = True
        self._rc = -15

    def kill(self):
        self.killed = True
        self._rc = -9

    def wait(self, timeout=None):
        return self._rc if self._rc is not None else 0


class FakeSpawn:
    def __init__(self):
        self.argv_calls = []
        self.procs = []

    def __call__(self, argv, log_path):
        self.argv_calls.append(list(argv))
        proc = FakeProc(pid=5000 + len(self.procs))
        self.procs.append(proc)
        # Wrap like ChildProcess does, minimally.
        return _FakeChild(argv[1], proc, log_path)


class _FakeChild:
    def __init__(self, task_id, proc, log_path):
        self.task_id = task_id
        self._proc = proc
        self.log_path = log_path

    @property
    def pid(self):
        return self._proc.pid

    def poll(self):
        return self._proc.poll()

    def terminate(self):
        self._proc.terminate()

    def kill(self):
        self._proc.kill()

    def wait(self, timeout=None):
        return self._proc.wait(timeout)


def make_task(task_id, status="queued", harness="opencode",
              repo_identity="task-fabric-svn", created_at=1, trace_id=None):
    return {"task_id": task_id, "status": status, "harness": harness,
            "target_repo_identity": repo_identity, "created_at": created_at,
            "trace_id": trace_id or f"trace-{task_id}"}


class FakeSystemctl:
    """systemctl --user stand-in: records calls, returns a fixed rc."""

    def __init__(self, rc=0):
        self.rc = rc
        self.calls = []

    def __call__(self, args, timeout):
        self.calls.append(list(args))
        return self.rc


class SupervisorTestBase(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.tmpdir = Path(self.tmp.name)
        self.state_dir = self.tmpdir / "state"
        self.state_dir.mkdir()
        self.repo_dir = self.tmpdir / "repo"
        self.repo_dir.mkdir()
        registry_toml = self.tmpdir / "task-repositories.toml"
        registry_toml.write_text(
            "version = 1\n"
            "[repositories.task-fabric-svn]\n"
            f'path = "{self.repo_dir}"\n'
        )
        self.registry = RepositoryRegistry(registry_toml)
        self.proxy_env = self.tmpdir / "task-proxy.env"
        self.proxy_env.write_text("TASK_FABRIC_SVN_PROXY_HMAC_SECRET=test-only\n")
        self.proxy_unit_dir = self.tmpdir / "systemd-user"
        self.proxy_unit_dir.mkdir()
        (self.proxy_unit_dir / "task-fabric-svn-task-inference-proxy@.service"
         ).write_text("[Unit]\nDescription=test proxy template\n")
        self.alive_pids = set()

    _USE_DEFAULT_HARNESS = object()

    def make_supervisor(self, tasks, spawn=None, grant_check=None,
                        supervisor_id="sup-A", proxy_ready_rc=True,
                        systemctl_rc=0, task_model="openrouter/qwen/qwen3.8-27b:free",
                        harness_fn=_USE_DEFAULT_HARNESS):
        ledger = FakeLedger(tasks)
        systemctl = FakeSystemctl(rc=systemctl_rc)
        ready_calls = []

        def fake_proxy_ready(manifest, timeout_s):
            ready_calls.append((manifest, timeout_s))
            return proxy_ready_rc

        cfg = SupervisorConfig(
            state_dir=self.state_dir,
            worker_path=self.tmpdir / "task-fabric-svn-task-worker.py",
            registry_path=self.tmpdir / "task-repositories.toml",
            manifest_dir=self.tmpdir / "manifests",
            socket_dir=self.tmpdir / "sockets",
            proxy_env_file=self.proxy_env,
            proxy_unit_dir=self.proxy_unit_dir,
            task_model=task_model,
            deadline=60,
            poll_interval=1,
            supervisor_id=supervisor_id,
        )
        leases = LeaseStore(
            self.state_dir, supervisor_id,
            pid_fn=lambda: 1111, boot_id_fn=lambda: "bootA",
            alive_fn=lambda pid: pid in self.alive_pids,
        )
        spawn = spawn or FakeSpawn()
        sup = Supervisor(cfg, ledger, self.registry, leases, spawn=spawn,
                         grant_check=grant_check or (lambda *a: (True, "test allow")),
                         pid_alive_fn=lambda pid: pid in self.alive_pids,
                         systemctl_fn=systemctl,
                         proxy_ready_fn=fake_proxy_ready,
                         harness_fn=(lambda: sys.executable)
                         if harness_fn is self._USE_DEFAULT_HARNESS
                         else harness_fn)
        sup._fakes = SimpleNamespace(systemctl=systemctl,
                                     ready_calls=ready_calls)
        return sup, ledger, spawn

    def acquire_lock(self, sup):
        sup.leases.acquire_instance_lock()


# ---------------------------------------------------------------------------
# Leases
# ---------------------------------------------------------------------------

class LeaseTests(SupervisorTestBase):
    def test_two_supervisors_one_task_lease_exclusivity(self):
        a = LeaseStore(self.state_dir, "sup-A", pid_fn=lambda: 1111,
                       boot_id_fn=lambda: "bootA",
                       alive_fn=lambda pid: pid in self.alive_pids)
        b = LeaseStore(self.state_dir, "sup-B", pid_fn=lambda: 2222,
                       boot_id_fn=lambda: "bootA",
                       alive_fn=lambda pid: pid in self.alive_pids)
        a.acquire_lease("task-abc")
        with self.assertRaises(LeaseError):
            b.acquire_lease("task-abc")

    def test_lease_release_then_reacquire(self):
        a = LeaseStore(self.state_dir, "sup-A", pid_fn=lambda: 1111,
                       boot_id_fn=lambda: "bootA",
                       alive_fn=lambda pid: pid in self.alive_pids)
        b = LeaseStore(self.state_dir, "sup-B", pid_fn=lambda: 2222,
                       boot_id_fn=lambda: "bootA",
                       alive_fn=lambda pid: pid in self.alive_pids)
        a.acquire_lease("task-abc")
        a.release_lease("task-abc")
        b.acquire_lease("task-abc")  # must not raise

    def test_stale_lease_with_dead_worker_is_reaped(self):
        a = LeaseStore(self.state_dir, "sup-A", pid_fn=lambda: 1111,
                       boot_id_fn=lambda: "bootA",
                       alive_fn=lambda pid: pid in self.alive_pids)
        a.acquire_lease("task-abc", worker_pid=9999)  # 9999 not in alive_pids
        adopted = a.reap_stale_leases()
        self.assertEqual(adopted, [])
        self.assertIsNone(a.read_lease("task-abc"))

    def test_lease_with_live_worker_is_adopted_not_reaped(self):
        self.alive_pids.add(7777)
        a = LeaseStore(self.state_dir, "sup-A", pid_fn=lambda: 1111,
                       boot_id_fn=lambda: "bootA",
                       alive_fn=lambda pid: pid in self.alive_pids)
        a.acquire_lease("task-abc", worker_pid=7777)
        adopted = a.reap_stale_leases()
        self.assertEqual(adopted, [("task-abc", 7777)])
        self.assertIsNotNone(a.read_lease("task-abc"))

    def test_instance_lock_second_supervisor_refused(self):
        a = LeaseStore(self.state_dir, "sup-A", pid_fn=lambda: 1111,
                       boot_id_fn=lambda: "bootA",
                       alive_fn=lambda pid: pid in self.alive_pids)
        b = LeaseStore(self.state_dir, "sup-B", pid_fn=lambda: 2222,
                       boot_id_fn=lambda: "bootA",
                       alive_fn=lambda pid: pid in self.alive_pids)
        self.alive_pids.add(1111)
        a.acquire_instance_lock()
        with self.assertRaises(SupervisorAlreadyRunning):
            b.acquire_instance_lock()

    def test_instance_lock_taken_over_when_holder_dead(self):
        a = LeaseStore(self.state_dir, "sup-A", pid_fn=lambda: 1111,
                       boot_id_fn=lambda: "bootA",
                       alive_fn=lambda pid: pid in self.alive_pids)
        b = LeaseStore(self.state_dir, "sup-B", pid_fn=lambda: 2222,
                       boot_id_fn=lambda: "bootA",
                       alive_fn=lambda pid: pid in self.alive_pids)
        a.acquire_instance_lock()
        # 1111 not in alive_pids -> dead holder; B must take over.
        holder = b.acquire_instance_lock()
        self.assertEqual(holder["supervisor_id"], "sup-B")


# ---------------------------------------------------------------------------
# Sweep / gates / launch
# ---------------------------------------------------------------------------

class SweepTests(SupervisorTestBase):
    def test_sweep_launches_oldest_queued_with_correct_argv(self):
        sup, ledger, spawn = self.make_supervisor([
            make_task("task-new", created_at=20),
            make_task("task-old", created_at=10),
        ])
        self.acquire_lock(sup)
        launched = sup.sweep()
        self.assertEqual(launched, "task-old")
        self.assertEqual(len(spawn.argv_calls), 1)
        argv = spawn.argv_calls[0]
        self.assertEqual(argv[0], str(self.tmpdir / "task-fabric-svn-task-worker.py"))
        self.assertEqual(argv[1], "task-old")
        self.assertNotIn("--resume", argv)
        self.assertIn("--containment-config", argv)
        manifest_arg = argv[argv.index("--containment-config") + 1]
        self.assertTrue(manifest_arg.endswith("task-old.json"))
        self.assertTrue(Path(manifest_arg).is_file())
        self.assertIn("--model", argv)
        model_arg = argv[argv.index("--model") + 1]
        self.assertEqual(model_arg, "openrouter/qwen/qwen3.8-27b:free")
        # The manifest contract identity matches the worker's --model.
        doc = json.loads(Path(manifest_arg).read_text())
        self.assertEqual(doc["contract"]["task_id"], "task-old")
        self.assertEqual(doc["contract"]["trace_id"], "trace-task-old")
        self.assertEqual(doc["contract"]["model"], model_arg)
        # Proxy instance was started and its handshake awaited.
        self.assertIn(["start", "task-fabric-svn-task-inference-proxy@task-old.service"],
                      sup._fakes.systemctl.calls)
        self.assertEqual(len(sup._fakes.ready_calls), 1)
        self.assertIn("--deadline", argv)
        self.assertIn("--poll-interval", argv)
        # Lease held while the worker runs.
        self.assertIsNotNone(sup.leases.read_lease("task-old"))

    def test_sweep_is_serial_second_task_waits(self):
        sup, ledger, spawn = self.make_supervisor([
            make_task("task-a", created_at=10),
            make_task("task-b", created_at=20),
        ])
        self.acquire_lock(sup)
        sup.sweep()
        self.assertIsNone(sup.sweep())  # a worker is already running
        self.assertEqual(len(spawn.argv_calls), 1)

    def test_refuse_unregistered_repo(self):
        sup, ledger, spawn = self.make_supervisor([
            make_task("task-x", repo_identity="no-such-repo"),
        ])
        self.acquire_lock(sup)
        self.assertIsNone(sup.sweep())
        self.assertEqual(len(spawn.argv_calls), 0)
        self.assertEqual(len(ledger.fail_calls), 1)
        tid, reason = ledger.fail_calls[0]
        self.assertEqual(tid, "task-x")
        self.assertIn("unregistered_repo", reason)
        self.assertEqual(ledger.inspect("task-x")["status"], "failed")

    def test_refuse_unsupported_harness(self):
        sup, ledger, spawn = self.make_supervisor([
            make_task("task-x", harness="codex"),
        ])
        self.acquire_lock(sup)
        self.assertIsNone(sup.sweep())
        self.assertEqual(len(spawn.argv_calls), 0)
        tid, reason = ledger.fail_calls[0]
        self.assertIn("unsupported_harness", reason)

    def test_refuse_grant_denied(self):
        sup, ledger, spawn = self.make_supervisor(
            [make_task("task-x")],
            grant_check=lambda *a: (False, "grant_denied: test hook says no"),
        )
        self.acquire_lock(sup)
        self.assertIsNone(sup.sweep())
        self.assertEqual(len(spawn.argv_calls), 0)
        tid, reason = ledger.fail_calls[0]
        self.assertIn("grant_denied", reason)

    def test_refuse_without_proxy_template_leaves_task_queued(self):
        sup, ledger, spawn = self.make_supervisor([make_task("task-x")])
        (self.proxy_unit_dir / "task-fabric-svn-task-inference-proxy@.service").unlink()
        self.acquire_lock(sup)
        self.assertIsNone(sup.sweep())
        self.assertEqual(len(spawn.argv_calls), 0)
        self.assertEqual(len(ledger.fail_calls), 0)
        self.assertEqual(ledger.inspect("task-x")["status"], "queued")

    def test_missing_proxy_env_file_leaves_task_queued(self):
        sup, ledger, spawn = self.make_supervisor([make_task("task-x")])
        sup.cfg.proxy_env_file = self.tmpdir / "does-not-exist.env"
        self.acquire_lock(sup)
        self.assertIsNone(sup.sweep())
        self.assertEqual(len(spawn.argv_calls), 0)
        self.assertEqual(ledger.inspect("task-x")["status"], "queued")

    def test_missing_trace_id_fails_task_honestly(self):
        task = make_task("task-x")
        del task["trace_id"]
        sup, ledger, spawn = self.make_supervisor([task])
        self.acquire_lock(sup)
        self.assertIsNone(sup.sweep())
        self.assertEqual(len(spawn.argv_calls), 0)
        self.assertEqual(len(sup._fakes.systemctl.calls), 0)
        self.assertEqual(len(ledger.fail_calls), 1)
        tid, reason = ledger.fail_calls[0]
        self.assertEqual(tid, "task-x")
        self.assertIn("missing_trace_id", reason)
        self.assertIsNone(sup.leases.read_lease("task-x"))

    def test_proxy_start_failure_leaves_task_queued(self):
        sup, ledger, spawn = self.make_supervisor(
            [make_task("task-x")], systemctl_rc=1)
        self.acquire_lock(sup)
        self.assertIsNone(sup.sweep())
        self.assertEqual(len(spawn.argv_calls), 0)
        self.assertEqual(len(ledger.fail_calls), 0)
        self.assertEqual(ledger.inspect("task-x")["status"], "queued")
        self.assertIsNone(sup.leases.read_lease("task-x"))

    def test_proxy_not_ready_leaves_task_queued_and_stops_proxy(self):
        sup, ledger, spawn = self.make_supervisor(
            [make_task("task-x")], proxy_ready_rc=False)
        self.acquire_lock(sup)
        self.assertIsNone(sup.sweep())
        self.assertEqual(len(spawn.argv_calls), 0)
        self.assertEqual(ledger.inspect("task-x")["status"], "queued")
        calls = sup._fakes.systemctl.calls
        self.assertIn(["start", "task-fabric-svn-task-inference-proxy@task-x.service"],
                      calls)
        self.assertIn(["stop", "task-fabric-svn-task-inference-proxy@task-x.service"],
                      calls)

    def test_no_launch_when_task_no_longer_queued(self):
        sup, ledger, spawn = self.make_supervisor([make_task("task-x")])
        self.acquire_lock(sup)
        ledger.set_status("task-x", "cancelled")
        self.assertIsNone(sup.sweep())
        self.assertEqual(len(spawn.argv_calls), 0)
        self.assertIsNone(sup.leases.read_lease("task-x"))


# ---------------------------------------------------------------------------
# Observation: exit codes never imply success; cancellation; crashes.
# ---------------------------------------------------------------------------

class ObserveTests(SupervisorTestBase):
    def _launch(self, sup, ledger, spawn, task_id="task-x", status="queued"):
        launched = sup.sweep()
        self.assertEqual(launched, task_id)
        ledger.set_status(task_id, "working")
        return spawn.procs[0]

    def test_exit_zero_nonterminal_relaunches_with_resume(self):
        sup, ledger, spawn = self.make_supervisor([make_task("task-x")])
        self.acquire_lock(sup)
        proc = self._launch(sup, ledger, spawn)
        proc.exit_now(0)  # clean exit, but the ledger says still working
        sup.observe()
        self.assertEqual(len(spawn.argv_calls), 2)
        self.assertIn("--resume", spawn.argv_calls[1])
        self.assertEqual(ledger.inspect("task-x")["status"], "working")

    def test_terminal_state_on_exit_finalizes(self):
        sup, ledger, spawn = self.make_supervisor([make_task("task-x")])
        self.acquire_lock(sup)
        proc = self._launch(sup, ledger, spawn)
        ledger.set_status("task-x", "completed")
        proc.exit_now(0)
        sup.observe()
        self.assertEqual(sup.running, {})
        self.assertIsNone(sup.leases.read_lease("task-x"))
        self.assertEqual(len(spawn.argv_calls), 1)  # no relaunch

    def test_worker_crash_relaunches_until_budget_then_gives_up(self):
        sup, ledger, spawn = self.make_supervisor([make_task("task-x")])
        self.acquire_lock(sup)
        self._launch(sup, ledger, spawn)
        for _ in range(MAX_LAUNCH_ATTEMPTS):
            proc = spawn.procs[-1]
            proc.exit_now(-9)  # SIGKILL: worker crashed
            sup.observe()
        # Budget spent: 3 launches, then the task is failed honestly.
        self.assertEqual(len(spawn.argv_calls), MAX_LAUNCH_ATTEMPTS)
        tid, reason = ledger.fail_calls[0]
        self.assertEqual(tid, "task-x")
        self.assertIn("supervisor_gave_up", reason)
        self.assertEqual(ledger.inspect("task-x")["status"], "failed")
        self.assertEqual(sup.running, {})

    def test_queued_worker_died_before_claim_relaunches_fresh(self):
        sup, ledger, spawn = self.make_supervisor([make_task("task-x")])
        self.acquire_lock(sup)
        proc = self._launch(sup, ledger, spawn)
        ledger.set_status("task-x", "queued")  # died before the claim landed
        proc.exit_now(1)
        sup.observe()
        self.assertEqual(len(spawn.argv_calls), 2)
        self.assertNotIn("--resume", spawn.argv_calls[1])

    def test_external_cancel_terminates_worker(self):
        sup, ledger, spawn = self.make_supervisor([make_task("task-x")])
        self.acquire_lock(sup)
        proc = self._launch(sup, ledger, spawn)
        ledger.set_status("task-x", "cancelled")  # human ran: task-fabric-svn-work task cancel
        for _ in range(4):  # ledger is polled every LEDGER_POLL_EVERY ticks
            sup.observe()
        self.assertTrue(proc.terminated)
        self.assertEqual(sup.running, {})
        self.assertIsNone(sup.leases.read_lease("task-x"))

    def test_worker_keeps_running_while_waiting_on_human(self):
        sup, ledger, spawn = self.make_supervisor([make_task("task-x")])
        self.acquire_lock(sup)
        proc = self._launch(sup, ledger, spawn)
        ledger.set_status("task-x", "input_required")
        for _ in range(6):
            sup.observe()
        self.assertFalse(proc.terminated)
        self.assertIn("task-x", sup.running)


# ---------------------------------------------------------------------------
# Proxy lifecycle: resume reuses the manifest/proxy; finalize tears down.
# ---------------------------------------------------------------------------

class ProxyLifecycleTests(SupervisorTestBase):
    def _manifest_arg(self, argv):
        return argv[argv.index("--containment-config") + 1]

    def test_resume_reuses_same_manifest_and_proxy(self):
        sup, ledger, spawn = self.make_supervisor([make_task("task-x")])
        self.acquire_lock(sup)
        launched = sup.sweep()
        self.assertEqual(launched, "task-x")
        ledger.set_status("task-x", "working")
        first_manifest = self._manifest_arg(spawn.argv_calls[0])
        spawn.procs[0].exit_now(0)  # clean exit, ledger still working
        sup.observe()
        self.assertEqual(len(spawn.argv_calls), 2)
        self.assertIn("--resume", spawn.argv_calls[1])
        # Same task -> same manifest path (idempotent reuse), same proxy unit.
        self.assertEqual(self._manifest_arg(spawn.argv_calls[1]), first_manifest)
        manifests = list((self.tmpdir / "manifests").glob("*.json"))
        self.assertEqual(len(manifests), 1)
        starts = [c for c in sup._fakes.systemctl.calls
                  if c == ["start", "task-fabric-svn-task-inference-proxy@task-x.service"]]
        self.assertEqual(len(starts), 2)  # start is idempotent on a live unit

    def test_finalize_stops_proxy_and_removes_manifest(self):
        sup, ledger, spawn = self.make_supervisor([make_task("task-x")])
        self.acquire_lock(sup)
        launched = sup.sweep()
        self.assertEqual(launched, "task-x")
        manifest_arg = self._manifest_arg(spawn.argv_calls[0])
        # Plant a non-socket file where the proxy socket would be: teardown
        # must not remove files it did not create.
        socket_path = self.tmpdir / "sockets" / "task-x.socket"
        socket_path.write_text("not a socket")
        ledger.set_status("task-x", "completed")
        spawn.procs[0].exit_now(0)
        sup.observe()
        self.assertIn(["stop", "task-fabric-svn-task-inference-proxy@task-x.service"],
                      sup._fakes.systemctl.calls)
        self.assertFalse(Path(manifest_arg).exists())
        self.assertTrue(socket_path.is_file())  # untouched: not a real socket

    def test_finalize_never_fails_the_task_on_teardown_error(self):
        sup, ledger, spawn = self.make_supervisor(
            [make_task("task-x")], systemctl_rc=1)
        # systemctl_rc=1 only affects start; patch stop to raise instead.
        def boom(args, timeout):
            if args[0] == "stop":
                raise OSError("systemctl exploded")
            return 0
        sup._systemctl_fn = boom
        self.acquire_lock(sup)
        launched = sup.sweep()
        self.assertEqual(launched, "task-x")
        ledger.set_status("task-x", "completed")
        spawn.procs[0].exit_now(0)
        sup.observe()  # must not raise
        self.assertEqual(ledger.inspect("task-x")["status"], "completed")
        self.assertEqual(len(ledger.fail_calls), 0)


# ---------------------------------------------------------------------------
# Reconciliation
# ---------------------------------------------------------------------------

class ReconcileTests(SupervisorTestBase):
    def test_reconcile_relaunches_orphaned_starting_with_resume(self):
        sup, ledger, spawn = self.make_supervisor([
            make_task("task-s", status="starting"),
        ])
        self.acquire_lock(sup)
        sup.reconcile_orphans()
        self.assertEqual(len(spawn.argv_calls), 1)
        self.assertIn("--resume", spawn.argv_calls[0])
        self.assertEqual(spawn.argv_calls[0][1], "task-s")

    def test_reconcile_relaunches_orphaned_input_required(self):
        sup, ledger, spawn = self.make_supervisor([
            make_task("task-q", status="input_required"),
            make_task("task-a", status="approval_required"),
        ])
        self.acquire_lock(sup)
        sup.reconcile_orphans()
        launched = [c[1] for c in spawn.argv_calls]
        self.assertEqual(sorted(launched), ["task-a", "task-q"])
        for argv in spawn.argv_calls:
            self.assertIn("--resume", argv)

    def test_reconcile_adopts_live_worker_instead_of_relaunch(self):
        self.alive_pids.add(7777)
        sup, ledger, spawn = self.make_supervisor([
            make_task("task-x", status="working"),
        ])
        self.acquire_lock(sup)
        # Simulate a lease left by the crashed supervisor with a live worker.
        old = LeaseStore(self.state_dir, "sup-OLD", pid_fn=lambda: 999,
                         boot_id_fn=lambda: "bootA",
                         alive_fn=lambda pid: pid in self.alive_pids)
        old.acquire_lease("task-x", worker_pid=7777)
        adopted = sup.leases.reap_stale_leases()
        self.assertEqual(adopted, [("task-x", 7777)])
        for task_id, worker_pid in adopted:
            sup.running[task_id] = sup._adopted_child(task_id, worker_pid)
        sup.reconcile_orphans()
        self.assertEqual(len(spawn.argv_calls), 0)  # adopted, never relaunched
        self.assertIn("task-x", sup.running)

    def test_reconcile_budget_exhausted_fails_task(self):
        sup, ledger, spawn = self.make_supervisor([
            make_task("task-x", status="working"),
        ])
        self.acquire_lock(sup)
        sup._load_attempts()["task-x"] = MAX_LAUNCH_ATTEMPTS
        sup._save_attempts()
        sup.reconcile_orphans()
        self.assertEqual(len(spawn.argv_calls), 0)
        tid, reason = ledger.fail_calls[0]
        self.assertIn("supervisor_gave_up", reason)

    def test_registry_rejects_unknown_identity(self):
        with self.assertRaises(UnregisteredRepo):
            self.registry.resolve("nope")

    def test_registry_rejects_missing_path(self):
        bad = self.tmpdir / "bad.toml"
        bad.write_text('version = 1\n[repositories.ghost]\npath = "/no/such/dir"\n')
        reg = RepositoryRegistry(bad)
        with self.assertRaises(RegistryError):
            reg.resolve("ghost")



class HarnessResolutionTests(SupervisorTestBase):
    def test_default_harness_resolves_to_a_real_executable(self):
        # Must never be the mise shim: the shim cannot exec inside the bwrap
        # sandbox (mise config trust fails under the sandbox HOME/env),
        # which aborted a live dispatch before any inference.
        path = _mod._default_harness()
        self.assertTrue(path, "no opencode executable resolved")
        self.assertTrue(Path(path).is_file(), f"not a file: {path}")
        self.assertTrue(os.access(path, os.X_OK), f"not executable: {path}")
        self.assertNotIn("/shims/", path,
                         f"resolved to a mise shim: {path}")

    def test_supervisor_falls_back_to_default_harness_resolution(self):
        sup, _, _ = self.make_supervisor([], harness_fn=None)
        self.assertIs(sup._harness_fn, _mod._default_harness)
        resolved = sup._harness_fn()
        self.assertTrue(resolved)
        self.assertNotIn("/shims/", resolved)


if __name__ == "__main__":
    unittest.main(verbosity=2)
