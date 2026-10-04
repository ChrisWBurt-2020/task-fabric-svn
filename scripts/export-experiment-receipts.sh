#!/usr/bin/env bash
set -euo pipefail

usage() {
  printf 'Usage: %s --experiment ID --trace TRACE --output-dir ABSOLUTE_DIR\n' "$0" >&2
  exit 2
}

experiment=''
trace=''
output_dir=''
while (($#)); do
  case "$1" in
    --experiment) (($# >= 2)) || usage; experiment=$2; shift 2 ;;
    --trace) (($# >= 2)) || usage; trace=$2; shift 2 ;;
    --output-dir) (($# >= 2)) || usage; output_dir=$2; shift 2 ;;
    *) usage ;;
  esac
done

[[ "$experiment" =~ ^[A-Za-z0-9._-]{1,128}$ && "$trace" =~ ^[A-Za-z0-9._-]{1,256}$ ]] || {
  printf 'experiment and trace must be safe opaque identifiers\n' >&2
  exit 1
}
[[ "$output_dir" = /* ]] || { printf 'output-dir must be absolute\n' >&2; exit 1; }
[[ -n "${NINEROUTER_KEY:-}" ]] || { printf 'NINEROUTER_KEY is required but never written to disk\n' >&2; exit 1; }
command -v curl >/dev/null 2>&1 || { printf 'curl is required\n' >&2; exit 1; }
command -v jq >/dev/null 2>&1 || { printf 'jq is required\n' >&2; exit 1; }

mkdir -p "$output_dir"
raw="$output_dir/router-receipts-$trace.json"
jsonl="$output_dir/router-receipts-$trace.jsonl"
[[ ! -e "$raw" && ! -e "$jsonl" ]] || {
  printf 'refusing to overwrite existing receipt snapshot\n' >&2
  exit 1
}

curl --fail --silent --show-error --max-time 15 \
  -H "Authorization: Bearer $NINEROUTER_KEY" \
  "http://127.0.0.1:20128/api/executions?trace_id=$trace&limit=100" \
  >"$raw"

jq -e --arg experiment "$experiment" --arg trace "$trace" '
  (.trace_id == $trace)
  and (.executions | type == "array" and length > 0)
  and all(.executions[]; (.task_fabric_svn.work_id // null) == $experiment
    and (.task_fabric_svn.trace_id // null) == $trace)
' "$raw" >/dev/null || {
  printf 'receipt response is empty or not fully attributable to the requested experiment/trace\n' >&2
  exit 1
}

jq -c '.executions[]' "$raw" >"$jsonl"
printf 'raw=%s\njsonl=%s\n' "$raw" "$jsonl"
sha256sum "$raw" "$jsonl"
