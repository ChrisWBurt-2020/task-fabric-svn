# Omarchy adapter templates

These are repo-side fragments for the user-owned Omarchy layer. They are not
installed automatically and do not edit `/usr/share/omarchy` or any file under
`~/.config` when this repository is built or tested.

Install the plugin into the user-owned directory only after reviewing the
diff:

```bash
install -d "$HOME/.config/omarchy/plugins/task-fabric-svn.pulse"
install -m 0644 adapter/omarchy/plugins/task-fabric-svn.pulse/manifest.json \
  adapter/omarchy/plugins/task-fabric-svn.pulse/BarWidget.qml \
  "$HOME/.config/omarchy/plugins/task-fabric-svn.pulse/"
```

Then merge `shell.fragment.jsonc`, `menu.fragment.jsonc`, and
`hypr.bindings.fragment.lua` into the corresponding user-owned files. Preserve
existing entries and review the resulting diff before reloading Omarchy. The
templates deliberately contain no absolute paths and use only fixed
`task-fabric-svn-work` commands.
