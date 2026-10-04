//! Stateful alert selection and attention economics for actionable attention items.
//!
//! Delivery is deliberately outside the core model. This module decides
//! when a newly-seen item should be surfaced, manages the token-bucket
//! attention budget, tracks flow-state suppression, and records economic
//! attention spend into `attention-spend.jsonl`.

use crate::{AttentionItem, StatusSnapshot};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

pub const DEFAULT_ALERT_COOLDOWN_SECONDS: u64 = 300;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct ActiveWindowInfo {
    pub class: String,
    pub title: String,
    pub pid: Option<u32>,
    pub fullscreen: bool,
    pub workspace: Option<String>,
}

pub fn query_active_window() -> Option<ActiveWindowInfo> {
    let output = std::process::Command::new("hyprctl")
        .args(["activewindow", "-j"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let val: serde_json::Value = serde_json::from_slice(&output.stdout).ok()?;
    let class = val
        .get("class")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    let title = val
        .get("title")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    let pid = val.get("pid").and_then(|v| v.as_u64()).map(|p| p as u32);
    let fullscreen = val
        .get("fullscreen")
        .and_then(|v| v.as_i64())
        .map(|f| f > 0)
        .unwrap_or(false);
    let workspace = val
        .get("workspace")
        .and_then(|w| w.get("name"))
        .and_then(|n| n.as_str())
        .map(|s| s.to_string());
    if class.is_empty() && title.is_empty() {
        None
    } else {
        Some(ActiveWindowInfo {
            class,
            title,
            pid,
            fullscreen,
            workspace,
        })
    }
}

pub fn detect_flow_state(window: Option<&ActiveWindowInfo>) -> bool {
    let Some(w) = window else { return false };
    if w.class.is_empty() && w.title.is_empty() {
        return false;
    }
    let class = w.class.to_ascii_lowercase();
    let title = w.title.to_ascii_lowercase();

    // Editor, IDE, and high-focus coding environments
    let is_editor = matches!(
        class.as_str(),
        "neovim"
            | "nvim"
            | "vim"
            | "code"
            | "codium"
            | "vscodium"
            | "emacs"
            | "zed"
            | "antigravity"
            | "cursor"
            | "sublime_text"
            | "jetbrains-idea"
            | "clion"
            | "rustrover"
    );

    // Terminal classes
    let is_terminal = matches!(
        class.as_str(),
        "kitty" | "alacritty" | "foot" | "ghostty" | "wezterm" | "xterm"
    ) || class.contains("scratchpad")
        || class.contains("task-fabric-svn");

    // Title keywords indicating active engineering, testing, debugging, or focused editing
    let flow_title_keywords = [
        "nvim", "vim", "nano", "cargo test", "cargo build", "cargo check", "cargo run",
        "git rebase", "git commit", "pytest", "python", "make", "docker", "debug", "gdb",
        "lldb", "repl", "htop", "btop", "tmux", "test suite",
    ];
    let has_flow_title = flow_title_keywords.iter().any(|kw| title.contains(kw));

    is_editor || (is_terminal && (has_flow_title || w.fullscreen))
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AttentionSpendOutcome {
    Delivered,
    SuppressedFlow,
    SuppressedCooldown,
    BaselineQuiet,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AttentionSpendRecord {
    pub id: String,
    pub timestamp_epoch_seconds: u64,
    pub item_id: String,
    pub urgency: String,
    pub severity: String,
    pub cost_tokens: u32,
    pub focused_app: Option<String>,
    pub focused_title: Option<String>,
    pub in_flow_state: bool,
    pub outcome: AttentionSpendOutcome,
    pub summary: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AttentionSpendSummary {
    pub window_hours: u32,
    pub total_records: usize,
    pub total_spend_tokens: u32,
    pub delivered_count: usize,
    pub suppressed_flow_count: usize,
    pub suppressed_cooldown_count: usize,
    pub flow_preservation_rate: f64,
    pub current_window: Option<ActiveWindowInfo>,
    pub in_flow_state: bool,
    pub recent_records: Vec<AttentionSpendRecord>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct AlertState {
    pub initialized: bool,
    pub active: BTreeSet<String>,
    pub pending: BTreeMap<String, AttentionItem>,
    pub last_alert_epoch_seconds: Option<u64>,
    pub flow_suppressed_keys: BTreeSet<String>,
}

impl AlertState {
    /// Observe a snapshot and return items that should be delivered together.
    /// Backward-compatible wrapper for observe_with_focus without active window info.
    pub fn observe(
        &mut self,
        snapshot: &StatusSnapshot,
        now_epoch_seconds: u64,
        cooldown_seconds: u64,
    ) -> Vec<AttentionItem> {
        self.observe_with_focus(snapshot, now_epoch_seconds, cooldown_seconds, None)
            .0
    }

    /// Observe a snapshot taking into account compositor active window and flow state.
    /// Returns unsuppressed items to deliver alongside economic spend records to log.
    pub fn observe_with_focus(
        &mut self,
        snapshot: &StatusSnapshot,
        now_epoch_seconds: u64,
        cooldown_seconds: u64,
        active_window: Option<&ActiveWindowInfo>,
    ) -> (Vec<AttentionItem>, Vec<AttentionSpendRecord>) {
        let project_id = snapshot
            .project
            .as_ref()
            .map(|project| project.id.as_str())
            .unwrap_or("no-project");
        let current: BTreeMap<String, AttentionItem> = snapshot
            .attention
            .iter()
            .filter(|item| is_actionable(item))
            .map(|item| (format!("{project_id}:{}", item.id), item.clone()))
            .collect();
        let current_keys: BTreeSet<String> = current.keys().cloned().collect();

        if !self.initialized {
            self.initialized = true;
            self.active = current_keys;
            return (Vec::new(), Vec::new());
        }

        for key in current_keys.difference(&self.active) {
            if let Some(item) = current.get(key) {
                self.pending.insert(key.clone(), item.clone());
            }
        }
        self.pending.retain(|key, _| current.contains_key(key));
        self.flow_suppressed_keys
            .retain(|key| current.contains_key(key));
        self.active = current_keys;

        if self.pending.is_empty() {
            return (Vec::new(), Vec::new());
        }

        let in_flow = detect_flow_state(active_window);
        let focused_app = active_window.map(|w| w.class.clone());
        let focused_title = active_window.map(|w| w.title.clone());

        let mut to_deliver = Vec::new();
        let mut spend_records = Vec::new();

        // 1. Critical items ALWAYS deliver immediately (bypassing flow state and cooldown)
        let critical_keys: Vec<String> = self
            .pending
            .iter()
            .filter(|(_, item)| is_critical(item))
            .map(|(key, _)| key.clone())
            .collect();

        for key in critical_keys {
            if let Some(item) = self.pending.remove(&key) {
                spend_records.push(AttentionSpendRecord {
                    id: format!("spend-{}-{}", now_epoch_seconds, item.id),
                    timestamp_epoch_seconds: now_epoch_seconds,
                    item_id: item.id.clone(),
                    urgency: item.urgency.clone(),
                    severity: item.severity.clone(),
                    cost_tokens: 5,
                    focused_app: focused_app.clone(),
                    focused_title: focused_title.clone(),
                    in_flow_state: in_flow,
                    outcome: AttentionSpendOutcome::Delivered,
                    summary: item.title.clone(),
                });
                to_deliver.push(item);
                self.flow_suppressed_keys.remove(&key);
            }
        }

        // 2. Non-critical actionable items
        if !self.pending.is_empty() {
            if in_flow {
                // Flow state actively suppresses non-critical interruptions!
                for (key, item) in &self.pending {
                    if !self.flow_suppressed_keys.contains(key) {
                        spend_records.push(AttentionSpendRecord {
                            id: format!("spend-{}-{}", now_epoch_seconds, item.id),
                            timestamp_epoch_seconds: now_epoch_seconds,
                            item_id: item.id.clone(),
                            urgency: item.urgency.clone(),
                            severity: item.severity.clone(),
                            cost_tokens: 0,
                            focused_app: focused_app.clone(),
                            focused_title: focused_title.clone(),
                            in_flow_state: true,
                            outcome: AttentionSpendOutcome::SuppressedFlow,
                            summary: format!("Flow state protected from: {}", item.title),
                        });
                        self.flow_suppressed_keys.insert(key.clone());
                    }
                }
            } else {
                // User is not in flow state. Check token-bucket cooldown budget.
                let cooldown_elapsed = self
                    .last_alert_epoch_seconds
                    .map(|last| now_epoch_seconds.saturating_sub(last) >= cooldown_seconds)
                    .unwrap_or(true);

                if cooldown_elapsed {
                    let keys_to_deliver: Vec<String> = self.pending.keys().cloned().collect();
                    for key in keys_to_deliver {
                        if let Some(item) = self.pending.remove(&key) {
                            spend_records.push(AttentionSpendRecord {
                                id: format!("spend-{}-{}", now_epoch_seconds, item.id),
                                timestamp_epoch_seconds: now_epoch_seconds,
                                item_id: item.id.clone(),
                                urgency: item.urgency.clone(),
                                severity: item.severity.clone(),
                                cost_tokens: 2,
                                focused_app: focused_app.clone(),
                                focused_title: focused_title.clone(),
                                in_flow_state: false,
                                outcome: AttentionSpendOutcome::Delivered,
                                summary: item.title.clone(),
                            });
                            to_deliver.push(item);
                            self.flow_suppressed_keys.remove(&key);
                        }
                    }
                }
            }
        }

        if !to_deliver.is_empty() {
            self.last_alert_epoch_seconds = Some(now_epoch_seconds);
        }

        (to_deliver, spend_records)
    }
}

pub fn default_attention_spend_path() -> PathBuf {
    crate::worldstate::state_dir().join("attention-spend.jsonl")
}

pub fn append_spend_record(record: &AttentionSpendRecord) -> std::io::Result<()> {
    append_spend_record_to_path(&default_attention_spend_path(), record)
}

pub fn append_spend_record_to_path(
    path: &Path,
    record: &AttentionSpendRecord,
) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = file.set_permissions(std::fs::Permissions::from_mode(0o600));
    }
    let bytes = serde_json::to_vec(record).map_err(std::io::Error::other)?;
    use std::io::Write;
    file.write_all(&bytes)?;
    file.write_all(b"\n")
}

pub fn read_spend_records_from_path(path: &Path, limit: usize) -> Vec<AttentionSpendRecord> {
    let content = std::fs::read_to_string(path).unwrap_or_default();
    let mut records: Vec<AttentionSpendRecord> = content
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect();
    records.sort_by(|a, b| b.timestamp_epoch_seconds.cmp(&a.timestamp_epoch_seconds));
    records.truncate(limit);
    records
}

pub fn read_spend_records(limit: usize) -> Vec<AttentionSpendRecord> {
    read_spend_records_from_path(&default_attention_spend_path(), limit)
}

pub fn compile_spend_summary(
    records: &[AttentionSpendRecord],
    hours: u32,
    now_epoch_seconds: u64,
    current_window: Option<ActiveWindowInfo>,
) -> AttentionSpendSummary {
    let cutoff = now_epoch_seconds.saturating_sub((hours as u64) * 3600);
    let filtered: Vec<&AttentionSpendRecord> = records
        .iter()
        .filter(|r| r.timestamp_epoch_seconds >= cutoff)
        .collect();

    let mut total_spend_tokens = 0;
    let mut delivered_count = 0;
    let mut suppressed_flow_count = 0;
    let mut suppressed_cooldown_count = 0;

    for r in &filtered {
        total_spend_tokens += r.cost_tokens;
        match r.outcome {
            AttentionSpendOutcome::Delivered => delivered_count += 1,
            AttentionSpendOutcome::SuppressedFlow => suppressed_flow_count += 1,
            AttentionSpendOutcome::SuppressedCooldown => suppressed_cooldown_count += 1,
            AttentionSpendOutcome::BaselineQuiet => {}
        }
    }

    let total_attempts = delivered_count + suppressed_flow_count;
    let flow_preservation_rate = if total_attempts == 0 {
        100.0
    } else {
        (suppressed_flow_count as f64 / total_attempts as f64) * 100.0
    };

    let in_flow_state = detect_flow_state(current_window.as_ref());

    AttentionSpendSummary {
        window_hours: hours,
        total_records: filtered.len(),
        total_spend_tokens,
        delivered_count,
        suppressed_flow_count,
        suppressed_cooldown_count,
        flow_preservation_rate,
        current_window,
        in_flow_state,
        recent_records: filtered.into_iter().take(20).cloned().collect(),
    }
}

pub fn is_actionable(item: &AttentionItem) -> bool {
    item.urgency == "critical"
        || item.urgency == "actionable"
        || matches!(item.severity.as_str(), "warning" | "error" | "critical")
}

pub fn is_critical(item: &AttentionItem) -> bool {
    item.urgency == "critical" || item.severity == "critical"
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ProjectConfig, StatusSnapshot};
    use std::path::Path;

    fn snapshot(items: &[(&str, &str)]) -> StatusSnapshot {
        let config = ProjectConfig::example_pilot();
        let mut snapshot = StatusSnapshot::local(&config, Path::new("/tmp/projects.toml"), false);
        snapshot.attention = items
            .iter()
            .map(|(id, severity)| {
                let urgency = if *severity == "critical" {
                    "critical"
                } else if *severity == "warning" || *severity == "error" {
                    "actionable"
                } else {
                    "ambient"
                };
                AttentionItem {
                    id: (*id).into(),
                    severity: (*severity).into(),
                    title: format!("Attention {id}"),
                    detail: "detail".into(),
                    urgency: urgency.into(),
                    interference_class: None,
                    source_event_id: None,
                }
            })
            .collect();
        snapshot
    }

    #[test]
    fn initial_actionable_items_establish_a_quiet_baseline() {
        let mut state = AlertState::default();
        assert!(state
            .observe(&snapshot(&[("service-a", "warning")]), 100, 300)
            .is_empty());
    }

    #[test]
    fn new_actionable_item_alerts_once_but_info_never_does() {
        let mut state = AlertState::default();
        state.observe(&snapshot(&[]), 100, 300);
        let current = snapshot(&[("dirty", "info"), ("service-a", "warning")]);
        assert_eq!(state.observe(&current, 101, 300).len(), 1);
        assert!(state.observe(&current, 102, 300).is_empty());
    }

    #[test]
    fn cooldown_queues_new_items_while_they_remain_active() {
        let mut state = AlertState::default();
        state.observe(&snapshot(&[]), 0, 300);
        state.observe(&snapshot(&[("a", "warning")]), 1, 300);
        assert!(state
            .observe(&snapshot(&[("a", "warning"), ("b", "error")]), 2, 300)
            .is_empty());
        let alerts = state.observe(&snapshot(&[("a", "warning"), ("b", "error")]), 301, 300);
        assert_eq!(
            alerts
                .iter()
                .map(|item| item.id.as_str())
                .collect::<Vec<_>>(),
            vec!["b"]
        );
    }

    #[test]
    fn cleared_item_can_alert_if_it_recurs() {
        let mut state = AlertState::default();
        state.observe(&snapshot(&[]), 0, 10);
        state.observe(&snapshot(&[("a", "warning")]), 1, 10);
        state.observe(&snapshot(&[]), 2, 10);
        assert_eq!(
            state.observe(&snapshot(&[("a", "warning")]), 11, 10).len(),
            1
        );
    }

    #[test]
    fn critical_urgency_bypasses_cooldown_immediately() {
        let mut state = AlertState::default();
        state.observe(&snapshot(&[]), 0, 300);
        state.observe(&snapshot(&[("warn-1", "warning")]), 1, 300);
        // During cooldown (at t=10), a critical alert arrives
        let current = snapshot(&[("warn-1", "warning"), ("crit-1", "critical")]);
        let alerts = state.observe(&current, 10, 300);
        assert!(!alerts.is_empty());
        assert!(alerts.iter().any(|item| item.id == "crit-1"));
    }

    #[test]
    fn flow_state_detector_recognizes_editors_and_flow_titles() {
        let nvim_window = ActiveWindowInfo {
            class: "neovim".into(),
            title: "crates/task-fabric-svn-work-core/src/lib.rs".into(),
            pid: Some(1234),
            fullscreen: false,
            workspace: Some("1".into()),
        };
        assert!(detect_flow_state(Some(&nvim_window)));

        let terminal_coding = ActiveWindowInfo {
            class: "alacritty".into(),
            title: "cargo test --all".into(),
            pid: Some(1235),
            fullscreen: false,
            workspace: Some("2".into()),
        };
        assert!(detect_flow_state(Some(&terminal_coding)));

        let browser_window = ActiveWindowInfo {
            class: "chromium".into(),
            title: "Google Search".into(),
            pid: Some(1236),
            fullscreen: false,
            workspace: Some("3".into()),
        };
        assert!(!detect_flow_state(Some(&browser_window)));
        assert!(!detect_flow_state(None));
    }

    #[test]
    fn flow_state_suppresses_actionable_alerts_and_logs_protection() {
        let mut state = AlertState::default();
        state.observe(&snapshot(&[]), 0, 300);

        let flow_window = ActiveWindowInfo {
            class: "neovim".into(),
            title: "main.rs".into(),
            pid: Some(999),
            fullscreen: false,
            workspace: Some("1".into()),
        };

        // User is editing in Neovim when a warning arrives
        let current = snapshot(&[("warn-test", "warning")]);
        let (alerts, spend) =
            state.observe_with_focus(&current, 10, 300, Some(&flow_window));

        // Must be suppressed to protect flow state
        assert!(alerts.is_empty());
        assert_eq!(spend.len(), 1);
        assert_eq!(spend[0].outcome, AttentionSpendOutcome::SuppressedFlow);
        assert_eq!(spend[0].cost_tokens, 0);
        assert!(spend[0].in_flow_state);
        assert_eq!(spend[0].focused_app.as_deref(), Some("neovim"));
    }

    #[test]
    fn critical_urgency_bypasses_flow_state_immediately() {
        let mut state = AlertState::default();
        state.observe(&snapshot(&[]), 0, 300);

        let flow_window = ActiveWindowInfo {
            class: "neovim".into(),
            title: "main.rs".into(),
            pid: Some(999),
            fullscreen: false,
            workspace: Some("1".into()),
        };

        // Critical item arrives during flow state
        let current = snapshot(&[("crit-test", "critical")]);
        let (alerts, spend) =
            state.observe_with_focus(&current, 10, 300, Some(&flow_window));

        assert_eq!(alerts.len(), 1);
        assert_eq!(alerts[0].id, "crit-test");
        assert_eq!(spend.len(), 1);
        assert_eq!(spend[0].outcome, AttentionSpendOutcome::Delivered);
        assert_eq!(spend[0].cost_tokens, 5);
        assert!(spend[0].in_flow_state);
    }

    #[test]
    fn spend_summary_calculates_preservation_rate_and_totals() {
        let records = vec![
            AttentionSpendRecord {
                id: "1".into(),
                timestamp_epoch_seconds: 1000,
                item_id: "crit-1".into(),
                urgency: "critical".into(),
                severity: "critical".into(),
                cost_tokens: 5,
                focused_app: Some("neovim".into()),
                focused_title: Some("main.rs".into()),
                in_flow_state: true,
                outcome: AttentionSpendOutcome::Delivered,
                summary: "Critical service down".into(),
            },
            AttentionSpendRecord {
                id: "2".into(),
                timestamp_epoch_seconds: 1010,
                item_id: "warn-1".into(),
                urgency: "actionable".into(),
                severity: "warning".into(),
                cost_tokens: 0,
                focused_app: Some("neovim".into()),
                focused_title: Some("main.rs".into()),
                in_flow_state: true,
                outcome: AttentionSpendOutcome::SuppressedFlow,
                summary: "Service degraded".into(),
            },
            AttentionSpendRecord {
                id: "3".into(),
                timestamp_epoch_seconds: 1020,
                item_id: "warn-2".into(),
                urgency: "actionable".into(),
                severity: "warning".into(),
                cost_tokens: 0,
                focused_app: Some("neovim".into()),
                focused_title: Some("main.rs".into()),
                in_flow_state: true,
                outcome: AttentionSpendOutcome::SuppressedFlow,
                summary: "Disk warning".into(),
            },
        ];

        let summary = compile_spend_summary(&records, 1, 1030, None);
        assert_eq!(summary.total_records, 3);
        assert_eq!(summary.total_spend_tokens, 5);
        assert_eq!(summary.delivered_count, 1);
        assert_eq!(summary.suppressed_flow_count, 2);
        // 2 suppressed out of 3 total attempts = 66.66%
        assert!((summary.flow_preservation_rate - 66.66).abs() < 1.0);
    }
}

