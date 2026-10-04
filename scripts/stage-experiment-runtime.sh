#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

usage() {
  printf 'Usage: %s --candidate PATH --destination PATH [--source-opencode PATH]\n' "$0" >&2
  exit 2
}

candidate=''
destination=''
source_opencode="${XDG_CONFIG_HOME:-${HOME:?}/.config}/opencode"
while (($#)); do
  case "$1" in
    --candidate) (($# >= 2)) || usage; candidate=$2; shift 2 ;;
    --destination) (($# >= 2)) || usage; destination=$2; shift 2 ;;
    --source-opencode) (($# >= 2)) || usage; source_opencode=$2; shift 2 ;;
    *) usage ;;
  esac
done

[[ -n "$candidate" && -n "$destination" ]] || usage
[[ "$candidate" = /* && "$destination" = /* && "$source_opencode" = /* ]] || {
  printf 'candidate, destination, and source-opencode must be absolute paths\n' >&2
  exit 1
}
[[ -d "$candidate/governor" && -d "$candidate/agents" && -d "$candidate/plugins" ]] || {
  printf 'candidate must contain governor/, agents/, and plugins/\n' >&2
  exit 1
}
[[ -f "$candidate/governor/PROTOCOL.md" && -f "$candidate/agents/builder.md" && -f "$candidate/plugins/governor.js" ]] || {
  printf 'candidate is missing the required Governor harness files\n' >&2
  exit 1
}
for role in supervisor reviewer verifier; do
  [[ -f "$source_opencode/agents/$role.md" ]] || {
    printf 'missing controlled external prompt: %s\n' "$source_opencode/agents/$role.md" >&2
    exit 1
  }
done
[[ ! -e "$destination" ]] || {
  printf 'refusing to overwrite existing runtime: %s\n' "$destination" >&2
  exit 1
}

mkdir -p "$destination/agents" "$destination/plugins" "$destination/governor"

# Copy only harness/config material. Credentials, databases, sessions, and
# the source OpenCode config are intentionally excluded.
cp -a "$candidate/governor/." "$destination/governor/"
cp -a "$candidate/plugins/." "$destination/plugins/"
cp -a "$candidate/agents/builder.md" "$destination/agents/builder.md"
for role in supervisor reviewer verifier; do
  cp -a "$source_opencode/agents/$role.md" "$destination/agents/$role.md"
done

# All staged harness text must resolve the staged protocol, never the live
# symlink. This includes help text and CLI defaults that can otherwise send a
# later operation back to the owner runtime.
while IFS= read -r -d '' prompt; do
  tmp=$(mktemp "$destination/.prompt.XXXXXX")
  sed \
    -e "s#\~/.config/opencode/governor#$destination/governor#g" \
    -e "s#\$HOME/.config/opencode/governor#$destination/governor#g" \
    "$prompt" >"$tmp"
  chmod --reference="$prompt" "$tmp"
  mv "$tmp" "$prompt"
done < <(find "$destination/agents" "$destination/governor" "$destination/plugins" -type f -print0)

cp -a "$repo_root/config/experiment-opencode.json" "$destination/opencode.json"

printf 'Staged isolated OpenCode runtime: %s\n' "$destination"
printf 'Candidate harness: %s\n' "$candidate"
printf 'Credential files copied: none\n'
