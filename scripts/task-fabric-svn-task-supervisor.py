#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.11"
# dependencies = []
# ///
"""Task Fabric SVN Task Supervisor -- automatic, boring, deterministic task dispatch.

Closes the hole in the center of the Task Fabric: Field can submit durable
tasks to the ledger and the contained worker (task-fabric-svn-task-worker.py) already
exists, but nothing automatically decides to run the worker when a task is
queued. This supervisor is that decision, made dumb on purpose.

What it does, in order, every sweep:
  1. Reconcile: on startup (and after every worker exit) look at the ledger,
     not at process state. Orphaned starting / working / input_required /
     approval_required tasks are relaunched with --resume (bounded by a
     per-task attempt budget) or adopted if their worker is still alive.
  2. Gate: for the oldest queued task, validate repo identity against the
     registered-repository registry (config/task-repositories.toml), check the
     harness allowlist (opencode only, Phase 1), and consult the grant-check
     hook point (Worker B's consumer plugs in here). Refusals fail the task
     in the ledger with an honest reason; they never launch a worker.
  3. Lease: acquire a per-task execution lease (atomic file creation) so two
     supervisors can never run the same task. A single-instance lock proves
     at most one supervisor runs on this machine.
  4. Launch: spawn the existing task-fabric-svn-task-worker.py (wrapped, never
     rewritten) as a child process.
  5. Observe: watch the child and the ledger. Worker exit never implies
     success -- only the ledger/task contract does. External cancellation
     (task-fabric-svn-work task cancel) terminates the worker within one poll interval.

Survival properties:
  - Supervisor crash / machine reboot: the single-instance lock is re-acquired
    on startup (stale locks from a dead boot are taken over), all task leases
    are reaped, and orphaned tasks are reconciled from the ledger.
  - Worker crash: the child exit is observed, the ledger is re-inspected, and
    the task is relaunched with --resume until the attempt budget is spent,
    then failed honestly as supervisor_gave_up.
  - No double-launch: single-instance lock + per-task lease + a fresh ledger
    inspect immediately before spawn.

The supervisor never touches a repository checkout. Repo mutation happens only
through the worker's existing checked-result application path.

Cancellation: `task-fabric-svn-work task cancel <id> --reason ...` (the existing ledger
contract). The supervisor sees the terminal state on its next ledger poll and
terminates the worker. SIGTERM to the supervisor terminates children
gracefully and releases leases.

Usage:
    task-fabric-svn-task-supervisor.py [--loop] [--once] [--reconcile-only]
        [--state-dir PATH] [--registry PATH] [--worker PATH]
        [--proxy-manifest-dir PATH] [--proxy-socket-dir PATH]
        [--proxy-env-file PATH] [--proxy-unit-template NAME]
        [--task-model MODEL] [--deadline S] [--poll-interval S]

Per-task dispatch wiring: at launch the supervisor generates a per-task
proxy manifest (task_proxy_manifest.py), starts
task-fabric-svn-task-inference-proxy@<task-id>.service, waits for the contract
handshake on the task's socket, then launches the worker with
--containment-config <manifest> --model <model>. On finalize the proxy
instance is stopped and the manifest/socket are removed.
"""

from __future__ import annotations

import argparse
import json
import os
import shutil
import signal
import stat
import subprocess
import sys
import time
import tomllib
import uuid
from dataclasses import dataclass, field
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from task_proxy_manifest import (
    DEFAULT_MODEL as DEFAULT_TASK_MODEL,
    ManifestError,
    generate_manifest,
    manifest_paths,
    proxy_ready,
)

# ---------------------------------------------------------------------------
# Constants: the boring, fixed policy of the supervisor.
# ---------------------------------------------------------------------------

QUEUED = "queued"
STARTING = "starting"
WORKING = "working"
INPUT_REQUIRED = "input_required"
APPROVAL_REQUIRED = "approval_required"
COMPLETED = "completed"
FAILED = "failed"
CANCELLED = "cancelled"

TERMINAL_STATES = frozenset({COMPLETED, FAILED, CANCELLED})
ORPHAN_STATES = frozenset({STARTING, WORKING, INPUT_REQUIRED, APPROVAL_REQUIRED})

SUPPORTED_HARNESSES = frozenset({"opencode"})  # Phase 1: opencode only
MAX_LAUNCH_ATTEMPTS = 3          # per task, across restarts, then fail honestly
OBSERVE_INTERVAL = 5.0           # seconds between child/ledger checks
LEDGER_POLL_EVERY = 3            # ledger re-inspect every N observe ticks
TERM_GRACE = 10.0                # seconds between SIGTERM and SIGKILL
GRANT_HOOK_TIMEOUT = 30          # seconds for the grant-check hook

INSTANCE_LOCK_NAME = "supervisor.lock"
LEASE_DIR_NAME = "task-leases"
ATTEMPTS_FILE_NAME = "supervisor-attempts.json"
WORKER_LOG_DIR_NAME = "worker-logs"
PROXY_MANIFEST_SUBDIR = "task-proxy/manifests"
PROXY_SOCKET_SUBDIR = "task-proxy/sockets"
PROXY_READY_TIMEOUT = 20.0       # seconds to wait for a task proxy's contract handshake
SYSTEMCTL_TIMEOUT = 30           # seconds for systemctl --user start/stop


def log(msg: str) -> None:
    print(f"{time.strftime('%Y-%m-%dT%H:%M:%SZ', time.gmtime())} [supervisor] {msg}",
          flush=True)


def boot_id() -> str:
    try:
        return Path("/proc/sys/kernel/random/boot_id").read_text().strip()
    except OSError:
        return "unknown"


def pid_alive(pid: int) -> bool:
    try:
        os.kill(pid, 0)
    except ProcessLookupError:
        return False
    except PermissionError:
        return True
    return True


# ---------------------------------------------------------------------------
# Ledger adapter. The supervisor only ever talks to the ledger, never to
# worker internals. A fake implementation backs the unit tests.
# ---------------------------------------------------------------------------

class LedgerError(Exception):
    pass


class TaskLedger:
    def list_tasks(self) -> list[dict]:
        raise NotImplementedError

    def inspect(self, task_id: str) -> dict:
        raise NotImplementedError

    def fail(self, task_id: str, reason: str) -> dict:
        raise NotImplementedError

    def note(self, task_id: str, text: str) -> None:
        raise NotImplementedError


class TaskFabricSvnWorkLedger(TaskLedger):
    """Ledger adapter over the `task-fabric-svn-work task ...` CLI."""

    def __init__(self, exe: str | None = None):
        self.exe = exe or shutil.which("task-fabric-svn-work")
        if not self.exe:
            raise LedgerError("task-fabric-svn-work not found on PATH")

    def _run(self, *args: str) -> dict:
        try:
            proc = subprocess.run(
                [self.exe, "task", *args],
                capture_output=True, text=True, timeout=60,
            )
        except (OSError, subprocess.SubprocessError) as e:
            raise LedgerError(f"task-fabric-svn-work invocation failed: {e}")
        if proc.returncode != 0:
            raise LedgerError(
                f"task-fabric-svn-work task {' '.join(args)} failed: {proc.stderr.strip()[:500]}")
        try:
            return json.loads(proc.stdout)
        except json.JSONDecodeError as e:
            raise LedgerError(f"task-fabric-svn-work printed non-JSON: {e}")

    def list_tasks(self) -> list[dict]:
        out = self._run("list")
        tasks = out.get("tasks")
        if not isinstance(tasks, list):
            raise LedgerError("task-fabric-svn-work task list returned no task list")
        return tasks

    def inspect(self, task_id: str) -> dict:
        out = self._run("inspect", task_id)
        task = out.get("task")
        if not isinstance(task, dict):
            raise LedgerError(f"task-fabric-svn-work task inspect returned no task for {task_id}")
        return task

    def fail(self, task_id: str, reason: str) -> dict:
        return self._run("fail", task_id, "--reason", reason[:2000])

    def note(self, task_id: str, text: str) -> None:
        try:
            self._run("note-progress", task_id, "--note", text[:2000])
        except LedgerError as e:
            # Progress notes must never kill the supervisor.
            log(f"warning: note-progress failed for {task_id}: {e}")


# ---------------------------------------------------------------------------
# Registered-repository registry. Only known repo identities may run.
# ---------------------------------------------------------------------------

class RegistryError(Exception):
    pass


class UnregisteredRepo(RegistryError):
    pass


class RepoPathInvalid(RegistryError):
    pass


class FingerprintMismatch(RegistryError):
    pass


@dataclass
class RepositoryEntry:
    identity: str
    path: Path
    fingerprint: str | None = None


class RepositoryRegistry:
    """Loads config/task-repositories.toml and validates identities.

    Format:
        version = 1
        [repositories.example-project]
        path = "/home/user/Work/example-project"
        fingerprint = "https://github.com/YOUR-USER/example-project.git"
    """

    def __init__(self, path: Path):
        self.path = path
        self.entries: dict[str, RepositoryEntry] = {}
        self._load()

    def _load(self) -> None:
        try:
            with open(self.path, "rb") as f:
                doc = tomllib.load(f)
        except FileNotFoundError:
            raise RegistryError(f"repository registry not found: {self.path}")
        except tomllib.TOMLDecodeError as e:
            raise RegistryError(f"repository registry is not valid TOML: {e}")
        repos = doc.get("repositories")
        if not isinstance(repos, dict):
            raise RegistryError("repository registry has no [repositories] table")
        for identity, raw in repos.items():
            if not isinstance(raw, dict) or not raw.get("path"):
                raise RegistryError(f"repository '{identity}' has no path")
            self.entries[identity] = RepositoryEntry(
                identity=identity,
                path=Path(raw["path"]).expanduser(),
                fingerprint=raw.get("fingerprint"),
            )

    def resolve(self, identity: str | None) -> RepositoryEntry:
        """Return the registered entry, verifying path and fingerprint.

        Raises UnregisteredRepo / RepoPathInvalid / FingerprintMismatch.
        """
        if not identity or identity not in self.entries:
            raise UnregisteredRepo(
                f"unregistered_repo: '{identity}' is not in {self.path}")
        entry = self.entries[identity]
        if not entry.path.is_absolute():
            raise RepoPathInvalid(
                f"repo_path_invalid: '{identity}' path is not absolute: {entry.path}")
        if not entry.path.is_dir():
            raise RepoPathInvalid(
                f"repo_path_invalid: '{identity}' path is not a directory: {entry.path}")
        if entry.fingerprint:
            actual = self._git_origin(entry.path)
            if actual != entry.fingerprint:
                raise FingerprintMismatch(
                    f"fingerprint_mismatch: '{identity}' origin {actual!r} "
                    f"does not match registered {entry.fingerprint!r}")
        return entry

    @staticmethod
    def _git_origin(repo_dir: Path) -> str | None:
        try:
            proc = subprocess.run(
                ["git", "-C", str(repo_dir), "remote", "get-url", "origin"],
                capture_output=True, text=True, timeout=15,
            )
        except (OSError, subprocess.SubprocessError):
            return None
        if proc.returncode != 0:
            return None
        return proc.stdout.strip() or None


# ---------------------------------------------------------------------------
# Grant-check hook point. Worker B's production proxy/grant path plugs in
# here. Phase 1 default (no hook configured): allow, and say so loudly.
# ---------------------------------------------------------------------------

def default_grant_check_factory(hook: str | None):
    def check(task_id: str, repo_identity: str, harness: str) -> tuple[bool, str]:
        if not hook:
            return True, "no grant hook configured (Phase 1 default: allow)"
        try:
            proc = subprocess.run(
                [hook, task_id, repo_identity or "", harness or ""],
                capture_output=True, text=True, timeout=GRANT_HOOK_TIMEOUT,
            )
        except (OSError, subprocess.SubprocessError) as e:
            return False, f"grant hook failed to run: {e}"
        except subprocess.TimeoutExpired:
            return False, "grant hook timed out"
        reason = (proc.stdout or proc.stderr or "").strip()[:500] or "grant hook denied"
        return (proc.returncode == 0,
                "grant hook allowed" if proc.returncode == 0 else f"grant_denied: {reason}")
    return check


# ---------------------------------------------------------------------------
# Leases. Single-instance lock (boot_id + pid) proves at most one supervisor
# per machine; per-task leases (atomic O_CREAT|O_EXCL) prove at most one
# worker per task. All files live under the state dir.
# ---------------------------------------------------------------------------

class LeaseError(Exception):
    pass


class SupervisorAlreadyRunning(LeaseError):
    pass


class ContainmentPrepError(Exception):
    """Per-task containment setup failed.

    permanent=True: the task itself is at fault (missing trace_id, manifest
    identity conflict) -- fail it honestly, it will never self-heal.
    permanent=False: the proxy machinery hiccupped (unit would not start,
    contract handshake timed out) -- leave the task queued for the next
    sweep, never fail it for an environmental problem.
    """

    def __init__(self, reason: str, permanent: bool = False):
        super().__init__(reason)
        self.reason = reason
        self.permanent = permanent


class LeaseStore:
    def __init__(self, state_dir: Path, supervisor_id: str,
                 pid_fn=os.getpid, boot_id_fn=boot_id, alive_fn=pid_alive):
        self.state_dir = state_dir
        self.supervisor_id = supervisor_id
        self._pid_fn = pid_fn
        self._boot_id_fn = boot_id_fn
        self._alive_fn = alive_fn
        self.lock_path = state_dir / INSTANCE_LOCK_NAME
        self.lease_dir = state_dir / LEASE_DIR_NAME
        self.lease_dir.mkdir(parents=True, exist_ok=True)

    # -- single-instance lock -------------------------------------------
    def acquire_instance_lock(self) -> dict:
        """Hold the machine-wide supervisor lock. Raises SupervisorAlreadyRunning."""
        me = {"supervisor_id": self.supervisor_id,
              "pid": self._pid_fn(),
              "boot_id": self._boot_id_fn(),
              "started_at": int(time.time())}
        while True:
            try:
                fd = os.open(self.lock_path, os.O_CREAT | os.O_EXCL | os.O_WRONLY, 0o600)
            except FileExistsError:
                holder = self._read_lock()
                if holder and holder.get("boot_id") == me["boot_id"] \
                        and self._alive_fn(int(holder.get("pid", -1))):
                    raise SupervisorAlreadyRunning(
                        f"supervisor {holder.get('supervisor_id')} "
                        f"(pid {holder.get('pid')}) already holds the instance lock")
                # Stale lock: dead pid or older boot. Take it over.
                try:
                    os.unlink(self.lock_path)
                except FileNotFoundError:
                    pass
                continue
            with os.fdopen(fd, "w") as f:
                json.dump(me, f)
            return me

    def _read_lock(self) -> dict | None:
        try:
            return json.loads(self.lock_path.read_text())
        except (OSError, json.JSONDecodeError):
            return None

    def release_instance_lock(self) -> None:
        holder = self._read_lock()
        if holder and holder.get("supervisor_id") == self.supervisor_id:
            try:
                os.unlink(self.lock_path)
            except FileNotFoundError:
                pass

    # -- per-task leases --------------------------------------------------
    def _lease_path(self, task_id: str) -> Path:
        safe = "".join(c for c in task_id if c.isalnum() or c in "-_")
        if not safe or safe != task_id:
            raise LeaseError(f"unsafe task id for lease file: {task_id!r}")
        return self.lease_dir / f"{safe}.lease"

    def acquire_lease(self, task_id: str, worker_pid: int | None = None) -> dict:
        """Atomically claim the task. Raises LeaseError if already held."""
        lease = {"supervisor_id": self.supervisor_id,
                 "pid": self._pid_fn(),
                 "boot_id": self._boot_id_fn(),
                 "task_id": task_id,
                 "worker_pid": worker_pid,
                 "acquired_at": int(time.time())}
        path = self._lease_path(task_id)
        try:
            fd = os.open(path, os.O_CREAT | os.O_EXCL | os.O_WRONLY, 0o600)
        except FileExistsError:
            holder = self.read_lease(task_id)
            raise LeaseError(
                f"task {task_id} already leased by "
                f"{(holder or {}).get('supervisor_id', 'unknown')}")
        with os.fdopen(fd, "w") as f:
            json.dump(lease, f)
        return lease

    def read_lease(self, task_id: str) -> dict | None:
        try:
            return json.loads(self._lease_path(task_id).read_text())
        except (OSError, json.JSONDecodeError):
            return None

    def set_worker_pid(self, task_id: str, worker_pid: int) -> None:
        lease = self.read_lease(task_id)
        if lease and lease.get("supervisor_id") == self.supervisor_id:
            lease["worker_pid"] = worker_pid
            self._lease_path(task_id).write_text(json.dumps(lease))

    def release_lease(self, task_id: str) -> None:
        lease = self.read_lease(task_id)
        if lease and lease.get("supervisor_id") == self.supervisor_id:
            try:
                self._lease_path(task_id).unlink()
            except FileNotFoundError:
                pass

    def reap_stale_leases(self) -> list[tuple[str, int | None]]:
        """Drop every lease whose worker is dead. Returns (task_id, worker_pid)
        for leases whose worker is still alive: those tasks are adopted, never
        re-launched, so a surviving worker is never duplicated."""
        adopted: list[tuple[str, int | None]] = []
        for path in sorted(self.lease_dir.glob("*.lease")):
            task_id = path.stem
            try:
                lease = json.loads(path.read_text())
            except (OSError, json.JSONDecodeError):
                try:
                    path.unlink()
                except OSError:
                    pass
                continue
            worker_pid = lease.get("worker_pid")
            alive = (
                isinstance(worker_pid, int)
                and lease.get("boot_id") == self._boot_id_fn()
                and self._alive_fn(worker_pid)
            )
            if alive:
                adopted.append((task_id, worker_pid))
            else:
                try:
                    path.unlink()
                except OSError:
                    pass
        return adopted


# ---------------------------------------------------------------------------
# Worker launcher. Wraps task-fabric-svn-task-worker.py; never rewrites it.
# ---------------------------------------------------------------------------

class ChildProcess:
    """Minimal interface the supervisor needs from a worker process."""

    def __init__(self, popen: subprocess.Popen, task_id: str, log_path: Path):
        self._popen = popen
        self.task_id = task_id
        self.log_path = log_path

    @property
    def pid(self) -> int | None:
        return self._popen.pid

    def poll(self) -> int | None:
        return self._popen.poll()

    def terminate(self) -> None:
        self._popen.terminate()

    def kill(self) -> None:
        self._popen.kill()

    def wait(self, timeout: float | None) -> int:
        return self._popen.wait(timeout=timeout)


def default_spawn(argv: list[str], log_path: Path) -> ChildProcess:
    log_path.parent.mkdir(parents=True, exist_ok=True)
    log_file = open(log_path, "ab")
    try:
        popen = subprocess.Popen(argv, stdout=log_file, stderr=subprocess.STDOUT,
                                 stdin=subprocess.DEVNULL)
    except Exception:
        log_file.close()
        raise
    # The child keeps the file handle; ours can close.
    log_file.close()
    return ChildProcess(popen, argv[1] if len(argv) > 1 else "?", log_path)


@dataclass
class SupervisorConfig:
    state_dir: Path
    worker_path: Path
    registry_path: Path
    manifest_dir: Path
    socket_dir: Path
    proxy_env_file: Path
    proxy_unit_dir: Path
    proxy_unit_template: str = "task-fabric-svn-task-inference-proxy@"
    task_model: str = DEFAULT_TASK_MODEL
    deadline: int = 43200
    poll_interval: int = 15
    grant_hook: str | None = None
    supervisor_id: str = field(default_factory=lambda: uuid.uuid4().hex[:12])


def _default_harness() -> str | None:
    """Resolve the opencode executable to its real binary, not the mise shim.

    The mise shim re-resolves mise config trust at exec time and fails
    inside the bwrap sandbox (fake HOME, cleared environment), which aborts
    every contained harness run before any inference. The real binary runs
    standalone, so prefer it; fall back to a plain PATH lookup.
    """
    mise = shutil.which("mise")
    if mise:
        try:
            proc = subprocess.run([mise, "which", "opencode"],
                                  capture_output=True, text=True, timeout=15)
        except (OSError, subprocess.SubprocessError):
            proc = None
        if proc is not None and proc.returncode == 0:
            for line in proc.stdout.splitlines():
                candidate = line.strip()
                if candidate and Path(candidate).is_file():
                    return candidate
    return shutil.which("opencode")


def _default_systemctl(args: list[str], timeout: float) -> int:
    """Run systemctl --user; return its exit code (127 if it could not run)."""
    try:
        proc = subprocess.run(["systemctl", "--user", *args],
                              capture_output=True, text=True, timeout=timeout)
    except (OSError, subprocess.SubprocessError) as e:
        log(f"systemctl --user {' '.join(args)} could not run: {e}")
        return 127
    if proc.returncode != 0:
        log(f"systemctl --user {' '.join(args)} rc={proc.returncode}: "
            f"{proc.stderr.strip()[:300]}")
    return proc.returncode


# ---------------------------------------------------------------------------
# The supervisor itself: reconcile, gate, lease, launch, observe.
# ---------------------------------------------------------------------------

class Supervisor:
    def __init__(self, cfg: SupervisorConfig, ledger: TaskLedger,
                 registry: RepositoryRegistry, leases: LeaseStore,
                 spawn=default_spawn,
                 grant_check=None,
                 pid_alive_fn=pid_alive,
                 systemctl_fn=None,
                 proxy_ready_fn=None,
                 harness_fn=None):
        self.cfg = cfg
        self.ledger = ledger
        self.registry = registry
        self.leases = leases
        self.spawn = spawn
        self.grant_check = grant_check or default_grant_check_factory(cfg.grant_hook)
        self._pid_alive = pid_alive_fn
        self._systemctl_fn = systemctl_fn or _default_systemctl
        self._proxy_ready_fn = proxy_ready_fn or proxy_ready
        self._harness_fn = harness_fn or _default_harness
        self.running: dict[str, ChildProcess] = {}
        self.pending_inspect: set[str] = set()  # child exited, ledger not yet readable
        self.last_rc: dict[str, int] = {}
        self.ledger_ticks: dict[str, int] = {}
        self.stopping = False
        self.attempts_path = cfg.state_dir / ATTEMPTS_FILE_NAME
        self._attempts: dict[str, int] | None = None

    # -- attempt budget ---------------------------------------------------
    def _load_attempts(self) -> dict[str, int]:
        if self._attempts is None:
            try:
                raw = json.loads(self.attempts_path.read_text())
                self._attempts = {k: int(v) for k, v in raw.items()
                                  if isinstance(v, int)}
            except (OSError, json.JSONDecodeError, ValueError, AttributeError):
                self._attempts = {}
        return self._attempts

    def _save_attempts(self) -> None:
        tmp = self.attempts_path.with_suffix(".tmp")
        tmp.write_text(json.dumps(self._load_attempts()))
        os.replace(tmp, self.attempts_path)

    def _record_attempt(self, task_id: str) -> int:
        attempts = self._load_attempts()
        attempts[task_id] = attempts.get(task_id, 0) + 1
        self._save_attempts()
        return attempts[task_id]

    def _clear_attempts(self, task_id: str) -> None:
        attempts = self._load_attempts()
        if task_id in attempts:
            del attempts[task_id]
            self._save_attempts()

    # -- startup ----------------------------------------------------------
    def startup(self) -> None:
        """Acquire the instance lock, reap leases, reconcile orphans."""
        self.leases.acquire_instance_lock()
        log(f"instance lock acquired (id={self.cfg.supervisor_id})")
        self._reconcile_proxy_instances()
        adopted = self.leases.reap_stale_leases()
        for task_id, worker_pid in adopted:
            log(f"adopting live worker pid={worker_pid} for orphaned task {task_id}")
            self.running[task_id] = self._adopted_child(task_id, worker_pid)
            self.ledger_ticks[task_id] = 0
        self.reconcile_orphans()

    def _adopted_child(self, task_id: str, worker_pid: int) -> ChildProcess:
        return _AdoptedChild(task_id, worker_pid, self._pid_alive)

    def reconcile_orphans(self) -> None:
        """Relaunch orphaned non-terminal tasks with --resume, bounded by budget."""
        try:
            tasks = self.ledger.list_tasks()
        except LedgerError as e:
            log(f"reconcile: ledger unavailable: {e}; will retry next sweep")
            return
        for task in tasks:
            task_id = task.get("task_id", "")
            status = task.get("status")
            if status in TERMINAL_STATES:
                self.leases.release_lease(task_id)
                self._clear_attempts(task_id)
                continue
            if status not in ORPHAN_STATES:
                continue
            if task_id in self.running:
                continue  # adopted already
            attempts = self._load_attempts().get(task_id, 0)
            if attempts >= MAX_LAUNCH_ATTEMPTS:
                self._fail_task(task_id,
                                f"supervisor_gave_up: worker could not progress "
                                f"task after {attempts} launches")
                continue
            log(f"reconcile: relaunching orphaned task {task_id} (status={status}) "
                f"with --resume (attempt {attempts + 1}/{MAX_LAUNCH_ATTEMPTS})")
            self._launch(task_id, resume=True, task=task)

    # -- per-task proxy lifecycle ------------------------------------------
    def proxy_unit_name(self, task_id: str) -> str:
        return f"{self.cfg.proxy_unit_template}{task_id}.service"

    def proxy_template_unit_path(self) -> Path:
        return self.cfg.proxy_unit_dir / f"{self.cfg.proxy_unit_template}.service"

    def containment_ready(self) -> bool:
        """Supervisor-level precondition: the per-task proxy machinery must
        exist before ANY task launches. Fail-closed, never per-task."""
        if not self._harness_fn():
            log("refusing launches: harness executable 'opencode' not found "
                "on PATH")
            return False
        if not self.cfg.proxy_env_file.is_file():
            log(f"refusing launches: proxy env file not found: "
                f"{self.cfg.proxy_env_file}")
            return False
        template = self.proxy_template_unit_path()
        if not template.is_file():
            log(f"refusing launches: proxy template unit not installed: "
                f"{template}")
            return False
        if not self.cfg.task_model:
            log("refusing launches: no --task-model configured")
            return False
        return True

    def _prepare_containment(self, task_id: str, task: dict
                             ) -> tuple[Path, str]:
        """Generate the task's manifest and bring up its proxy instance.

        Returns (manifest_path, model). Raises ContainmentPrepError.
        """
        trace_id = task.get("trace_id")
        if not trace_id or not isinstance(trace_id, str):
            raise ContainmentPrepError(
                f"missing_trace_id: task {task_id} carries no trace_id; the "
                "containment contract requires per-task identity binding",
                permanent=True)
        model = self.cfg.task_model
        if not model:
            raise ContainmentPrepError("no task model configured",
                                       permanent=True)
        harness = self._harness_fn()
        if not harness:
            raise ContainmentPrepError(
                "harness executable 'opencode' not found on PATH",
                permanent=True)
        try:
            manifest = generate_manifest(
                task_id, trace_id, model,
                manifest_dir=self.cfg.manifest_dir,
                socket_dir=self.cfg.socket_dir,
                harness_executable=harness)
        except ManifestError as e:
            raise ContainmentPrepError(f"manifest_unavailable: {e}",
                                       permanent=True) from e
        unit = self.proxy_unit_name(task_id)
        rc = self._systemctl_fn(["start", unit], SYSTEMCTL_TIMEOUT)
        if rc != 0:
            raise ContainmentPrepError(
                f"proxy_start_failed: systemctl start {unit} rc={rc}")
        if not self._proxy_ready_fn(manifest, timeout_s=PROXY_READY_TIMEOUT):
            self._systemctl_fn(["stop", unit], SYSTEMCTL_TIMEOUT)
            raise ContainmentPrepError(
                f"proxy_not_ready: {unit} did not serve the task contract "
                f"within {PROXY_READY_TIMEOUT:.0f}s")
        log(f"task {task_id}: containment ready (manifest={manifest}, "
            f"proxy={unit}, model={model})")
        return manifest, model

    def _teardown_containment(self, task_id: str) -> None:
        """Stop the task's proxy instance and remove its manifest and socket.
        Only removes what the supervisor/proxy created: an owner regular
        file at the manifest path, an owner socket at the socket path.
        Best-effort: never raises, never fails a task."""
        unit = self.proxy_unit_name(task_id)
        try:
            self._systemctl_fn(["stop", unit], SYSTEMCTL_TIMEOUT)
        except Exception as e:
            log(f"task {task_id}: proxy stop failed (ignored): {e}")
        try:
            manifest, sock = manifest_paths(task_id, self.cfg.manifest_dir,
                                            self.cfg.socket_dir)
        except ManifestError:
            return
        for path, want in ((manifest, stat.S_ISREG), (sock, stat.S_ISSOCK)):
            try:
                st = path.lstat()
            except OSError:
                continue
            if stat.S_ISLNK(st.st_mode) or st.st_uid != os.getuid():
                continue
            if not want(st.st_mode):
                continue
            try:
                path.unlink()
            except OSError:
                pass
        log(f"task {task_id}: proxy {unit} stopped; manifest/socket removed")

    def _reconcile_proxy_instances(self) -> None:
        """After a crash or reboot, proxy instances may outlive the
        supervisor. Stop the ones whose tasks are terminal or unknown; keep
        the ones backing a still-live task (a worker may be adopted with its
        proxy intact)."""
        template = self.cfg.proxy_unit_template
        try:
            proc = subprocess.run(
                ["systemctl", "--user", "list-units", "--type=service",
                 "--state=running", "--no-legend", "--plain", f"{template}*"],
                capture_output=True, text=True, timeout=SYSTEMCTL_TIMEOUT)
        except (OSError, subprocess.SubprocessError) as e:
            log(f"reconcile: could not list proxy instances: {e}")
            return
        if proc.returncode != 0:
            return
        for line in proc.stdout.splitlines():
            parts = line.split()
            if not parts:
                continue
            unit = parts[0]
            if not unit.startswith(template) or not unit.endswith(".service"):
                continue
            task_id = unit[len(template):-len(".service")]
            try:
                task = self.ledger.inspect(task_id)
            except LedgerError:
                task = None
            if task is None or task.get("status") in TERMINAL_STATES:
                log(f"reconcile: stopping orphaned proxy {unit}")
                self._teardown_containment(task_id)
            else:
                log(f"reconcile: keeping proxy {unit} for live task {task_id}")

    def sweep(self) -> str | None:
        """One pass over queued tasks. Launches at most one worker.
        Returns the launched task id, or None."""
        if self.running:
            return None  # Phase 1: serial execution
        if not self.containment_ready():
            return None
        try:
            tasks = self.ledger.list_tasks()
        except LedgerError as e:
            log(f"sweep: ledger unavailable: {e}")
            return None
        queued = sorted(
            (t for t in tasks if t.get("status") == QUEUED),
            key=lambda t: t.get("created_at", 0),
        )
        for task in queued:
            task_id = task.get("task_id", "")
            if self._try_launch(task):
                return task_id
        return None

    def _try_launch(self, task: dict) -> bool:
        """Run the gates; refuse honestly or launch. Returns True if launched."""
        task_id = task.get("task_id", "")
        harness = task.get("harness")
        repo_identity = task.get("target_repo_identity")

        # Gate 1: harness allowlist (Phase 1: opencode only).
        if harness not in SUPPORTED_HARNESSES:
            self._fail_task(
                task_id,
                f"unsupported_harness: '{harness}' is not in the Phase 1 "
                f"allowlist {sorted(SUPPORTED_HARNESSES)}; refusing to launch")
            return False

        # Gate 2: registered-repository registry.
        try:
            entry = self.registry.resolve(repo_identity)
        except RegistryError as e:
            self._fail_task(task_id, str(e))
            return False
        log(f"task {task_id}: repo identity '{repo_identity}' -> {entry.path}")

        # Gate 3: grant-check hook point (Worker B's consumer).
        granted, reason = self.grant_check(task_id, repo_identity or "", harness or "")
        if not granted:
            self._fail_task(task_id, reason)
            return False
        if reason:
            log(f"task {task_id}: grant check: {reason}")

        # Lease, then a fresh inspect immediately before spawn.
        try:
            self.leases.acquire_lease(task_id)
        except LeaseError as e:
            log(f"task {task_id}: lease refused: {e}; skipping")
            return False
        try:
            fresh = self.ledger.inspect(task_id)
        except LedgerError as e:
            log(f"task {task_id}: inspect before launch failed: {e}; releasing lease")
            self.leases.release_lease(task_id)
            return False
        if fresh.get("status") != QUEUED:
            log(f"task {task_id}: no longer queued (status={fresh.get('status')}); "
                f"releasing lease")
            self.leases.release_lease(task_id)
            return False
        return self._launch(task_id, resume=False, task=fresh)

    def _fail_task(self, task_id: str, reason: str) -> None:
        log(f"task {task_id}: REFUSED: {reason}")
        try:
            self.ledger.fail(task_id, reason)
        except LedgerError as e:
            log(f"task {task_id}: could not record refusal in ledger: {e}")
        self._clear_attempts(task_id)

    def _worker_argv(self, task_id: str, resume: bool,
                     manifest: Path, model: str) -> list[str]:
        argv = [str(self.cfg.worker_path), task_id]
        if resume:
            argv.append("--resume")
        argv += ["--deadline", str(self.cfg.deadline),
                 "--poll-interval", str(self.cfg.poll_interval),
                 "--containment-config", str(manifest),
                 "--model", model]
        return argv

    def _launch(self, task_id: str, resume: bool, task: dict) -> bool:
        """Launch a worker. Returns True only if a worker was actually spawned."""
        try:
            self.leases.acquire_lease(task_id)
        except LeaseError:
            pass  # already held (e.g. by the sweep that called us)
        try:
            manifest, model = self._prepare_containment(task_id, task)
        except ContainmentPrepError as e:
            self.leases.release_lease(task_id)
            if e.permanent:
                self._fail_task(task_id, f"containment_unavailable: {e.reason}")
            else:
                log(f"task {task_id}: containment not ready ({e.reason}); "
                    f"leaving queued for the next sweep")
            return False
        argv = self._worker_argv(task_id, resume, manifest, model)
        log_path = self.cfg.state_dir / WORKER_LOG_DIR_NAME / f"{task_id}.log"
        attempt = self._record_attempt(task_id)
        log(f"launching worker for {task_id} (resume={resume}, attempt {attempt}/"
            f"{MAX_LAUNCH_ATTEMPTS}): {' '.join(argv)}")
        try:
            child = self.spawn(argv, log_path)
        except Exception as e:
            log(f"task {task_id}: worker spawn failed: {e}; releasing lease")
            self.leases.release_lease(task_id)
            return False
        if child.pid:
            self.leases.set_worker_pid(task_id, child.pid)
        self.running[task_id] = child
        self.ledger_ticks[task_id] = 0
        return True

    # -- observation --------------------------------------------------------
    def observe(self) -> None:
        """One observation tick: child exits, external cancels, ledger truth."""
        for task_id in list(self.running):
            child = self.running[task_id]
            rc = child.poll()
            if rc is not None:
                self._on_child_exit(task_id, rc)
                continue
            self.ledger_ticks[task_id] = self.ledger_ticks.get(task_id, 0) + 1
            if self.ledger_ticks[task_id] % LEDGER_POLL_EVERY != 0:
                continue
            try:
                task = self.ledger.inspect(task_id)
            except LedgerError as e:
                log(f"task {task_id}: ledger poll failed: {e}; worker keeps running")
                continue
            status = task.get("status")
            if status in TERMINAL_STATES:
                # Someone ended the task externally (e.g. task-fabric-svn-work task cancel)
                # while the worker is still alive: stop it, then reconcile.
                log(f"task {task_id}: externally {status}; terminating worker")
                self._terminate_child(task_id)
                self._finalize(task_id)
            elif status not in ORPHAN_STATES and status != QUEUED:
                log(f"task {task_id}: unexpected ledger status '{status}'; "
                    f"terminating worker as a precaution")
                self._terminate_child(task_id)
                self._finalize(task_id)

    def _on_child_exit(self, task_id: str, rc: int) -> None:
        """A worker process ended. Never infer success from the exit code:
        only the ledger/task contract decides."""
        self.last_rc[task_id] = rc
        log(f"task {task_id}: worker exited rc={rc}; checking ledger (exit code "
            f"never implies success)")
        try:
            task = self.ledger.inspect(task_id)
        except LedgerError as e:
            log(f"task {task_id}: ledger unreadable after worker exit: {e}; "
                f"keeping lease, will retry")
            self.pending_inspect.add(task_id)
            return
        self.pending_inspect.discard(task_id)
        status = task.get("status")
        if status in TERMINAL_STATES:
            log(f"task {task_id}: ledger says {status}; "
                f"{'completed' if status == COMPLETED else 'ended without completion'}")
            self._finalize(task_id)
            return
        # Non-terminal ledger state after worker exit: anomalous. Relaunch
        # with --resume while the budget lasts, else fail honestly.
        attempts = self._load_attempts().get(task_id, 0)
        if attempts >= MAX_LAUNCH_ATTEMPTS:
            self._fail_task(
                task_id,
                f"supervisor_gave_up: worker exited (rc={rc}) without reaching a "
                f"terminal ledger state after {attempts} launches")
            self._finalize(task_id)
            return
        log(f"task {task_id}: worker exited (rc={rc}) but ledger is '{status}'; "
            f"relaunching with --resume (attempt {attempts + 1}/{MAX_LAUNCH_ATTEMPTS})")
        # Keep the lease held across the relaunch: no other supervisor can
        # exist (instance lock), so the task cannot be double-launched.
        self._launch(task_id, resume=(status != QUEUED), task=task)

    def _terminate_child(self, task_id: str) -> None:
        child = self.running.get(task_id)
        if child is None:
            return
        if child.poll() is not None:
            return
        try:
            child.terminate()
        except Exception as e:
            log(f"task {task_id}: terminate failed: {e}")
            return
        try:
            child.wait(timeout=TERM_GRACE)
        except Exception:
            try:
                child.kill()
            except Exception as e:
                log(f"task {task_id}: kill failed: {e}")

    def _finalize(self, task_id: str) -> None:
        self.running.pop(task_id, None)
        self.pending_inspect.discard(task_id)
        self.last_rc.pop(task_id, None)
        self.ledger_ticks.pop(task_id, None)
        self._teardown_containment(task_id)
        self.leases.release_lease(task_id)
        self._clear_attempts(task_id)
        log(f"task {task_id}: finalized (lease released)")

    # -- run modes ------------------------------------------------------------
    def run_once(self) -> None:
        self.startup()
        try:
            self.sweep()
            while self.running and not self.stopping:
                self.observe()
                time.sleep(OBSERVE_INTERVAL)
        finally:
            self.shutdown()

    def run_forever(self) -> None:
        self.startup()
        try:
            while not self.stopping:
                self.sweep()
                self.observe()
                # Retry ledger reads for children whose exit we saw while the
                # ledger was unreadable.
                for task_id in list(self.pending_inspect):
                    if task_id in self.running:
                        self._on_child_exit(task_id, self.last_rc.get(task_id, -1))
                time.sleep(OBSERVE_INTERVAL)
        finally:
            self.shutdown()

    def reconcile_only(self) -> None:
        self.startup()
        self.shutdown()

    def request_stop(self) -> None:
        self.stopping = True

    def shutdown(self) -> None:
        for task_id in list(self.running):
            self._terminate_child(task_id)
            # Do NOT release leases here on a dirty shutdown path: the next
            # startup reaps them after proving no supervisor is alive. On a
            # clean stop we release so a prompt restart reconciles faster.
            self.leases.release_lease(task_id)
        self.running.clear()
        self.leases.release_instance_lock()
        log("supervisor stopped")


class _AdoptedChild(ChildProcess):
    """A worker that outlived its supervisor: observed by pid, not waited on."""

    def __init__(self, task_id: str, worker_pid: int, pid_alive_fn):
        self._worker_pid = worker_pid
        self._pid_alive = pid_alive_fn
        self.task_id = task_id
        self.log_path = Path(f"<adopted pid={worker_pid}>")

    @property
    def pid(self) -> int | None:
        return self._worker_pid

    def poll(self) -> int | None:
        return None if self._pid_alive(self._worker_pid) else -1

    def terminate(self) -> None:
        os.kill(self._worker_pid, signal.SIGTERM)

    def kill(self) -> None:
        os.kill(self._worker_pid, signal.SIGKILL)

    def wait(self, timeout: float | None) -> int:
        deadline = time.time() + (timeout or 0)
        while self._pid_alive(self._worker_pid) and time.time() < deadline:
            time.sleep(0.2)
        return 0 if not self._pid_alive(self._worker_pid) else 1


# ---------------------------------------------------------------------------
# CLI
# ---------------------------------------------------------------------------

def default_state_dir() -> Path:
    return Path(os.path.expanduser("~/.local/state/task-fabric-svn"))


def default_registry_path() -> Path:
    override = os.environ.get("TASK_FABRIC_SVN_TASK_REPOSITORIES")
    if override:
        return Path(override).expanduser()
    here = Path(__file__).resolve().parent
    return here.parent / "config" / "task-repositories.toml"


def default_worker_path() -> Path:
    return Path(__file__).resolve().parent / "task-fabric-svn-task-worker.py"


def default_proxy_manifest_dir() -> Path:
    return default_state_dir() / PROXY_MANIFEST_SUBDIR


def default_proxy_socket_dir() -> Path:
    return default_state_dir() / PROXY_SOCKET_SUBDIR


def default_proxy_env_file() -> Path:
    return Path(os.path.expanduser("~/.config/task_fabric_svn/task-proxy.env"))


def default_proxy_unit_dir() -> Path:
    return Path(os.path.expanduser("~/.config/systemd/user"))


def parse_args(argv: list[str] | None = None) -> argparse.Namespace:
    ap = argparse.ArgumentParser(description="Task Fabric SVN Task Supervisor")
    mode = ap.add_mutually_exclusive_group()
    mode.add_argument("--loop", action="store_true",
                      help="run forever (systemd mode)")
    mode.add_argument("--once", action="store_true",
                      help="one sweep, then exit (default)")
    mode.add_argument("--reconcile-only", action="store_true",
                      help="reconcile orphaned tasks, then exit")
    ap.add_argument("--state-dir", default=str(default_state_dir()))
    ap.add_argument("--registry", default=str(default_registry_path()))
    ap.add_argument("--worker", default=str(default_worker_path()))
    ap.add_argument("--proxy-manifest-dir",
                    default=str(default_proxy_manifest_dir()),
                    help="directory for per-task proxy manifests")
    ap.add_argument("--proxy-socket-dir",
                    default=str(default_proxy_socket_dir()),
                    help="directory for per-task proxy sockets")
    ap.add_argument("--proxy-env-file",
                    default=str(default_proxy_env_file()),
                    help="owner-controlled env file carrying TASK_FABRIC_SVN_PROXY_HMAC_SECRET")
    ap.add_argument("--proxy-unit-dir",
                    default=str(default_proxy_unit_dir()),
                    help="systemd user unit dir holding the proxy template unit")
    ap.add_argument("--proxy-unit-template",
                    default="task-fabric-svn-task-inference-proxy@",
                    help="proxy template unit prefix; instances are "
                         "<template><task-id>.service")
    ap.add_argument("--task-model",
                    default=os.environ.get("TASK_FABRIC_SVN_TASK_MODEL", DEFAULT_TASK_MODEL),
                    help="inference model pinned in every task's proxy contract "
                         "(Phase-1 grant allowlist)")
    ap.add_argument("--deadline", type=int, default=43200)
    ap.add_argument("--poll-interval", type=int, default=15)
    ap.add_argument("--grant-hook", default=os.environ.get("TASK_FABRIC_SVN_SUPERVISOR_GRANT_CHECK"),
                    help="executable consulted per task: "
                         "<hook> <task-id> <repo-identity> <harness>; rc 0 = grant")
    return ap.parse_args(argv)


def main() -> int:
    args = parse_args()
    cfg = SupervisorConfig(
        state_dir=Path(args.state_dir).expanduser(),
        worker_path=Path(args.worker),
        registry_path=Path(args.registry),
        manifest_dir=Path(args.proxy_manifest_dir).expanduser(),
        socket_dir=Path(args.proxy_socket_dir).expanduser(),
        proxy_env_file=Path(args.proxy_env_file).expanduser(),
        proxy_unit_dir=Path(args.proxy_unit_dir).expanduser(),
        proxy_unit_template=args.proxy_unit_template,
        task_model=args.task_model,
        deadline=args.deadline,
        poll_interval=args.poll_interval,
        grant_hook=args.grant_hook,
    )
    cfg.state_dir.mkdir(parents=True, exist_ok=True)
    try:
        ledger = TaskFabricSvnWorkLedger()
    except LedgerError as e:
        log(f"fatal: {e}")
        return 2
    try:
        registry = RepositoryRegistry(cfg.registry_path)
    except RegistryError as e:
        log(f"fatal: {e}")
        return 2
    leases = LeaseStore(cfg.state_dir, cfg.supervisor_id)
    supervisor = Supervisor(cfg, ledger, registry, leases)

    def on_signal(signum, _frame):
        log(f"received signal {signum}; stopping cleanly")
        supervisor.request_stop()

    signal.signal(signal.SIGTERM, on_signal)
    signal.signal(signal.SIGINT, on_signal)

    try:
        if args.reconcile_only:
            supervisor.reconcile_only()
        elif args.loop:
            supervisor.run_forever()
        else:
            supervisor.run_once()
    except SupervisorAlreadyRunning as e:
        log(f"not starting: {e}")
        return 3
    return 0


if __name__ == "__main__":
    sys.exit(main())
