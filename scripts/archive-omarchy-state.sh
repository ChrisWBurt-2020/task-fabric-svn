#!/usr/bin/env bash
set -euo pipefail

# Archive timestamped backups and duplicate plugin trees from ~/.config/omarchy/
# into ~/.local/state/omarchy/archive/<timestamp>/ with an index manifest.

umask 077

TARGET_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/omarchy"
ARCHIVE_BASE="${XDG_STATE_HOME:-$HOME/.local/state}/omarchy/archive"
TIMESTAMP="$(date +%Y%m%d-%H%M%S)"
ARCHIVE_DIR="${ARCHIVE_BASE}/${TIMESTAMP}"

mkdir -p "${ARCHIVE_DIR}"
chmod 0700 "${ARCHIVE_DIR}"

MANIFEST="${ARCHIVE_DIR}/manifest.tsv"
printf "ORIGINAL_PATH\tSHA256\tBYTES\tARCHIVED_NAME\n" > "${MANIFEST}"

echo "Starting Omarchy state cleanup..."
echo "Archive destination: ${ARCHIVE_DIR}"

# 1. Find all *.bak* and *.backup-* files directly inside ~/.config/omarchy/
count=0
while IFS= read -r file; do
    [ -f "${file}" ] || continue
    rel_name="$(basename "${file}")"
    sha="$(sha256sum "${file}" | awk '{print $1}')"
    size="$(stat -c%s "${file}")"
    dest="${ARCHIVE_DIR}/${rel_name}"
    
    printf "%s\t%s\t%s\t%s\n" "${file}" "${sha}" "${size}" "${rel_name}" >> "${MANIFEST}"
    mv "${file}" "${dest}"
    chmod 0600 "${dest}"
    count=$((count + 1))
done < <(find "${TARGET_DIR}" -maxdepth 1 -type f \( -name "*.bak*" -o -name "*.backup-*" \))

echo "Archived ${count} top-level backup files."

# 2. Archive duplicate/backup files inside ~/.config/omarchy/plugins/
plugin_count=0
while IFS= read -r file; do
    [ -f "${file}" ] || continue
    rel_path="${file#${TARGET_DIR}/plugins/}"
    safe_name="plugin-file-$(echo "${rel_path}" | tr '/' '_')"
    sha="$(sha256sum "${file}" | awk '{print $1}')"
    size="$(stat -c%s "${file}")"
    dest="${ARCHIVE_DIR}/${safe_name}"
    
    printf "%s\t%s\t%s\t%s\n" "${file}" "${sha}" "${size}" "${safe_name}" >> "${MANIFEST}"
    mv "${file}" "${dest}"
    chmod 0600 "${dest}"
    plugin_count=$((plugin_count + 1))
done < <(find "${TARGET_DIR}/plugins" -type f \( -name "*.bak*" -o -name "*.backup-*" -o -name "*pre-overlap*" \))

echo "Archived ${plugin_count} backup files from plugins directory."

# 3. Archive obsolete duplicate plugin directory: task_fabric_svn.pre-merge-20260827-205725
if [ -d "${TARGET_DIR}/plugins/task_fabric_svn.pre-merge-20260827-205725" ]; then
    tar_dest="${ARCHIVE_DIR}/task_fabric_svn.pre-merge-20260827-205725.tar.gz"
    tar -czf "${tar_dest}" -C "${TARGET_DIR}/plugins" "task_fabric_svn.pre-merge-20260827-205725"
    sha="$(sha256sum "${tar_dest}" | awk '{print $1}')"
    size="$(stat -c%s "${tar_dest}")"
    printf "%s\t%s\t%s\t%s\n" "${TARGET_DIR}/plugins/task_fabric_svn.pre-merge-20260827-205725" "${sha}" "${size}" "task_fabric_svn.pre-merge-20260827-205725.tar.gz" >> "${MANIFEST}"
    rm -rf "${TARGET_DIR}/plugins/task_fabric_svn.pre-merge-20260827-205725"
    chmod 0600 "${tar_dest}"
    echo "Archived obsolete plugin tree task_fabric_svn.pre-merge-20260827-205725."
fi

chmod 0600 "${MANIFEST}"
echo "Archiving complete. Manifest written to ${MANIFEST}"
