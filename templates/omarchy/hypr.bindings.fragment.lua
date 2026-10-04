-- Merge into ~/.config/hypr/bindings.lua after reviewing the key choice.
-- SUPER+D only opens the static Task Fabric SVN menu; no project data is interpolated
-- into a Hyprland command. Keep the project-aware actions in task-fabric-svn-work.
o.bind("SUPER + D", "Task Fabric SVN Horizon", "task-fabric-svn-work horizon toggle")
