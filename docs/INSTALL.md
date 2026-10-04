# Task Fabric SVN installation

The installer is deliberately conservative. It is a user-layer installation
for Omarchy; it never writes `/usr/share/omarchy`, other projects' source trees,
or system locations.

## Preview and apply

From this repository:

```sh
./scripts/install.sh
./scripts/install.sh --apply
```

The first command is the default and only prints the planned changes. The
second creates a timestamped backup under
`~/.local/state/task-fabric-svn/backups/YYYYMMDD-HHMMSS/`, then installs:

- `~/.config/task-fabric-svn/projects.toml`
- `~/.config/systemd/user/task-fabric-svn-workd.service`
- `~/.config/systemd/user/task-fabric-svn-experiment-worker.service`
- `~/.local/bin/task-fabric-svn-work` and `~/.local/bin/task-fabric-svn-workd`
- `~/.local/bin/task-fabric-svn-experiment-worker`
- `~/.config/omarchy/plugins/task-fabric-svn.pulse` when the plugin exists in this repo
- the Task Fabric SVN menu entries in `~/.config/omarchy/extensions/omarchy-menu.jsonc`
- the `Super+D` binding in `~/.config/hypr/bindings.lua`
- `task-fabric-svn.pulse` in the left side of Omarchy's bar, after `omarchy.workspaces`
- the existing user-owned `user.notifications` and `user.polkit` plugins,
  routing FreeDesktop and native authentication events through `task-fabric-svn-soundd`
  when those plugins are present

The menu and binding changes are enclosed by a managed marker. Re-running the
installer sees the marker and does not append a duplicate. `shell.json` is
updated with `jq` so unrelated layout and plugin settings remain intact. If
`jq` or `shell.json` is unavailable, the installer leaves the shell layout
unchanged and explains why.

The daemons and experiment worker are intentionally not enabled or started by
the installer. After verification, enable only the service you intend to run
explicitly. For the workstation daemon:

```sh
systemctl --user daemon-reload
systemctl --user enable --now task-fabric-svn-workd.service
systemctl --user status task-fabric-svn-workd.service
```

The experiment worker is a separate supervised evaluation boundary. Start it
only for an approved, isolated trial:

```sh
systemctl --user daemon-reload
systemctl --user enable --now task-fabric-svn-experiment-worker.service
systemctl --user status task-fabric-svn-experiment-worker.service
```

If a local plugin is not ready yet, use `--skip-plugin`; all other user-layer
scaffolding can still be installed.

The Pulse widget's Task Fabric SVN Test summary is advisory. It never blocks project work
or starts Codex automatically. Actions are rendered from each project's
declared configuration, and Resume project displays the daemon's transition
effects before confirmation. “Record signal” writes only one of the fixed,
owner-local reality event types.

## Verification

Before starting the daemon, check the generated files and the backup:

```sh
test -f ~/.config/task-fabric-svn/projects.toml
systemd-analyze --user --no-pager verify ~/.config/systemd/user/task-fabric-svn-workd.service
systemd-analyze --user --no-pager verify ~/.config/systemd/user/task-fabric-svn-experiment-worker.service
grep -n 'TASK-FABRIC-SVN\|task-fabric-svn.pulse' ~/.config/omarchy/extensions/omarchy-menu.jsonc \
  ~/.config/hypr/bindings.lua ~/.config/omarchy/shell.json
```

Then verify the user unit and shell in separate steps:

```sh
systemctl --user is-enabled task-fabric-svn-workd.service || true
systemctl --user is-active task-fabric-svn-workd.service
omarchy restart shell
hyprctl configerrors
```

The final two commands may be unavailable outside a running Omarchy session;
that is a verification limitation, not a reason to edit packaged files.

## Rollback

Every existing target is copied before replacement. Find the backup created by
the install timestamp, inspect it, and restore only the affected target:

```sh
ls -dt ~/.local/state/task-fabric-svn/backups/* | head
systemctl --user disable --now task-fabric-svn-workd.service 2>/dev/null || true
```

Restore files with `cp -a` from the selected backup directory, then reload the
user service and restart the shell. For example:

```sh
backup=~/.local/state/task-fabric-svn/backups/YYYYMMDD-HHMMSS
cp -a "$backup/.config/systemd/user/task-fabric-svn-workd.service" \
  ~/.config/systemd/user/task-fabric-svn-workd.service
cp -a "$backup/.config/omarchy/shell.json" ~/.config/omarchy/shell.json
systemctl --user daemon-reload
omarchy restart shell
```

If a target did not exist before installation, remove only that newly-created
target after confirming its path. Do not remove the whole backup directory;
it is the recovery record for the experiment.
