# Bounded Improvement Compiler

`task-fabric-svn-work improve` retains the original Friction Compiler report and adds a
typed proposal stage. It reads canonical WorldState events and retained
experiment records, hashes the source records, and writes owner-local
proposals under `$TASK_FABRIC_SVN_WORK_STATE/improvements/` (normally
`~/.local/state/task-fabric-svn/improvements/`).

The proposal is a diagnosis and experiment specification, not a patch. It
identifies the affected world, repository and component, the observed
friction, falsifiable hypothesis, measurable effect, protected invariants,
negative controls, comparison conditions, evaluator, recurrence history,
evidence refs, and the complete experiment/release lineage available so far.

Useful commands:

```text
task-fabric-svn-work improve propose --json
task-fabric-svn-work improve review improvement-governor-attempt-budget-environment-probe
task-fabric-svn-work improve queue-trial <proposal-id>
```

`queue-trial` is a fail-closed handoff to the existing experiment substrate.
It may mark an evidence-complete proposal queued, but it does not execute a
candidate, alter a target repository, approve a release, or promote a canary.
Incomplete proposals print the missing evidence and cannot be queued.

The retained Governor attempt-budget case is recognized as the `governor`
component class and links to its existing experiment record. Repeated
approval-waiting WorldState attention events are recognized separately as the
`workstation-attention` class; because no matched parent/candidate owner-impact
trial is retained, that proposal remains explicitly blocked rather than being
fabricated into an experiment.
