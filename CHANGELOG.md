# task-fabric-svn changelog

Semantic versioning (`v0.x.y` annotated tags). Newest first.

## v0.10.0 — 2026-09-23
- soundd: parallel notification profiles — `profiles.json` with priority, triggers, per-profile humor and voice rotation (`58f962b`).
- soundd: `--version` flag and version in the startup log.
- Tagged at `304ce88`.

## v0.9.0 — 2026-09-23
- soundd: direct Unix-socket `speak` command for scheduled rituals (used by Memento Mori).
- Tagged at `9dee2b2`.

## v0.8.0 — 2026-09-23
- soundd: `humor_persona` voice injection into generated quips.
- soundd: `dry_run` preview support — returns preview text, speaks nothing, consumes no budget.
- Tagged at `d8c7e70`.

## v0.7.0 — 2026-09-22
- Phase B: bookended local personality for notification speech.
- task-fabric-svn-soundd: mtime-based config auto-reload with validate-then-swap (edits to audio.toml apply without restart).
- Phase A: richer approval speech context.
- Tagged at `7e20fb0`.

## v0.6.0 — 2026-09-21
- Task Fabric: durable Task Contract v1 ledger/state machine.
- `task-fabric-svn-work task` CLI subcommands; OpenCode worker adapter; approval-required handling in the worker.
- task-fabric-svn-context-distiller harvester for mobile snapshot bundles.
- Tagged at `d596e52`.

## v0.5.0 — 2026-09-12
- Attention economics: Focus Guardian flow-state suppression and spend ledger.
- Compounding provenance: Friction Compiler and Socratic Tutor.
- Autonomous worldstate mining and discovery engine.
- Tagged at `7563082`.

## v0.4.0 — 2026-09-12
- The Loom: IntentRecord provenance, trace_id propagation, `task-fabric-svn-work why` CLI.
- Counterfactual dry-run engine (flight simulator) with pre-flight machine checks.
- Tagged at `228462a`.

## v0.3.0 — 2026-09-04
- task-fabric-svn-soundd: per-app Kokoro TTS voice persona routing.
- Classifier: phrase book, humor flavors, and body-read enrichment.
- Per-context voice layers, global voice override; espeak fallback disabled.
- Tagged at `5797094`.

## v0.2.0 — 2026-08-25
- Worldstate typed transition runtime with owner persistence.
- Transition effects idempotent and identifiable; focused Hub windows/route markers.
- Project selection persisted across status surfaces; actionable attention alerts.
- Tagged at `6612d1f`.

## v0.1.0 — 2026-08-24
- Genesis: workstation pulse pilot — Task Fabric SVN project state and resume actions.
- Adaptive local notification audio; semantic attention events.
- Tagged at `b098156`.
