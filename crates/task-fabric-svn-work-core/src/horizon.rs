//! Rebuildable, privacy-first models for the Task Fabric SVN Horizon desktop projection.
//!
//! The browser receives these models through task-fabric-svn-workd.  Compositor details
//! are intentionally kept out of the public projection; only the daemon may
//! resolve opaque ids back to Hyprland targets.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashSet;
use std::time::{SystemTime, UNIX_EPOCH};

pub const SCHEMA: &str = "task_fabric_svn.desktop-projection.v1";
pub const HORIZON_SCHEMA: &str = "task_fabric_svn.horizon-projection.v1";

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct PrivacyConfig {
    pub excluded_classes: Vec<String>,
    pub excluded_titles: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WindowProjection {
    pub id: String,
    pub title: String,
    pub app: String,
    pub workspace_id: i64,
    pub workspace: String,
    pub monitor: i64,
    pub geometry: [i64; 4],
    pub focused: bool,
    pub floating: bool,
    pub grouped: bool,
    pub project_id: Option<String>,
    pub operations: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceProjection {
    pub id: i64,
    pub name: String,
    pub monitor: i64,
    pub focused: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MonitorProjection {
    pub id: i64,
    pub name: String,
    pub width: i64,
    pub height: i64,
    pub focused: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DesktopProjection {
    pub schema: String,
    pub revision: u64,
    pub generated_at: u64,
    pub windows: Vec<WindowProjection>,
    pub workspaces: Vec<WorkspaceProjection>,
    pub monitors: Vec<MonitorProjection>,
    pub hidden_window_count: usize,
    pub privacy: PrivacySummary,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PrivacySummary {
    pub configured_rule_count: usize,
}

fn text(value: Option<&Value>, fallback: &str, limit: usize) -> String {
    let value = value
        .and_then(Value::as_str)
        .unwrap_or(fallback)
        .chars()
        .filter(|c| !c.is_control())
        .take(limit)
        .collect::<String>();
    redact_path(&value)
}

fn redact_path(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    let mut chars = value.chars().peekable();
    while let Some(character) = chars.next() {
        if character == '~' && chars.peek() == Some(&'/') {
            output.push_str("workspace");
            let _ = chars.next();
            while chars.peek().is_some_and(|c| !c.is_whitespace()) {
                let _ = chars.next();
            }
        } else {
            output.push(character);
        }
    }
    output
}

fn matches_pattern(value: &str, patterns: &[String]) -> bool {
    let folded = value.to_ascii_lowercase();
    patterns
        .iter()
        .any(|pattern| folded.contains(&pattern.to_ascii_lowercase()))
}

/// Built-ins are deliberately fail-closed and cannot be disabled by user config.
pub fn is_private(class: &str, title: &str, config: &PrivacyConfig) -> bool {
    let class = class.to_ascii_lowercase();
    let title = title.to_ascii_lowercase();
    [
        "keepass",
        "bitwarden",
        "1password",
        "polkit",
        "authentication",
        "password",
        "pinentry",
        "api key",
        "api keys",
        "secret",
        "credential",
        "token",
    ]
    .iter()
    .any(|needle| class.contains(needle) || title.contains(needle))
        || matches_pattern(&class, &config.excluded_classes)
        || matches_pattern(&title, &config.excluded_titles)
}

pub fn build_desktop_projection(
    clients: &Value,
    workspaces: &Value,
    monitors: &Value,
    active_workspace: &Value,
    revision: u64,
    config: PrivacyConfig,
    ids: &mut std::collections::HashMap<String, String>,
) -> DesktopProjection {
    let mut hidden = 0;
    let mut windows = Vec::new();
    let clients = clients.as_array().cloned().unwrap_or_default();
    for client in clients {
        let class = text(client.get("class"), "unknown", 96);
        let title = text(client.get("title"), "Untitled window", 160);
        if is_private(&class, &title, &config) {
            hidden += 1;
            continue;
        }
        let stable = client
            .get("stableId")
            .and_then(Value::as_i64)
            .map(|id| id.to_string())
            .or_else(|| {
                client
                    .get("address")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            })
            .unwrap_or_else(|| format!("{}:{}", class, title));
        let id = ids
            .entry(stable)
            .or_insert_with(|| format!("horizon-window-{}", opaque_id()))
            .clone();
        let workspace = client.get("workspace").cloned().unwrap_or_default();
        let at = client
            .get("at")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let size = client
            .get("size")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let n =
            |items: &[Value], index: usize| items.get(index).and_then(Value::as_i64).unwrap_or(0);
        windows.push(WindowProjection {
            id,
            title,
            app: class,
            workspace_id: workspace.get("id").and_then(Value::as_i64).unwrap_or(-1),
            workspace: text(workspace.get("name"), "workspace", 96),
            monitor: client.get("monitor").and_then(Value::as_i64).unwrap_or(-1),
            geometry: [n(&at, 0), n(&at, 1), n(&size, 0), n(&size, 1)],
            focused: client
                .get("focusHistoryID")
                .and_then(Value::as_i64)
                .unwrap_or(-1)
                == 0
                || client
                    .get("focusHistoryID")
                    .and_then(Value::as_i64)
                    .is_none()
                    && client
                        .get("visible")
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
            floating: client
                .get("floating")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            grouped: client
                .get("grouped")
                .and_then(Value::as_array)
                .map(|v| !v.is_empty())
                .unwrap_or(false),
            project_id: None,
            operations: vec![
                "focus_window".into(),
                "move_window".into(),
                "gather_semantic_group".into(),
                "create_native_group".into(),
            ],
        });
    }
    let active_workspace_id = active_workspace.get("id").and_then(Value::as_i64);
    let workspaces = workspaces
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .map(|w| WorkspaceProjection {
            id: w.get("id").and_then(Value::as_i64).unwrap_or(-1),
            name: text(w.get("name"), "workspace", 96),
            // Hyprland's JSON uses monitorID. Older releases exposed monitor
            // here, so accept it as a compatibility fallback.
            monitor: w
                .get("monitorID")
                .or_else(|| w.get("monitor"))
                .and_then(Value::as_i64)
                .unwrap_or(-1),
            focused: w.get("id").and_then(Value::as_i64) == active_workspace_id,
        })
        .collect();
    let monitors = monitors
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .map(|m| MonitorProjection {
            id: m.get("id").and_then(Value::as_i64).unwrap_or(-1),
            name: text(m.get("name"), "monitor", 96),
            width: m.get("width").and_then(Value::as_i64).unwrap_or(0),
            height: m.get("height").and_then(Value::as_i64).unwrap_or(0),
            focused: m.get("focused").and_then(Value::as_bool).unwrap_or(false),
        })
        .collect();
    DesktopProjection {
        schema: SCHEMA.into(),
        revision,
        generated_at: now(),
        windows,
        workspaces,
        monitors,
        hidden_window_count: hidden,
        privacy: PrivacySummary {
            configured_rule_count: config.excluded_classes.len() + config.excluded_titles.len(),
        },
    }
}

pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn opaque_id() -> String {
    let mut bytes = [0u8; 16];
    if std::fs::File::open("/dev/urandom")
        .and_then(|mut f| std::io::Read::read_exact(&mut f, &mut bytes))
        .is_err()
    {
        let stamp = now().to_le_bytes();
        bytes[..8].copy_from_slice(&stamp);
    }
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case")]
pub enum Operation {
    FocusWindow {
        target_id: String,
    },
    FocusWorkspace {
        workspace_id: i64,
    },
    MoveWindow {
        target_id: String,
        workspace_id: i64,
    },
    GatherSemanticGroup {
        project_id: String,
        workspace_id: i64,
    },
    CreateNativeGroup {
        target_ids: Vec<String>,
    },
    AddToNativeGroup {
        target_id: String,
        group_target_id: String,
    },
    RemoveFromNativeGroup {
        target_id: String,
    },
    DissolveNativeGroup {
        target_id: String,
    },
    LaunchDeclaredAction {
        project_id: String,
        action_id: String,
    },
    DismissHorizon,
}

impl Operation {
    pub fn mutating(&self) -> bool {
        !matches!(
            self,
            Self::FocusWindow { .. } | Self::FocusWorkspace { .. } | Self::DismissHorizon
        )
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HorizonRequest {
    pub request: String,
    pub instance_nonce: Option<String>,
    pub capability: Option<String>,
    pub revision: Option<u64>,
    pub operation: Option<Operation>,
    pub payload: Option<Value>,
    #[serde(default)]
    pub preview: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArrangementWindow {
    pub app: String,
    pub title: String,
    pub workspace_id: i64,
    pub monitor: i64,
    pub native_group_key: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Arrangement {
    pub id: String,
    pub name: String,
    pub project_id: Option<String>,
    pub windows: Vec<ArrangementWindow>,
    pub created_at: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArrangementEffect {
    pub app: String,
    pub title: String,
    pub outcome: String,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArrangementReceipt {
    pub arrangement_id: String,
    pub created_at: u64,
    pub effects: Vec<ArrangementEffect>,
}

pub fn declared_project_ids(config: &crate::ProjectConfig) -> HashSet<String> {
    config.projects.iter().map(|p| p.id.clone()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn private_surfaces_are_hidden_and_ids_are_opaque() {
        let clients = json!([
            {"stableId": 11, "address":"0xsecret", "class":"org.keepass.KeePass", "title":"Vault"},
            {"stableId": 12, "address":"0xpublic", "class":"org.mozilla.Firefox", "title":"Task Fabric SVN notes", "workspace":{"id":1,"name":"one"}, "at":[0,0],"size":[800,500]}
        ]);
        let mut ids = std::collections::HashMap::new();
        let first = build_desktop_projection(
            &clients,
            &json!([]),
            &json!([]),
            &json!({}),
            1,
            PrivacyConfig::default(),
            &mut ids,
        );
        let second = build_desktop_projection(
            &clients,
            &json!([]),
            &json!([]),
            &json!({}),
            2,
            PrivacyConfig::default(),
            &mut ids,
        );
        assert_eq!(first.hidden_window_count, 1);
        assert_eq!(first.windows.len(), 1);
        assert!(!serde_json::to_string(&first).unwrap().contains("0xpublic"));
        assert_eq!(first.windows[0].id, second.windows[0].id);
    }

    #[test]
    fn configured_title_patterns_fail_closed() {
        let config = PrivacyConfig {
            excluded_titles: vec!["private meeting".into()],
            ..Default::default()
        };
        assert!(is_private(
            "org.example.App",
            "Private Meeting — notes",
            &config
        ));
    }

    #[test]
    fn display_titles_redact_home_paths() {
        let clients = json!([{"stableId":13,"class":"foot","title":"user@host:~/Work/example-project","workspace":{"id":1,"name":"one"}}]);
        let projection = build_desktop_projection(
            &clients,
            &json!([]),
            &json!([]),
            &json!({}),
            1,
            PrivacyConfig::default(),
            &mut std::collections::HashMap::new(),
        );
        assert!(!projection.windows[0].title.contains("private-project"));
        assert!(projection.windows[0].title.contains("workspace"));
    }

    #[test]
    fn workspace_topology_uses_hyprland_monitor_id_and_active_workspace() {
        let projection = build_desktop_projection(
            &json!([]),
            &json!([{"id": 3, "name": "3", "monitorID": 7}]),
            &json!([{"id": 7, "name": "eDP-1", "width": 1920, "height": 1080, "focused": true}]),
            &json!({"id": 3}),
            1,
            PrivacyConfig::default(),
            &mut std::collections::HashMap::new(),
        );
        assert_eq!(projection.workspaces[0].monitor, 7);
        assert!(projection.workspaces[0].focused);
        assert!(projection.monitors[0].focused);
    }
}
