# TASK-FABRIC-SVN Task Worker Protocol

This document specifies the marker contract between the Task Fabric SVN task worker (`task-fabric-svn-task-worker.py`) and the harness. The protocol uses three special markers to signal human decisions, approval gates, and task completion.

## Marker Syntax

### `TASK_FABRIC_SVN_QUESTION: <question>`

- The harness emits this marker at the end of a turn when a human decision is required.
- `<question>` must be a single line of plain text (no markdown formatting).
- **Exactly** one `TASK_FABRIC_SVN_QUESTION:` line must appear, and nothing else may follow it on that line or subsequently in the turn.
- The worker moves the task to `input_required`, prompts the human, and waits for a response before resuming the harness in the same session.
- If the harness needs to ask multiple questions, it should do so across separate turns, emitting one `TASK_FABRIC_SVN_QUESTION:` per turn.

### `TASK_FABRIC_SVN_APPROVAL: <short description of the action>`

- The harness emits this marker at the end of a turn when it needs the human to approve a specific action before taking it.
- `<short description of the action>` must be a single line of plain text naming the action (e.g. `TASK_FABRIC_SVN_APPROVAL: push the docs commit to origin`).
- **Exactly** one `TASK_FABRIC_SVN_APPROVAL:` line must appear, and nothing else may follow it on that line or subsequently in the turn.
- The worker moves the task to `approval_required` with the action description and waits. The human resolves the gate with one of:
  - `task-fabric-svn-work task approve <task-id> --approved true` — yes, proceed
  - `task-fabric-svn-work task approve <task-id> --approved true --reason "<write-in>"` — yes, with a free-text instruction the worker passes to the harness as a hard constraint
  - `task-fabric-svn-work task approve <task-id> --approved false --reason "approval_denied: <why>"` — no
- On approval the worker re-invokes the harness in the same session with the approval (and any write-in) before any other work. On denial the task ends terminally: the ledger maps denial to `cancelled`, and the reason is recorded as `approval_denied: <why>`. The worker never retries a denied action on its own.

### `TASK_FABRIC_SVN_DONE: <result summary>`

- The harness emits this marker at the end of a completed turn to signal successful task completion.
- `<result summary>` must be a concise summary (≤4000 characters) of what was accomplished, plus any new/modified repo files as artifact refs (via `git status`).
- **Exactly** one `TASK_FABRIC_SVN_DONE:` line must appear, and nothing else may follow it.
- After `TASK_FABRIC_SVN_DONE`, the task is considered complete and the worker exits.

## Marker Precedence

A turn that asks something — or requests approval — is not done. When several markers appear on one turn, precedence is:

`TASK_FABRIC_SVN_QUESTION` > `TASK_FABRIC_SVN_APPROVAL` > `TASK_FABRIC_SVN_DONE`

## Turn Lifecycle

1. **starting** — Worker claims a task from the durable ledger.
2. **working** — Worker executes opencode to make progress.
3. **input_required** — Harness emits `TASK_FABRIC_SVN_QUESTION:` and pauses for human input.
4. **approval_required** — Harness emits `TASK_FABRIC_SVN_APPROVAL:` and pauses for human approval (yes / no / write-in).
5. **resume** — Human answers or approves; worker re-invokes opencode in the same session with the answer, or the approval plus any write-in constraint.
6. **completed** — Harness emits `TASK_FABRIC_SVN_DONE:` and the task finishes.

## Failure Conditions

- Zero, two+, or malformed `TASK_FABRIC_SVN_QUESTION:` / `TASK_FABRIC_SVN_APPROVAL:` / `TASK_FABRIC_SVN_DONE:` markers cause the task to fail with an honest reason.
- Non-zero exit, timeout, or missing marker results in task failure (no silent retries, no invented results).
- A denied approval ends the task terminally (`cancelled`, with `approval_denied: <why>` recorded as the reason).

## Examples

**Asking a question:**
```
TASK_FABRIC_SVN_QUESTION: Do you want me to create docs/TASK_WORKER_PROTOCOL.md documenting this protocol?
```

**Requesting approval:**
```
TASK_FABRIC_SVN_APPROVAL: push the docs commit to origin
```

Resolving the approval gate:
```
task-fabric-svn-work task approve task-abc --approved true --reason "only this branch, no force-push"
```

**Completing the task:**
```
TASK_FABRIC_SVN_DONE: Created docs/TASK_WORKER_PROTOCOL.md documenting the TASK_FABRIC_SVN_QUESTION/TASK_FABRIC_SVN_APPROVAL/TASK_FABRIC_SVN_DONE marker contract. New file added: docs/TASK_WORKER_PROTOCOL.md
```
