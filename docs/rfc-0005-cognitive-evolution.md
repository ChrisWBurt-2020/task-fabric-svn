# RFC-0005: Cognitive Evolution — Grounding the Mind That Changes Shape Over Time

- **Author**: The Task Fabric SVN contributors
- **Date**: 2026-09-12
- **Status**: Accepted / Active Implementation
- **Extends**: `docs/ideation/04-fresh-eyes-ideation.md`
- **Reconciles**: Architectural Review & Phased Technical Ordering

> **Export note:** `docs/ideation/*` and the architectural review memo referenced
> above are not part of this public snapshot.

---

## 1. Executive Summary

Doc 04 established the core thesis: *the substrate is solved, but the mind is missing*. Omarchy now possesses a working observer, recorder, gatekeeper, vector memory, and voice mesh. What was missing was **a mind that changes shape over time**.

However, a cognitive operating system cannot be built upon speculative manifesto claims or ungrounded abstractions. As established by the architectural review:
1. **No building the roof first**: A belief layer or state compiler presupposes immutable provenance and trustworthy intent lineage that must be established first.
2. **Machine-checkable verification**: Reconciliation only works where targets are machine-checkable (filesystem paths, listening ports, process PIDs, git tree states, systemd units, test exit codes).
3. **Every phase requires an explicit Grounding and a First Tracer**.

This RFC defines the **Cognitive Evolution Roadmap**, directly mapping the 4-phase technical sequence to the cognitive frontiers of Doc 04.

---

## 2. Four-Phase Evolution Matrix

| Phase | Technical Milestone | Doc 04 Cognitive Frontier | Primary Grounding | First Tracer |
| :--- | :--- | :--- | :--- | :--- |
| **Phase 1** | **Receipts & Intent Lineage** | **The Loom** (Causal Replay Scrubber) | `trace_id`, `WorldEvent`, `WorldTransitionReceipt`, `cognitive_links` | `task-fabric-svn-work why <ref>` |
| **Phase 2** | **Counterfactual Dry-Run** | **The Flight Simulator** (Rehearsal) | `snapshot_for`, `plan_for`, machine port/service probes | `task-fabric-svn-work action dry-run <world> <action>` |
| **Phase 3** | **Attention Economics** | **The Focus Guardian** (Attention Economy) | `attention_alert.rs`, `horizon.rs` focus, `durable.rs` | Focus spend report + active window suppression |
| **Phase 4** | **Compounding Provenance** | **The Friction Compiler & The Tutor** | `EffectOutcome::Degraded`, `projects.toml`, `exocortex.db` | `task-fabric-svn-work improve` + `task-fabric-svn-work learn drill` |

---

## 3. Phase 1 Detailed Specification: The Loom (Causal Lineage)

### Grounding
- `trace_id` in `TransitionReference` (`crates/task-fabric-svn-work-core/src/worldstate.rs`)
- `WorldTransitionReceipt` and `WorldEvent`
- Runtime socket trace publishing (`task-fabric-svn/runtime.sock`)

### Data Model Extension
```rust
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum IntentCaller {
    HumanCli,
    VoiceGateway,
    AgentSession { agent_name: String, session_id: String },
    SystemTimer,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct IntentRecord {
    pub id: String,
    pub caller: IntentCaller,
    pub summary: String,
    pub raw_prompt: Option<String>,
    pub parent_trace_id: Option<String>,
    pub authorization_ref: Option<String>,
    pub timestamp_epoch_seconds: u64,
}
```

- Threaded into:
  - `WorldTransitionPlan.trace_id` and `WorldTransitionPlan.intent`
  - `WorldTransitionReceipt.trace_id` and `WorldTransitionReceipt.intent`
  - `WorldEvent.trace_id` and `WorldEvent.intent_id`

### First Tracer
`task-fabric-svn-work why <ref>`: Walks `trace_id` and causal links backward to print a readable numbered timeline:
```text
Trace trace-1741797600-1234 (World: example-paper)
  [1] INTENT    caller: human_cli ("enter example-paper") at 10:45:12
  [2] PLAN      7 effects staged (plan-1741797600-5678)
  [3] EFFECT    service.ensure (example-paper-ui.service) -> active (port 8787 reconciled)
  [4] EVENT     service.active (example-paper-ui.service)
  [5] RECEIPT   outcome: completed in 42ms (receipt-1741797600-9999)
```

---

## 4. Phase 2 Detailed Specification: The Flight Simulator (Counterfactual Dry-Run)

### Grounding
- `worldstate::plan_for` and `worldstate::snapshot_for`
- Machine-checkable invariants: TCP port occupancy via `ss`, systemd unit status via dbus/CLI, git repository dirtiness, directory existence.

### First Tracer
`task-fabric-svn-work enter <world> --dry-run` and `task-fabric-svn-work action dry-run <world> <action>`: Evaluates preconditions and expected postconditions without mutating desktop state.

---

## 5. Phase 3 Detailed Specification: The Focus Guardian (Attention Economics)

### Grounding
- `crates/task-fabric-svn-work-core/src/attention_alert.rs`
- Compositor window focus from `horizon.rs`
- Sound routing in `task-fabric-svn-soundd`

### Tri-Partite Implementation
1. **Token-Bucket Budget**: 1 interruption per 5 minutes for actionable items, bypass for critical.
2. **Focus-Aware Suppression**: Suppresses non-critical alerts when user is typing in high-flow processes (e.g. Neovim, cargo test).
3. **Spend Ledger**: `attention-spend.jsonl` tracks cost, interruption frequency, and time-to-action.

---

## 6. Phase 4 Detailed Specification: The Friction Compiler & The Tutor

### Grounding
- `EffectOutcome::Degraded` and `EffectOutcome::Failed`
- `exocortex.db` semantic profile & open questions

### First Tracers
- `task-fabric-svn-work improve`: Scans recent receipts for recurring degraded effects and generates actionable TOML diffs for `projects.toml`.
- `task-fabric-svn-work learn drill <world>`: Generates 3 Socratic questions based on real repo failures and past decisions.
