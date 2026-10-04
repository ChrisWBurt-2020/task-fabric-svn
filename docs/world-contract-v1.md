# World Contract v1

The daemon is the authority for World state. A world declaration is desired state: its id is unique, dependencies are explicit, and resources, services, agents, portals, and surfaces are owned by that world. `selected_world_id` is UI intent; `requested_world_id` is an accepted transition target; `active_world_id` is the last verified active world. `world_activation_status` is `active`, `degraded`, `failed`, or `in_progress`.

Observations are facts collected at a timestamp. They include freshness, method, and evidence references. Observed state never implies verified state: verification records the check and level (`declared`, `probed`, `authenticated`, or `reconciled`) and may downgrade activation to degraded when stale or incomplete.

Preflight validates duplicate identifiers, dependency references, and the complete dependency graph (including indirect cycles) before effects are created. A rejected plan produces no effects. Accepted transitions return a transition id and trace id immediately; progress is append-only WorldEvent data keyed by transition and trace ids. Terminal outcomes are active, degraded, or failed and always produce a receipt.

Effects execute in dependency order and carry authority, precondition, target, and expected postcondition. Receipts link intent, approval, plan, every effect result, observations, verification, and final outcome through one trace lineage. Resources have explicit owner world and ownership evidence. Port occupancy alone is a conflict observation; unknown processes are never terminated. Recovery requires an ownership-aware adapter or explicit user action.
