# Task Supervisor

The hole in the center of the Task Fabric: Field can submit durable tasks to
the ledger and the contained worker (`scripts/task-fabric-svn-task-worker.py`) exists,
but nothing automatically decided to run the worker when a task was queued.
The supervisor (`scripts/task-fabric-svn-task-supervisor.py`) is that decision, made
deliberately boring and deterministic. It is not intelligent; it discovers,
validates, leases, launches, and observes.

## The loop

Every sweep, in order:

1. **Reconcile (startup and after every worker exit).** The ledger is the
   only source of truth -- process state is never trusted. Tasks left in
   `starting` / `working` / `input_required` / `approval_required` by a dead
   worker or a dead supervisor are relaunched with `--resume`, bounded by a
   per-task attempt budget (3 launches, persisted across restarts in
   `~/.local/state/task-fabric-svn/supervisor-attempts.json`). When the
   budget is spent the task is failed honestly as `supervisor_gave_up`.
   A worker that outlived its supervisor is adopted, never duplicated.
2. **Gate.** The oldest queued task must pass three checks or it is refused
   (failed in the ledger with the reason, no worker launched):
   - harness allowlist: `opencode` only (Phase 1);
   - registered-repository registry: `target_repo_identity` must be in
     `config/task-repositories.toml`, with a live path and a matching git
     origin fingerprint;
   - grant-check hook point: an executable named by
     `TASK_FABRIC_SVN_SUPERVISOR_GRANT_CHECK`, called as
     `<hook> <task-id> <repo-identity> <harness>`; exit 0 grants.
     Phase 1 default (no hook configured): allow, logged.
3. **Lease.** A per-task lease file is created atomically
   (`O_CREAT|O_EXCL`) under `~/.local/state/task-fabric-svn/task-leases/`,
   so two supervisors can never run the same task. A machine-wide
   single-instance lock (boot-id + pid) proves at most one supervisor runs.
4. **Launch.** The existing `task-fabric-svn-task-worker.py` is spawned as a child
   (wrapped, never rewritten) with `--deadline`, `--poll-interval`, and
   `--containment-config`. A fresh ledger inspect happens immediately before
   spawn; if the task is no longer queued the lease is released.
5. **Observe.** While the worker runs, the supervisor polls the child and the
   ledger. A worker exit **never** implies success -- only the ledger/task
   contract does. External cancellation (`task-fabric-svn-work task cancel`) terminates
   the worker within one poll interval.

## Survival

- **Supervisor crash / machine reboot:** the instance lock is re-acquired
  (stale locks from a dead boot are taken over), all task leases are reaped,
  and orphaned tasks are reconciled from the ledger.
- **Worker crash:** observed exit, ledger re-inspected, relaunch with
  `--resume` while the budget lasts.
- **Ledger outage:** if the ledger is unreadable after a worker exit, the
  lease is kept and the decision is retried -- the supervisor never relaunches
  blind.
- **No double-launch:** single-instance lock + per-task lease + fresh inspect
  before spawn.

## Fail-closed preconditions

The supervisor launches **nothing** unless `--containment-config-path` (or
`TASK_FABRIC_SVN_SUPERVISOR_CONTAINMENT_CONFIG`) points at a real file -- the
owner-controlled task proxy manifest from the production proxy/grant path
(Worker B's work). This is a supervisor-level precondition: while it is
absent, queued tasks simply wait. The supervisor never touches a repository
checkout; repo mutation happens only through the worker's checked-result
application path.

## Cancellation

`task-fabric-svn-work task cancel <task-id> --reason "..."` (the existing ledger
contract). The supervisor sees the terminal state on its next ledger poll,
terminates the worker (SIGTERM, then SIGKILL after 10s), releases the lease,
and clears the attempt counter. SIGTERM to the supervisor stops children
cleanly.

## Deployment

Enable only after you have verified the install in your own environment:

```
cp systemd/task-fabric-svn-task-supervisor.service ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now task-fabric-svn-task-supervisor.service
# then retire the polling loop:
systemctl --user disable --now task-fabric-svn-task-worker.service
```

Logs go to the journal (`journalctl --user -u task-fabric-svn-task-supervisor`) and
per-task worker logs to
`~/.local/state/task-fabric-svn/worker-logs/<task-id>.log`.

## Tests

`scripts/tests/test_task_fabric_svn_task_supervisor.py` (unittest, no live state):
lease exclusivity, stale-lease reaping, single-instance lock, launch argv,
serial execution, all three gate refusals, fail-closed containment,
exit-code-never-implies-success, crash budget, external cancellation,
orphan reconciliation, live-worker adoption.
