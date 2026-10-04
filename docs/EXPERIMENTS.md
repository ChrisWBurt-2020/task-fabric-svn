# Isolated Governor experiments

The experiment worker exports `TASK_FABRIC_SVN_WORK_ID` as the experiment identity and
`TASK_FABRIC_SVN_TRACE_ID` as the run identity. Stage an isolated OpenCode runtime with:

```sh
scripts/stage-experiment-runtime.sh \
  --candidate /absolute/path/to/candidate-governor \
  --destination /absolute/path/to/private-xdg/opencode \
  --source-opencode /absolute/path/to/owner-opencode
```

The staging command copies the candidate harness, snapshots the three
controlled external role prompts, rewrites their live Governor paths, and
refuses to overwrite an existing runtime. It excludes credentials, databases,
sessions, and the live OpenCode config. The template contains no credentials;
`NINEROUTER_KEY` must remain an environment-only secret.

The resulting 9router receipts carry `task-fabric-svn.work_id` and `task-fabric-svn.trace_id`.
Export the receipt rows before router retention with the authenticated,
read-only snapshot helper:

```sh
scripts/export-experiment-receipts.sh \
  --experiment governor-cycle-20260915-replay \
  --trace runtime-attributed-trial-20260915-1 \
  --output-dir /absolute/path/to/receipt-snapshot
```

It refuses empty, mixed, or mismatched responses and never writes the router
key. Attach the resulting JSONL with:

```sh
task-fabric-svn-work experiment attach-router-cost <experiment-id> --receipts /absolute/path/receipts.jsonl
```

The importer requires every row to match the experiment identity and to carry
known usage and monetary cost. A mixed, malformed, or unknown-cost export is
rejected without changing the experiment record.

OpenCode event streams are inspectable separately; malformed launcher output is
retained as an evidence-quality signal and never replaces the router receipt:

```sh
task-fabric-svn-work experiment inspect-opencode /absolute/path/events.jsonl
```

Hash the authoritative staged harness separately from OpenCode's generated
dependency/cache directories:

```sh
task-fabric-svn-work experiment baseline runtime-id \
  /absolute/path/to/private-xdg/opencode/agents \
  /absolute/path/to/private-xdg/opencode/governor \
  /absolute/path/to/private-xdg/opencode/plugins \
  /absolute/path/to/private-xdg/opencode/opencode.json
```

Do not point `XDG_CONFIG_HOME` at the live owner configuration for a trial.
The worker's identity variables are reserved and cannot be overridden by a
job's arbitrary environment map.
