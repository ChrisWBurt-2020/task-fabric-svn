# Task Fabric SVN

A local-first control plane for working with coding agents. One registry
describes your projects. A read-only status daemon observes them, while a
separate durable task fabric dispatches bounded work to contained agent
harnesses — with honest failure semantics and fail-closed spend control
throughout.

Built in Rust (5 crates) and Python, deployed as systemd user services.

## What it does

**Project registry + status.** Each project declares a repository, agents,
services, and review-class actions in `projects.toml`.
`task-fabric-svn-work status --json` reports git state, service health, and
next actions across all of them — read-only collection, never a side effect.
Observation and execution are separate planes: the status daemon cannot
start, stop, or edit anything.

**Task fabric.** Durable tasks with an explicit state machine (below). Submit
a task from anywhere; the supervisor discovers, validates, leases, launches,
and observes a contained worker for it. Crashes are reconciled from the
ledger on restart — process state is never trusted.

**Attention routing.** Machine work flows through an attention policy —
*does the human need to know?* — and surfaces as visual, mobile, spoken, or
silent. A small sound daemon is one actuator behind that policy: it speaks
approvals, completions, and warnings through an adaptive voice pipeline
(pre-recorded templates → local neural TTS → fallbacks), with per-app voice
routing, rate limits, and media ducking.

### The task state machine

Implemented in `crates/task-fabric-svn-work-core/src/task_ledger.rs`. The
transition table is closed: anything not listed is rejected.

```text
queued ──→ starting ──→ working ──→ completed
  │            │             ├─→ input_required ──→ working
  │            │             ├─→ approval_required ──→ working    (approved)
  │            │             │                          └─→ cancelled   (denied)
  │            │             ├─→ failed
  │            │             └─→ cancelled
  │            ├─→ failed
  │            └─→ cancelled
  ├─→ failed       (budget / deadline pre-checks)
  └─→ cancelled    (cancelled before a worker claimed it)

input_required ──→ failed / cancelled
approval_required ──→ failed
```

Terminal states: `completed`, `failed`, `cancelled`. There is no separate
"denied" state — denying an approval cancels the task. Strings like
`budget_exhausted`, `deadline_exceeded`, and `supervisor_gave_up` are
*failure reasons* recorded on `failed`, not states.

## The craft

### 1. Tasks that can't lie to you

The worker and its harness communicate through a three-marker protocol
(`docs/TASK_WORKER_PROTOCOL.md`):

- `TASK_FABRIC_SVN_QUESTION:` — the agent needs a human decision; the task pauses.
- `TASK_FABRIC_SVN_APPROVAL:` — the agent wants to do something consequential;
  a human approves, denies, or approves with a written constraint.
- `TASK_FABRIC_SVN_DONE:` — the agent claims completion, with a summary and
  artifact refs.

A worker exit **never** implies success — only the ledger does. Zero, two, or
malformed markers fail the task with an honest reason. A denied approval ends
the task terminally; the worker never retries a denied action on its own.

### 2. Fail-closed spend control

Every dispatched task gets its own inference proxy (`systemd` template unit,
one instance per task). The proxy:

- strips whatever credentials the sandbox presents and replaces them with the
  deployment's own key — the contained worker can never exfiltrate or abuse
  provider credentials;
- signs each request with an HMAC attestation so the upstream router can
  verify the claim came from this proxy;
- enforces the owner-approved contract at runtime: pinned model (anything
  else is rejected as `task_proxy_model_mismatch`), `max_requests`, and
  `max_request_bytes` — a runaway agent hits
  `task inference request limit reached`, not your wallet;
- **refuses to start without its secrets** — no credentials, no proxy, no
  task. Refusal is terminal and machine-readable, never silently retried on
  another model.

The worker runs inside a `bwrap` sandbox with an allowlisted filesystem view,
a cleared environment, and per-task bind mounts. The design rule is simple:
when money or credentials are involved, the system stops rather than guesses.

### 3. The harness is a replaceable execution engine

This is the core architectural bet. Projects declare agents (`opencode`,
`codex`, `claude`, …) as data, not code paths. Actions are argument arrays —
the runner never evaluates shell strings.

```text
                    Task Fabric SVN
                          │
                     Task Contract
                          │
          ┌───────────────┼───────────────┐
          │               │               │
        Codex          OpenCode          Claude
          │               │               │
          └────────── durable work ───────┘
```

The fabric owns intent, task identity, durable state, authority, spending,
human gates, receipts, and lifecycle. The harness owns execution. Models and
harnesses commoditize fast; the durable layer above them is the part that
compounds.

The supervisor gates every launch on three checks: a harness allowlist, a
registered-repository registry with git-origin fingerprint verification, and
an optional grant hook. An unregistered repo is refused with an honest
reason, not launched anyway.

### 4. Attention routing, not just TTS

The important thing was never text-to-speech. It is the pipeline:

```text
Machine work
     ↓
Attention policy
     ↓
Does the human need to know?
     ↓
visual / mobile / spoken / silent
```

`task-fabric-svn-soundd` is one actuator behind that broker. Humor profiles
are opt-in and never touch approvals, failures, or critical events — and
every quip can be previewed with a dry run that speaks nothing and spends
nothing.

## Honest limits / roadmap

What this tree does not claim:

- **Per-task monetary budgets.** Request-count/size budgets and credential
  control are enforced in-tree (above). Per-task dollar budgets
  (`max_cost_usd`), per-token accounting, and unknown-price rejection live
  in the separate Governor budget service — not claimed here.
- **Network/egress authority.** Filesystem containment is explicit and
  allowlisted; per-task network authority is not yet modeled.
- **Isolation tiers above `bwrap`.** `bwrap` + cleared env + allowlisted
  mounts is solid containment for your own trusted workstation and repos —
  it is not a hardened boundary for hostile code. Container and microVM
  tiers are future work; the fabric decides *what work exists and what
  authority it receives*, so the execution substrate can change underneath
  it.

## Quick start

```sh
cargo test                                          # workspace tests
cargo run -p task-fabric-svn-work -- status --json
cargo run -p task-fabric-svn-work -- task list
```

Copy `examples/projects.toml` to `~/.config/task-fabric-svn/projects.toml`
and adapt it. `TASK_FABRIC_SVN_WORK_CONFIG` overrides the path for tests.

The installer is deliberately conservative — preview first, apply second:

```sh
./scripts/install.sh          # prints the plan, changes nothing
./scripts/install.sh --apply  # timestamped backup, then installs
```

To dispatch tasks, register a repository in `config/task-repositories.toml`
(a deliberate, committed act), then enable the supervisor:

```sh
cp systemd/task-fabric-svn-task-supervisor.service ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now task-fabric-svn-task-supervisor.service
```

## Layout

```
crates/task-fabric-svn-work-core   status, world state, task ledger, attention + audio policy
crates/task-fabric-svn-work        `task-fabric-svn-work` CLI (status, task, audio, signals)
crates/task-fabric-svn-workd       status daemon (Unix socket, desktop notifications)
crates/task-fabric-svn-soundd      adaptive notification audio / local TTS
crates/task-fabric-svn-experiment-worker  bounded, isolated evaluation trials
scripts/                 task supervisor, worker, containment, per-task proxy,
                         installers, TTS helpers (each with unit tests)
systemd/                 user service units
contracts/v1/            JSON schemas for world events and transitions
docs/                    supervisor design, worker protocol, install guide
adapter/ templates/      desktop-shell integration examples
```

## Tests

Rust: `cargo test` per crate. Python: `scripts/tests/test_*.py` (unittest,
no live state required) — lease exclusivity, gate refusals, fail-closed
containment, marker-protocol violations, crash budgets, orphan
reconciliation.

## License

MIT.
