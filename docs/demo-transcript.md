# Demo transcript — status and a task lifecycle, captured live

Captured 2026-11-14 at commit a186b5c on the author's workstation — unedited command output.

Setup: `cargo build` (debug), the example registry `examples/projects.toml`, a scratch
task-ledger state directory (`TASK_FABRIC_SVN_WORK_STATE` pointed at a temp dir), and
`--socket` pointed at a path with no daemon, so `status` uses its local read-only
collection instead of the live daemon. Nothing below touched live state.

## 1. Status across the registry (read-only)

```console
$ task-fabric-svn-work status --json --config /home/chris/Work/tf-wt-demo-transcript/examples/projects.toml --socket /tmp/tf-demo-scratch/no-daemon.sock
{
  "schema_version": 4,
  "generated_at_epoch_seconds": 1794689679,
  "source": "local_fallback",
  "config_path": "/home/chris/Work/tf-wt-demo-transcript/examples/projects.toml",
  "using_example_config": false,
  "projects": [
    {
      "id": "example-project",
      "label": "Example Project",
      "phase": "example"
    }
  ],
  "project": {
    "id": "example-project",
    "label": "Example Project",
    "repo": "/home/user/Work/example-project",
    "branch": "not-a-repository",
    "dirty": false,
    "ahead": null,
    "behind": null,
    "objective": "Show what a registered project looks like.",
    "phase": "example",
    "next_action": "Replace this entry with one of your own projects.",
    "herdr_session": null,
    "study_program_refs": [],
    "study_domain_refs": [],
    "research_question_refs": []
  },
  "suggested_project": null,
  "agents": [
    {
      "id": "codex",
      "label": "Codex",
      "state": "unknown",
      "detail": "Herdr integration is not connected yet",
      "raw_state": null,
      "session": null,
      "cwd": null,
      "project_id": null,
      "duration": null,
      "updated_at": null
    },
    {
      "id": "opencode",
      "label": "opencode",
      "state": "unknown",
      "detail": "Herdr integration is not connected yet",
      "raw_state": null,
      "session": null,
      "cwd": null,
      "project_id": null,
      "duration": null,
      "updated_at": null
    }
  ],
  "services": [],
  "attention": [],
  "actions": [
    {
      "id": "open-repo",
      "label": "Open repository",
      "risk": "review",
      "command": [
        "xdg-open",
        "."
      ],
      "cwd": "/home/user/Work/example-project"
    },
    {
      "id": "run-tests",
      "label": "Run workspace tests",
      "risk": "review",
      "command": [
        "cargo",
        "test",
        "--workspace"
      ],
      "cwd": "/home/user/Work/example-project"
    }
  ],
  "degraded": [],
  "durable_value": {
    "level": "quiet",
    "score": 0,
    "summary": "No significant durable-value drift detected.",
    "reasons": [],
    "metrics": {
      "abstraction_paths": 0,
      "service_or_config_paths": 0,
      "deletion_ratio_percent": 0,
      "codex_recent_activity": 0,
      "codex_title_matches": 0,
      "recent_reality_signals": 0
    },
    "last_evaluated_at": 1794689679,
    "codex_metadata_available": true,
    "recent_positive_signals": []
  },
  "selected_world_id": null,
  "requested_world_id": null,
  "active_world_id": null,
  "world_activation_status": "unavailable",
  "active_trace_id": null,
  "last_transition_id": null
}
```

The example project's repo (`/home/user/Work/example-project`) does not exist on this
machine, so the honest answer is `"branch": "not-a-repository"` — status reports what
it finds, it does not invent a healthier picture.

## 2. A task lifecycle in the durable ledger

The ledger starts empty:

```console
$ task-fabric-svn-work task list
{"ok":true,"tasks":[]}
```

Submit a task (it lands `queued`):

```console
$ task-fabric-svn-work task submit --conversation-id demo-conversation --target-space-id example-project --intent-summary "Demo task: verify the ledger records a lifecycle honestly" --risk-class read
{"ok":true,"task":{"artifact_refs":[],"conversation_id":"demo-conversation","created_at":1794689680,"diff_manifest":[],"intent_summary":"Demo task: verify the ledger records a lifecycle honestly","parameters":{},"receipt_refs":[],"resolved_envelopes":[],"risk_class":"read","schema_version":1,"state_history":[{"at":1794689680,"from":"queued","reason":"submitted","to":"queued"}],"status":"queued","target_space_id":"example-project","task_id":"task-79f12a782e62","updated_at":1794689680},"task_id":"task-79f12a782e62"}
```

List and inspect it:

```console
$ task-fabric-svn-work task list
{"ok":true,"tasks":[{"artifact_refs":[],"conversation_id":"demo-conversation","created_at":1794689680,"diff_manifest":[],"intent_summary":"Demo task: verify the ledger records a lifecycle honestly","parameters":{},"receipt_refs":[],"resolved_envelopes":[],"risk_class":"read","schema_version":1,"state_history":[{"at":1794689680,"from":"queued","reason":"submitted","to":"queued"}],"status":"queued","target_space_id":"example-project","task_id":"task-79f12a782e62","updated_at":1794689680}]}
$ task-fabric-svn-work task inspect task-79f12a782e62
{"ok":true,"task":{"artifact_refs":[],"conversation_id":"demo-conversation","created_at":1794689680,"diff_manifest":[],"intent_summary":"Demo task: verify the ledger records a lifecycle honestly","parameters":{},"receipt_refs":[],"resolved_envelopes":[],"risk_class":"read","schema_version":1,"state_history":[{"at":1794689680,"from":"queued","reason":"submitted","to":"queued"}],"status":"queued","target_space_id":"example-project","task_id":"task-79f12a782e62","updated_at":1794689680}}
```

Drive the state machine — `queued → starting → working → completed`:

```console
$ task-fabric-svn-work task start task-79f12a782e62 --harness-session-id demo-session-1
{"ok":true,"task":{"artifact_refs":[],"conversation_id":"demo-conversation","created_at":1794689680,"diff_manifest":[],"harness_session_id":"demo-session-1","intent_summary":"Demo task: verify the ledger records a lifecycle honestly","parameters":{},"receipt_refs":[],"resolved_envelopes":[],"risk_class":"read","schema_version":1,"state_history":[{"at":1794689680,"from":"queued","reason":"submitted","to":"queued"},{"at":1794689680,"from":"queued","to":"starting"}],"status":"starting","target_space_id":"example-project","task_id":"task-79f12a782e62","updated_at":1794689680}}
$ task-fabric-svn-work task begin-work task-79f12a782e62
{"ok":true,"task":{"artifact_refs":[],"conversation_id":"demo-conversation","created_at":1794689680,"diff_manifest":[],"harness_session_id":"demo-session-1","intent_summary":"Demo task: verify the ledger records a lifecycle honestly","parameters":{},"receipt_refs":[],"resolved_envelopes":[],"risk_class":"read","schema_version":1,"state_history":[{"at":1794689680,"from":"queued","reason":"submitted","to":"queued"},{"at":1794689680,"from":"queued","to":"starting"},{"at":1794689680,"from":"starting","to":"working"}],"status":"working","target_space_id":"example-project","task_id":"task-79f12a782e62","updated_at":1794689680}}
$ task-fabric-svn-work task note-progress task-79f12a782e62 --note "Demo progress note recorded in the ledger"
{"ok":true,"task":{"artifact_refs":[],"conversation_id":"demo-conversation","created_at":1794689680,"diff_manifest":[],"harness_session_id":"demo-session-1","intent_summary":"Demo task: verify the ledger records a lifecycle honestly","parameters":{},"receipt_refs":[],"resolved_envelopes":[],"risk_class":"read","schema_version":1,"state_history":[{"at":1794689680,"from":"queued","reason":"submitted","to":"queued"},{"at":1794689680,"from":"queued","to":"starting"},{"at":1794689680,"from":"starting","to":"working"}],"status":"working","target_space_id":"example-project","task_id":"task-79f12a782e62","updated_at":1794689680}}
$ task-fabric-svn-work task complete task-79f12a782e62 --result-summary "Demo task completed through the CLI lifecycle"
{"ok":true,"task":{"artifact_refs":[],"conversation_id":"demo-conversation","created_at":1794689680,"diff_manifest":[],"harness_session_id":"demo-session-1","intent_summary":"Demo task: verify the ledger records a lifecycle honestly","parameters":{},"receipt_refs":[],"resolved_envelopes":[],"result_summary":"Demo task completed through the CLI lifecycle","risk_class":"read","schema_version":1,"state_history":[{"at":1794689680,"from":"queued","reason":"submitted","to":"queued"},{"at":1794689680,"from":"queued","to":"starting"},{"at":1794689680,"from":"starting","to":"working"},{"at":1794689680,"from":"working","to":"completed"}],"status":"completed","target_space_id":"example-project","task_id":"task-79f12a782e62","updated_at":1794689680}}
```

Inspect the finished task, and read its event history:

```console
$ task-fabric-svn-work task inspect task-79f12a782e62
{"ok":true,"task":{"artifact_refs":[],"conversation_id":"demo-conversation","created_at":1794689680,"diff_manifest":[],"harness_session_id":"demo-session-1","intent_summary":"Demo task: verify the ledger records a lifecycle honestly","parameters":{},"receipt_refs":[],"resolved_envelopes":[],"result_summary":"Demo task completed through the CLI lifecycle","risk_class":"read","schema_version":1,"state_history":[{"at":1794689680,"from":"queued","reason":"submitted","to":"queued"},{"at":1794689680,"from":"queued","to":"starting"},{"at":1794689680,"from":"starting","to":"working"},{"at":1794689680,"from":"working","to":"completed"}],"status":"completed","target_space_id":"example-project","task_id":"task-79f12a782e62","updated_at":1794689680}}
$ task-fabric-svn-work task history task-79f12a782e62
{"events":[{"at":1794689680,"kind":"submitted","payload":{"conversation_id":"demo-conversation","intent_summary":"Demo task: verify the ledger records a lifecycle honestly","risk_class":"read","target_space_id":"example-project"},"schema_version":1,"seq":0,"task_id":"task-79f12a782e62"},{"at":1794689680,"kind":"started","payload":{"harness_session_id":"demo-session-1"},"schema_version":1,"seq":1,"task_id":"task-79f12a782e62"},{"at":1794689680,"kind":"progress","payload":{"from":"starting","reason":null,"to":"working"},"schema_version":1,"seq":2,"task_id":"task-79f12a782e62"},{"at":1794689680,"kind":"progress","payload":{"note":"Demo progress note recorded in the ledger"},"schema_version":1,"seq":3,"task_id":"task-79f12a782e62"},{"at":1794689680,"kind":"completed","payload":{"artifact_refs":[],"receipt_refs":[],"result_summary":"Demo task completed through the CLI lifecycle"},"schema_version":1,"seq":4,"task_id":"task-79f12a782e62"}],"ok":true,"task_id":"task-79f12a782e62"}
```

Boundary, plainly: these lifecycle transitions were driven directly through the CLI
against a scratch ledger. No supervisor leased the task and no worker harness ran an
agent — that dispatch path needs the running supervisor, and it is not shown here
rather than faked.

## What to take from this

- The ledger is durable and explicit: every transition is an event in `task history`,
  and the record carries its full `state_history` — a task cannot quietly skip states.
- Statuses are honest: a missing repo reads `not-a-repository`, an empty ledger reads
  `{"ok":true,"tasks":[]}`, and completion exists only because a `complete` call
  recorded it — never because a process exited.
