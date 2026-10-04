#!/usr/bin/env bash
set -euo pipefail

# Install the Task Fabric SVN user layer. This command is intentionally a
# dry-run unless --apply is supplied. It never writes /usr/share/omarchy.

repo_root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
umask 077
home_root=${TASK_FABRIC_SVN_WORK_HOME:-$HOME}
config_home=${XDG_CONFIG_HOME:-"$home_root/.config"}
state_home=${XDG_STATE_HOME:-"$home_root/.local/state"}
omarchy_home="$config_home/omarchy"
backup_parent="$state_home/task-fabric-svn/backups"
stamp=$(date +%Y%m%d-%H%M%S)
backup_root="$backup_parent/$stamp"
apply=0
skip_plugin=0

usage() {
  cat <<'USAGE'
Usage: scripts/install.sh [--apply] [--skip-plugin]

Without --apply, print the exact user-owned changes that would be made.

Options:
  --apply       Create backups and apply the installation.
  --skip-plugin Do not install the optional omarchy/plugins/task-fabric-svn.pulse tree.
  -h, --help    Show this help.
USAGE
}

while (($#)); do
  case "$1" in
    --apply) apply=1 ;;
    --skip-plugin) skip_plugin=1 ;;
    -h|--help) usage; exit 0 ;;
    *) printf 'Unknown option: %s\n' "$1" >&2; usage >&2; exit 2 ;;
  esac
  shift
done

# Two invocations in one second must never reuse a backup directory. This is
# especially important when an installer is being exercised by an agent.
if ((apply)); then
  backup_number=0
  while [[ -e "$backup_root" ]]; do
    backup_number=$((backup_number + 1))
    backup_root="$backup_parent/${stamp}-${backup_number}"
  done
fi

# The example registry is the seed: an existing user registry is never overwritten.
source_projects="$repo_root/examples/projects.toml"
source_unit="$repo_root/systemd/task-fabric-svn-workd.service"
source_sound_unit="$repo_root/systemd/task-fabric-svn-soundd.service"
source_experiment_unit="$repo_root/systemd/task-fabric-svn-experiment-worker.service"
source_audio_config="$repo_root/config/audio.toml"
source_menu="$repo_root/config/omarchy-menu.fragment.jsonc"
source_bindings="$repo_root/config/hypr-bindings.fragment.lua"
source_plugin="$repo_root/adapter/omarchy/plugins/task-fabric-svn.pulse"
source_audio_plugin="$repo_root/adapter/omarchy/plugins/task-fabric-svn.audio"
source_audio_assets="$repo_root/assets/audio"
source_notification_patch="$repo_root/scripts/install-notification-audio.sh"
source_polkit_patch="$repo_root/scripts/install-polkit-audio.sh"
source_kokoro_wrapper="$repo_root/scripts/kokoro-tts.py"
source_kokoro_launcher="$repo_root/scripts/kokoro-tts"
source_hook_post_boot="$repo_root/scripts/omarchy-hooks/task-fabric-svn-post-boot"
source_hook_post_update="$repo_root/scripts/omarchy-hooks/task-fabric-svn-post-update"
source_hook_battery_low="$repo_root/scripts/omarchy-hooks/task-fabric-svn-battery-low"
source_phrases="$repo_root/config/phrases.toml"

target_projects="$config_home/task-fabric-svn/projects.toml"
target_unit="$config_home/systemd/user/task-fabric-svn-workd.service"
target_sound_unit="$config_home/systemd/user/task-fabric-svn-soundd.service"
target_experiment_unit="$config_home/systemd/user/task-fabric-svn-experiment-worker.service"
target_audio_config="$config_home/task-fabric-svn/audio.toml"
target_phrases="$config_home/task-fabric-svn/phrases.toml"
target_bin_dir="$home_root/.local/bin"
target_cli="$target_bin_dir/task-fabric-svn-work"
target_daemon="$target_bin_dir/task-fabric-svn-workd"
target_sound_daemon="$target_bin_dir/task-fabric-svn-soundd"
target_experiment_worker="$target_bin_dir/task-fabric-svn-experiment-worker"
target_kokoro_script="$target_bin_dir/kokoro-tts.py"
target_audio_assets="$home_root/.local/share/task-fabric-svn/sounds"
target_template_dir="$home_root/.local/share/task-fabric-svn/templates"
target_audio_cache="$home_root/.cache/task-fabric-svn"
target_menu="$omarchy_home/extensions/omarchy-menu.jsonc"
target_bindings="$config_home/hypr/bindings.lua"
target_plugin="$omarchy_home/plugins/task-fabric-svn.pulse"
target_audio_plugin="$omarchy_home/plugins/task-fabric-svn.audio"
target_shell="$omarchy_home/shell.json"

active_task_fabric_svn_widget() {
  [[ -f "$target_shell" ]] || return 1
  command -v jq >/dev/null 2>&1 || return 1
  jq -e '
    [.bar.layout // {} | to_entries[]?.value[]?]
    | any(.[]?; (if type == "object" then .id else . end) == "task_fabric_svn")
  ' "$target_shell" >/dev/null 2>&1
}

for required in "$source_projects" "$source_unit" "$source_sound_unit" "$source_experiment_unit" "$source_audio_config" "$source_menu" "$source_bindings" "$source_audio_plugin" "$source_audio_assets" "$source_notification_patch" "$source_polkit_patch" "$source_kokoro_wrapper" "$source_kokoro_launcher" "$source_hook_post_boot" "$source_hook_post_update" "$source_hook_battery_low" "$source_phrases"; do
  [[ -e "$required" ]] || { printf 'Missing repository source: %s\n' "$required" >&2; exit 1; }
done

build_binaries() {
  command -v cargo >/dev/null 2>&1 || {
    printf 'Missing cargo; cannot build the workstation binaries.\n' >&2
    return 1
  }
  MISE_TRUSTED_CONFIG_PATHS="${MISE_TRUSTED_CONFIG_PATHS:-$repo_root}" \
    cargo build --release -p task-fabric-svn-work -p task-fabric-svn-workd -p task-fabric-svn-soundd -p task-fabric-svn-experiment-worker --manifest-path "$repo_root/Cargo.toml"
}

if ((apply)); then
  build_binaries
fi

say() { printf '%s\n' "$*"; }
plan() {
  if ((apply)); then
    "$@"
  else
    printf '+ '
    printf '%q ' "$@"
    printf '\n'
  fi
}

backup_target() {
  local target=$1 relative destination
  [[ -e "$target" || -L "$target" ]] || return 0
  relative=${target#"$home_root"/}
  if [[ "$relative" == "$target" ]]; then
    relative="absolute$(printf '%s' "$target" | tr '/' '_')"
  fi
  destination="$backup_root/$relative"
  if ((apply)); then
    mkdir -p "$(dirname -- "$destination")"
    cp -a -- "$target" "$destination"
    say "Backed up $target -> $destination"
  else
    plan cp -a -- "$target" "$destination"
  fi
}

install_file() {
  local source=$1 target=$2 mode=$3
  backup_target "$target"
  if ((apply)); then
    mkdir -p "$(dirname -- "$target")"
    install -m "$mode" "$source" "$target"
    say "Installed $target"
  else
    plan install -D -m "$mode" "$source" "$target"
  fi
}

install_tree() {
  local source=$1 target=$2
  backup_target "$target"
  if ((apply)); then
    mkdir -p "$target"
    cp -a -- "$source/." "$target/"
    say "Installed $target"
  else
    plan mkdir -p "$target"
    plan cp -a -- "$source/." "$target/"
  fi
}

ensure_audio_policy_keys() {
  local target=$1 temp
  [[ -f "$target" ]] || return 0
  if grep -Eq '^[[:space:]]*speech_profile[[:space:]]*=' "$target" \
    && grep -Eq '^[[:space:]]*template_dir[[:space:]]*=' "$target" \
    && grep -Eq '^[[:space:]]*kokoro_model[[:space:]]*=' "$target" \
    && grep -Eq '^[[:space:]]*kokoro_voices[[:space:]]*=' "$target" \
    && grep -Eq '^[[:space:]]*kokoro_voice[[:space:]]*=' "$target" \
    && grep -Eq '^[[:space:]]*max_speech_per_minute[[:space:]]*=' "$target" \
    && grep -Eq '^[[:space:]]*max_speech_per_10_minutes[[:space:]]*=' "$target" \
    && grep -Eq '^[[:space:]]*media_ducking[[:space:]]*=' "$target" \
    && grep -Eq '^[[:space:]]*media_duck_percent[[:space:]]*=' "$target" \
    && grep -Eq '^[[:space:]]*humor[[:space:]]*=' "$target" \
    && grep -Eq '^[[:space:]]*humor_kind[[:space:]]*=' "$target" \
    && grep -Eq '^[[:space:]]*humor_failure[[:space:]]*=' "$target" \
    && grep -Eq '^[[:space:]]*humor_failure_rate[[:space:]]*=' "$target" \
    && grep -Eq '^[[:space:]]*humor_max_per_10_minutes[[:space:]]*=' "$target" \
    && grep -Eq '^[[:space:]]*phrase_word_cap[[:space:]]*=' "$target" \
    && grep -Eq '^[[:space:]]*speech_read_min_words[[:space:]]*=' "$target" \
    && grep -Eq '^[[:space:]]*speech_read_cap[[:space:]]*=' "$target"; then
    return 0
  fi
  backup_target "$target"
  if (( ! apply )); then
    say "Would add speech-profile and attention-budget defaults to $target"
    return 0
  fi
  temp=$(mktemp)
  cp -a -- "$target" "$temp"
  grep -Eq '^[[:space:]]*speech_profile[[:space:]]=' "$temp" || printf '\nspeech_profile = "kokoro"\n' >>"$temp"
  grep -Eq '^[[:space:]]*template_dir[[:space:]]*=' "$temp" || printf 'template_dir = "%%h/.local/share/task-fabric-svn/templates"\n' >>"$temp"
  grep -Eq '^[[:space:]]*kokoro_model[[:space:]]*=' "$temp" || printf 'kokoro_model = "%%h/.local/share/task-fabric-svn/kokoro/kokoro-v1.0.onnx"\n' >>"$temp"
  grep -Eq '^[[:space:]]*kokoro_voices[[:space:]]*=' "$temp" || printf 'kokoro_voices = "%%h/.local/share/task-fabric-svn/kokoro/voices-v1.0.bin"\n' >>"$temp"
  grep -Eq '^[[:space:]]*kokoro_voice[[:space:]]*=' "$temp" || printf 'kokoro_voice = "af_heart"\n' >>"$temp"
  grep -Eq '^[[:space:]]*max_speech_per_minute[[:space:]]*=' "$temp" || printf 'max_speech_per_minute = 3\n' >>"$temp"
  grep -Eq '^[[:space:]]*max_speech_per_10_minutes[[:space:]]*=' "$temp" || printf 'max_speech_per_10_minutes = 8\n' >>"$temp"
  grep -Eq '^[[:space:]]*media_ducking[[:space:]]*=' "$temp" || printf 'media_ducking = true\n' >>"$temp"
  grep -Eq '^[[:space:]]*media_duck_percent[[:space:]]*=' "$temp" || printf 'media_duck_percent = 35\n' >>"$temp"
  grep -Eq '^[[:space:]]*humor[[:space:]]*=' "$temp" || printf 'humor = "dry"\n' >>"$temp"
  grep -Eq '^[[:space:]]*humor_kind[[:space:]]*=' "$temp" || printf 'humor_kind = "dry"\n' >>"$temp"
  grep -Eq '^[[:space:]]*humor_failure[[:space:]]*=' "$temp" || printf 'humor_failure = false\n' >>"$temp"
  grep -Eq '^[[:space:]]*humor_failure_rate[[:space:]]*=' "$temp" || printf 'humor_failure_rate = 15\n' >>"$temp"
  grep -Eq '^[[:space:]]*humor_max_per_10_minutes[[:space:]]*=' "$temp" || printf 'humor_max_per_10_minutes = 0\n' >>"$temp"
  grep -Eq '^[[:space:]]*phrase_word_cap[[:space:]]*=' "$temp" || printf 'phrase_word_cap = 20\n' >>"$temp"
  grep -Eq '^[[:space:]]*speech_read_min_words[[:space:]]*=' "$temp" || printf 'speech_read_min_words = 0\n' >>"$temp"
  grep -Eq '^[[:space:]]*speech_read_cap[[:space:]]*=' "$temp" || printf 'speech_read_cap = 60\n' >>"$temp"
  install -m 0600 "$temp" "$target"
  rm -f -- "$temp"
  say "Added notification speech policy defaults: $target"
}

install_project_seed() {
  # projects.toml is owner-local registry state.  The repository copy is a
  # seed for first install only; never replace a live registry that may have
  # additional worlds or owner edits.
  if [[ -e "$target_projects" ]]; then
    backup_target "$target_projects"
    say "Preserving existing project registry: $target_projects (repository file is seed-only)"
    return 0
  fi
  install_file "$source_projects" "$target_projects" 0600
}

insert_menu_fragment() {
  local target=$1 temp
  if [[ -f "$target" ]] && grep -Fq 'BEGIN TASK-FABRIC-SVN' "$target"; then
    say "Menu extension already contains Task Fabric SVN entries: $target"
    return 0
  fi
  if (( ! apply )); then
    say "Would insert $source_menu at the top level of $target (idempotent marker)"
    return 0
  fi
  backup_target "$target"
  mkdir -p "$(dirname -- "$target")"
  temp=$(mktemp)
  if [[ -f "$target" ]]; then
    awk -v fragment="$source_menu" '
      BEGIN { inserted = 0 }
      {
        print
        if (!inserted && $0 ~ /^[[:space:]]*\{[[:space:]]*(\/\/.*)?$/) {
          while ((getline line < fragment) > 0) print "  " line
          close(fragment)
          inserted = 1
        }
      }
      END { if (!inserted) exit 1 }
    ' "$target" >"$temp"
  else
    {
      printf '{\n'
      sed 's/^/  /' "$source_menu"
      printf '}\n'
    } >"$temp"
  fi
  install -m 0644 "$temp" "$target"
  rm -f -- "$temp"
  say "Updated menu extension: $target"
}

insert_binding_fragment() {
  local target=$1 temp
  if [[ -f "$target" ]] && grep -Fq 'BEGIN TASK-FABRIC-SVN' "$target"; then
    say "Hyprland binding already contains Task Fabric SVN entry: $target"
    return 0
  fi
  if [[ -f "$target" ]] && grep -Fq 'omarchy menu summon task_fabric_svn' "$target"; then
    say "Hyprland already binds the Task Fabric SVN menu: $target"
    return 0
  fi
  if (( ! apply )); then
    say "Would append $source_bindings to $target (idempotent marker)"
    return 0
  fi
  backup_target "$target"
  mkdir -p "$(dirname -- "$target")"
  temp=$(mktemp)
  if [[ -f "$target" ]]; then
    cp -a -- "$target" "$temp"
    printf '\n' >>"$temp"
    cat "$source_bindings" >>"$temp"
  else
    cp -a -- "$source_bindings" "$temp"
  fi
  install -m 0644 "$temp" "$target"
  rm -f -- "$temp"
  say "Updated Hyprland bindings: $target"
}

say "Task Fabric SVN installer"
say "Repository: $repo_root"
if ((apply)); then
  mkdir -p "$backup_root"
  say "Applying user-owned changes; backup root: $backup_root"
else
  say "DRY RUN: no files will be changed (use --apply to install)"
  say "Planned backup root: $backup_root"
fi

install_project_seed
install_file "$source_unit" "$target_unit" 0644
install_file "$source_sound_unit" "$target_sound_unit" 0644
install_file "$source_experiment_unit" "$target_experiment_unit" 0644
if [[ -e "$target_audio_config" ]]; then
  say "Preserving existing audio policy: $target_audio_config"
  ensure_audio_policy_keys "$target_audio_config"
else
  install_file "$source_audio_config" "$target_audio_config" 0600
fi
# The phrase book is a seed: copy it only when absent, so user overrides are
# never clobbered by a reinstall.
if [[ -e "$target_phrases" ]]; then
  backup_target "$target_phrases"
  say "Preserving existing phrase book: $target_phrases (repository file is seed-only)"
else
  install_file "$source_phrases" "$target_phrases" 0600
fi
if ((apply)); then
  install_file "$repo_root/target/release/task-fabric-svn-work" "$target_cli" 0755
  install_file "$repo_root/target/release/task-fabric-svn-workd" "$target_daemon" 0755
  install_file "$repo_root/target/release/task-fabric-svn-soundd" "$target_sound_daemon" 0755
  install_file "$repo_root/target/release/task-fabric-svn-experiment-worker" "$target_experiment_worker" 0755
  install_file "$source_kokoro_wrapper" "$target_kokoro_script" 0644
  install_file "$source_kokoro_launcher" "$target_bin_dir/kokoro-tts" 0755
  install_tree "$source_audio_assets" "$target_audio_assets"
  mkdir -p "$target_template_dir"
  mkdir -p "$target_audio_cache"
else
  say "Would build and install $target_cli, $target_daemon, and $target_experiment_worker"
fi
if ((skip_plugin)); then
  say "Skipping optional Task Fabric SVN Pulse plugin (--skip-plugin)"
elif [[ -d "$source_plugin" ]]; then
  if active_task_fabric_svn_widget; then
    say "Active task_fabric_svn widget detected; leaving Task Fabric SVN Pulse uninstalled: $target_plugin"
  else
    install_tree "$source_plugin" "$target_plugin"
  fi
else
  say "Optional plugin source is absent; leaving plugin untouched: $source_plugin"
fi
install_tree "$source_audio_plugin" "$target_audio_plugin"
target_notification_service="$omarchy_home/plugins/user.notifications/Service.qml"
if [[ -f "$target_notification_service" ]]; then
  backup_target "$target_notification_service"
  if ((apply)); then
    bash "$source_notification_patch" "$target_notification_service"
  else
    say "Would integrate Task Fabric SVN audio into $target_notification_service"
  fi
fi
target_polkit_service="$omarchy_home/plugins/user.polkit/PolkitAgent.qml"
if [[ -f "$target_polkit_service" ]]; then
  backup_target "$target_polkit_service"
  if ((apply)); then
    bash "$source_polkit_patch" "$target_polkit_service"
  else
    say "Would integrate Task Fabric SVN audio into $target_polkit_service"
  fi
fi

if [[ -f "$target_shell" ]]; then
  if command -v jq >/dev/null 2>&1; then
    if ((apply)); then
      backup_target "$target_shell"
      temp=$(mktemp)
      if active_task_fabric_svn_widget; then
        cp -a -- "$target_shell" "$temp"
        say "Preserving active task_fabric_svn widget; no Task Fabric SVN Pulse entry added"
      else
        jq '
        def plugin_id: if type == "object" then .id else . end;
        .bar.layout.left = (
          (.bar.layout.left // []) as $items
          | if any($items[]?; plugin_id == "task-fabric-svn.pulse") then $items
            else
              (reduce range(0; ($items | length)) as $i
                ([ ]; . + [$items[$i]]
                  + (if ($items[$i] | plugin_id) == "omarchy.workspaces"
                     then [{"id":"task-fabric-svn.pulse"}] else [] end))) as $with
              | if any($items[]?; plugin_id == "omarchy.workspaces")
                then $with else $items + [{"id":"task-fabric-svn.pulse"}] end
            end
        )
        | .bar.layout.right = (
          (.bar.layout.right // []) as $items
          | if any($items[]?; plugin_id == "task-fabric-svn.audio") then $items else [{"id":"task-fabric-svn.audio"}] + $items end
        )
        ' "$target_shell" >"$temp"
      fi
      install -m 0644 "$temp" "$target_shell"
      rm -f -- "$temp"
      say "Checked Omarchy shell layout: $target_shell"
    else
      if active_task_fabric_svn_widget; then
        say "Would preserve active task_fabric_svn widget and skip task-fabric-svn.pulse in $target_shell"
      else
        say "Would insert task-fabric-svn.pulse after omarchy.workspaces in $target_shell"
      fi
    fi
  else
    say "jq is unavailable; shell.json update is skipped (install jq before --apply)"
  fi
else
  say "Omarchy shell config not found; shell layout is unchanged: $target_shell"
fi

insert_menu_fragment "$target_menu"
insert_binding_fragment "$target_bindings"

install_hook() {
  local hook_name=$1 source=$2
  local target="$config_home/omarchy/hooks/$hook_name.d/$(basename "$source").hook"
  backup_target "$target"
  if ((apply)); then
    mkdir -p "$(dirname -- "$target")"
    install -m 0755 "$source" "$target"
    say "Installed Omarchy hook: $target"
  else
    plan install -D -m 0755 "$source" "$target"
  fi
}

install_hook post-boot "$source_hook_post_boot"
install_hook post-update "$source_hook_post_update"
install_hook battery-low "$source_hook_battery_low"

if ((apply)); then
  chmod 0700 "$config_home/task-fabric-svn" 2>/dev/null || true
  chmod 0600 "$target_audio_config" 2>/dev/null || true
  if command -v systemctl >/dev/null 2>&1; then
    systemctl --user daemon-reload || say "Could not reload user systemd; run it manually before enabling task-fabric-svn-workd.service"
  else
    say "systemctl is unavailable; run systemctl --user daemon-reload manually"
  fi
  say "Installed. The daemons are not enabled or started automatically."
  say "Verify first, then run: systemctl --user enable --now task-fabric-svn-workd.service task-fabric-svn-soundd.service"
else
  say "Dry run complete. No user-owned files were modified."
fi
