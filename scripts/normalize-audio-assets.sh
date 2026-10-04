#!/usr/bin/env bash
set -euo pipefail

source_dir=${1:-assets/audio}
target_dir=${2:-"$HOME/.local/share/task-fabric-svn/sounds"}
mkdir -p "$target_dir"

shopt -s nullglob
inputs=("$source_dir"/*.oga "$source_dir"/*.ogg "$source_dir"/*.wav)
if ((${#inputs[@]} == 0)); then
  printf 'No audio assets found in %s\n' "$source_dir" >&2
  exit 1
fi

for input in "${inputs[@]}"; do
  name=$(basename "${input%.*}")
  ffmpeg -hide_banner -loglevel error -y -i "$input" \
    -af 'loudnorm=I=-18:LRA=7:TP=-2' -ar 48000 -ac 1 \
    -c:a libopus -b:a 96k "$target_dir/$name.oga"
  printf 'Normalized %s -> %s\n' "$input" "$target_dir/$name.oga"
done
