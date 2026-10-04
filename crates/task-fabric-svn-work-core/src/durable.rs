//! Durable-value scoring, owner-local reality signals, and the read-only
//! Codex metadata adapter used by the workstation status surface.

use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

pub const EVENT_KINDS: &[&str] = &[
    "shipped",
    "deployed",
    "external-user",
    "returning-user",
    "customer-request",
    "payment",
    "feature-deleted",
    "platform-substitution",
    "usage-observed",
];

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RealityEvent {
    pub id: String,
    pub project_id: String,
    pub kind: String,
    pub recorded_at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct DurableMetrics {
    pub abstraction_paths: u32,
    pub service_or_config_paths: u32,
    pub deletion_ratio_percent: u32,
    pub codex_recent_activity: u32,
    pub codex_title_matches: u32,
    pub recent_reality_signals: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DurableValueStatus {
    pub level: String,
    pub score: i32,
    pub summary: String,
    pub reasons: Vec<String>,
    pub metrics: DurableMetrics,
    pub last_evaluated_at: u64,
    pub codex_metadata_available: bool,
    pub recent_positive_signals: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub struct DurableInputs {
    pub changed_paths: Vec<String>,
    pub deleted_lines: u64,
    pub added_lines: u64,
    pub codex_recent_activity: u32,
    pub codex_title_matches: u32,
    pub codex_metadata_available: bool,
    pub reality_events: Vec<RealityEvent>,
    pub now: u64,
}

#[derive(Debug, Clone, Default)]
struct CodexActivity {
    available: bool,
    recent_activity: u32,
    title_matches: u32,
}

pub fn durable_value_for(project: &Project, now: u64) -> DurableValueStatus {
    let (changed_paths, deleted_lines, added_lines) = git_change_inputs(&project.repo);
    let codex = read_agent_activity(&project.repo, now);
    let reality_events = read_events_for(&project.id).unwrap_or_default();
    evaluate(DurableInputs {
        changed_paths,
        deleted_lines,
        added_lines,
        codex_recent_activity: codex.recent_activity,
        codex_title_matches: codex.title_matches,
        codex_metadata_available: codex.available,
        reality_events,
        now,
    })
}

pub fn evaluate(input: DurableInputs) -> DurableValueStatus {
    let now = if input.now == 0 {
        crate::now_epoch_seconds()
    } else {
        input.now
    };
    let abstraction_paths = input
        .changed_paths
        .iter()
        .filter(|path| {
            contains_any(
                path,
                &[
                    "agent",
                    "orchestrat",
                    "router",
                    "scheduler",
                    "registry",
                    "protocol",
                    "memory",
                    "rag",
                    "model",
                    "workflow",
                    "daemon",
                    "architecture",
                    "refactor",
                ],
            )
        })
        .count() as u32;
    let service_or_config_paths = input
        .changed_paths
        .iter()
        .filter(|path| {
            contains_any(
                path,
                &[
                    "service", "systemd", "docker", "compose", "config", "manifest",
                ],
            )
        })
        .count() as u32;
    let deletion_ratio_percent = if input.deleted_lines + input.added_lines == 0 {
        0
    } else {
        ((input.deleted_lines * 100) / (input.deleted_lines + input.added_lines)) as u32
    };
    let recent_events: Vec<&RealityEvent> = input
        .reality_events
        .iter()
        .filter(|event| now.saturating_sub(event.recorded_at) <= 30 * 24 * 60 * 60)
        .collect();
    let positive_events: Vec<&RealityEvent> = recent_events
        .iter()
        .copied()
        .filter(|event| {
            matches!(
                event.kind.as_str(),
                "shipped"
                    | "deployed"
                    | "external-user"
                    | "returning-user"
                    | "customer-request"
                    | "payment"
                    | "feature-deleted"
                    | "platform-substitution"
                    | "usage-observed"
            )
        })
        .collect();

    let mut score = 0i32;
    let mut reasons = Vec::new();
    let mut categories = 0u32;
    if abstraction_paths > 0 {
        score += 2;
        categories += 1;
        reasons.push(format!(
            "{} new abstraction-like path(s)",
            abstraction_paths
        ));
    }
    if service_or_config_paths > 0 {
        score += (service_or_config_paths.min(3)) as i32;
        categories += 1;
        reasons.push(format!(
            "{} service/config path(s) changed",
            service_or_config_paths
        ));
    }
    if abstraction_paths >= 2 {
        score += 2;
        reasons.push("several related AI abstractions changed together".into());
    }
    if input.codex_title_matches > 0 {
        score += 2;
        categories += 1;
        reasons.push(format!(
            "{} recent Codex task title(s) suggest expansion",
            input.codex_title_matches
        ));
    }
    if input.codex_recent_activity >= 3 && positive_events.is_empty() {
        score += 1;
        reasons.push("sustained Codex activity has no recent reality signal".into());
    }
    if deletion_ratio_percent >= 40 {
        score -= 2;
        reasons.push("substantial deletion/replacement is a positive substitution signal".into());
    }
    if !positive_events.is_empty() {
        score -= (positive_events.len().min(3) as i32) * 2;
        reasons.push(format!(
            "{} recent reality signal(s) offset risk",
            positive_events.len()
        ));
    }
    score = score.clamp(0, 12);
    reasons.truncate(6);
    let level = if score >= 7 && categories >= 2 {
        "elevated"
    } else if score >= 4 {
        "notice"
    } else {
        "quiet"
    };
    let summary = match level {
        "elevated" => {
            "Commodity exposure is rising; inspect the durable asset before adding another layer."
                .into()
        }
        "notice" => {
            "Some scope-expansion signals are present; keep the next move tied to a real outcome."
                .into()
        }
        _ if !positive_events.is_empty() => {
            "Reality contact is visible; keep recording what makes this more valuable over time."
                .into()
        }
        _ => "No significant durable-value drift detected.".into(),
    };
    DurableValueStatus {
        level: level.into(),
        score,
        summary,
        reasons,
        metrics: DurableMetrics {
            abstraction_paths,
            service_or_config_paths,
            deletion_ratio_percent,
            codex_recent_activity: input.codex_recent_activity,
            codex_title_matches: input.codex_title_matches,
            recent_reality_signals: positive_events.len() as u32,
        },
        last_evaluated_at: now,
        codex_metadata_available: input.codex_metadata_available,
        recent_positive_signals: positive_events
            .iter()
            .take(4)
            .map(|event| event.kind.clone())
            .collect(),
    }
}

pub fn events_path() -> PathBuf {
    if let Some(path) = std::env::var_os("TASK_FABRIC_SVN_WORK_EVENTS") {
        return PathBuf::from(path);
    }
    if let Some(path) = std::env::var_os("XDG_STATE_HOME") {
        return PathBuf::from(path).join("task-fabric-svn/durable-value-events.jsonl");
    }
    PathBuf::from(std::env::var_os("HOME").unwrap_or_else(|| ".".into()))
        .join(".local/state/task-fabric-svn/durable-value-events.jsonl")
}

pub fn validate_kind(kind: &str) -> bool {
    EVENT_KINDS.contains(&kind)
}

pub fn append_event(project_id: &str, kind: &str, note: Option<&str>) -> io::Result<RealityEvent> {
    if !validate_kind(kind) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "unknown reality signal kind",
        ));
    }
    if project_id.is_empty() || project_id.len() > 128 || project_id.contains(['\n', '\r']) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid project id",
        ));
    }
    if note
        .map(|value| value.chars().count() > 240)
        .unwrap_or(false)
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "note exceeds 240 characters",
        ));
    }
    let now = crate::now_epoch_seconds();
    let event = RealityEvent {
        id: format!(
            "{}-{}-{}",
            now,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|value| value.subsec_nanos())
                .unwrap_or(0)
        ),
        project_id: project_id.into(),
        kind: kind.into(),
        recorded_at: now,
        note: note.filter(|value| !value.is_empty()).map(str::to_string),
    };
    let path = events_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let _lock = EventLock::acquire(&path)?;
    let mut file = open_events(&path)?;
    serde_json::to_writer(&mut file, &event).map_err(io::Error::other)?;
    file.write_all(b"\n")?;
    Ok(event)
}

pub fn read_events_for(project_id: &str) -> io::Result<Vec<RealityEvent>> {
    let path = events_path();
    if !path.exists() {
        return Ok(Vec::new());
    }
    let file = File::open(path)?;
    let mut events = Vec::new();
    for line in BufReader::new(file).lines().flatten() {
        if let Ok(event) = serde_json::from_str::<RealityEvent>(&line) {
            if event.project_id == project_id {
                events.push(event);
            }
        }
    }
    events.sort_by_key(|event| event.recorded_at);
    Ok(events)
}

pub fn remove_event(project_id: &str, event_id: &str) -> io::Result<bool> {
    let path = events_path();
    if !path.exists() {
        return Ok(false);
    }
    let _lock = EventLock::acquire(&path)?;
    let temp = path.with_extension("jsonl.tmp");
    let input = File::open(&path)?;
    let mut output = open_events(&temp)?;
    let mut removed = false;
    for line in BufReader::new(input).lines() {
        let line = line?;
        let matches = serde_json::from_str::<RealityEvent>(&line)
            .map(|event| event.project_id == project_id && event.id == event_id)
            .unwrap_or(false);
        if matches {
            removed = true;
        } else {
            output.write_all(line.as_bytes())?;
            output.write_all(b"\n")?;
        }
    }
    drop(output);
    if removed {
        fs::rename(temp, path)?;
    } else {
        let _ = fs::remove_file(temp);
    }
    Ok(removed)
}

fn open_events(path: &Path) -> io::Result<File> {
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .read(true)
        .open(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }
    Ok(file)
}

struct EventLock {
    path: PathBuf,
}

impl EventLock {
    fn acquire(events: &Path) -> io::Result<Self> {
        let path = events.with_extension("jsonl.lock");
        for _ in 0..100 {
            match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(file) => {
                    drop(file);
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::PermissionsExt;
                        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
                    }
                    return Ok(Self { path });
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(error) => return Err(error),
            }
        }
        Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            "reality signal ledger is busy",
        ))
    }
}

impl Drop for EventLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn git_change_inputs(repo: &Path) -> (Vec<String>, u64, u64) {
    let status = git(repo, &["status", "--porcelain=v1"]).unwrap_or_default();
    let changed_paths = status
        .lines()
        .filter_map(|line| {
            let value = line.get(3..)?.trim();
            Some(value.rsplit(" -> ").next().unwrap_or(value).to_string())
        })
        .collect();
    let numstat = git(repo, &["diff", "--numstat", "HEAD"]).unwrap_or_default();
    let (mut deleted, mut added) = (0, 0);
    for line in numstat.lines() {
        let mut parts = line.split_whitespace();
        if let (Some(a), Some(d)) = (parts.next(), parts.next()) {
            if let Ok(value) = a.parse::<u64>() {
                added += value;
            }
            if let Ok(value) = d.parse::<u64>() {
                deleted += value;
            }
        }
    }
    (changed_paths, deleted, added)
}

fn read_codex_activity(repo: &Path, now: u64) -> CodexActivity {
    let home = std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|value| PathBuf::from(value).join(".codex")));
    let Some(home) = home else {
        return CodexActivity::default();
    };
    let db = fs::read_dir(&home).ok().and_then(|entries| {
        entries
            .flatten()
            .filter_map(|entry| {
                let name = entry.file_name().to_string_lossy().to_string();
                if !(name.starts_with("state_") && name.ends_with(".sqlite")) {
                    return None;
                }
                Some((entry.metadata().ok()?.modified().ok()?, entry.path()))
            })
            .max_by_key(|(modified, _)| *modified)
            .map(|(_, path)| path)
    });
    let Some(db) = db else {
        return CodexActivity::default();
    };
    let query = "SELECT cwd, title, preview, updated_at, tokens_used FROM threads WHERE cwd IS NOT NULL ORDER BY updated_at DESC LIMIT 200;";
    let output = Command::new("sqlite3")
        .args(["-readonly", "-json", db.to_string_lossy().as_ref(), query])
        .output()
        .ok();
    let Some(output) = output.filter(|output| output.status.success()) else {
        return CodexActivity::default();
    };
    let rows: Vec<serde_json::Value> = serde_json::from_slice(&output.stdout).unwrap_or_default();
    let root = fs::canonicalize(repo).unwrap_or_else(|_| repo.to_path_buf());
    let mut result = CodexActivity {
        available: true,
        ..Default::default()
    };
    for row in rows {
        let cwd = row
            .get("cwd")
            .and_then(serde_json::Value::as_str)
            .map(PathBuf::from);
        let Some(cwd) = cwd else { continue };
        let cwd = fs::canonicalize(cwd).unwrap_or_default();
        if !cwd.starts_with(&root) {
            continue;
        }
        let updated = row
            .get("updated_at")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0);
        if now.saturating_sub(updated) <= 14 * 24 * 60 * 60 {
            result.recent_activity += 1;
        }
        let title = format!(
            "{} {}",
            row.get("title")
                .and_then(serde_json::Value::as_str)
                .unwrap_or(""),
            row.get("preview")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("")
        );
        if contains_any(
            &title,
            &[
                "architecture",
                "refactor",
                "agent",
                "orchestrat",
                "memory",
                "router",
                "scheduler",
                "workflow",
                "daemon",
                "model",
            ],
        ) {
            result.title_matches += 1;
        }
    }
    result
}

fn read_agent_activity(repo: &Path, now: u64) -> CodexActivity {
    let mut result = read_codex_activity(repo, now);

    // Ingest OpenCode SQLite database if present
    let home = std::env::var_os("HOME").map(PathBuf::from);
    if let Some(home) = &home {
        let opencode_db = home.join(".local/share/opencode/opencode.db");
        if opencode_db.exists() {
            let query = "SELECT project_directory.directory, session.title, session.time_updated FROM session JOIN project_directory ON session.project_id = project_directory.project_id WHERE session.time_updated IS NOT NULL ORDER BY session.time_updated DESC LIMIT 100;";
            if let Ok(output) = Command::new("sqlite3")
                .args(["-readonly", "-json", opencode_db.to_string_lossy().as_ref(), query])
                .output()
            {
                if output.status.success() {
                    let rows: Vec<serde_json::Value> = serde_json::from_slice(&output.stdout).unwrap_or_default();
                    let root = fs::canonicalize(repo).unwrap_or_else(|_| repo.to_path_buf());
                    for row in rows {
                        let dir = row.get("directory").and_then(serde_json::Value::as_str).map(PathBuf::from);
                        let Some(dir) = dir else { continue };
                        let dir = fs::canonicalize(dir).unwrap_or_default();
                        if !dir.starts_with(&root) && !root.starts_with(&dir) {
                            continue;
                        }
                        result.available = true;
                        let updated_ms = row.get("time_updated").and_then(serde_json::Value::as_u64).unwrap_or(0);
                        let updated_secs = updated_ms / 1000;
                        if now.saturating_sub(updated_secs) <= 14 * 24 * 60 * 60 {
                            result.recent_activity += 1;
                        }
                        let title = row.get("title").and_then(serde_json::Value::as_str).unwrap_or("");
                        if contains_any(title, &["architecture", "refactor", "agent", "orchestrat", "memory", "router", "scheduler", "workflow", "daemon", "model"]) {
                            result.title_matches += 1;
                        }
                    }
                }
            }
        }
    }

    // Ingest Shared Agent Registry ($XDG_RUNTIME_DIR/task-fabric-svn/agents)
    let runtime = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::var_os("HOME")
                .map(|h| PathBuf::from(h).join(".cache"))
                .unwrap_or_else(|| PathBuf::from("/tmp"))
        });
    let agents_dir = runtime.join("task-fabric-svn").join("agents");
    if let Ok(entries) = fs::read_dir(&agents_dir) {
        let root = fs::canonicalize(repo).unwrap_or_else(|_| repo.to_path_buf());
        let repo_name = repo.file_name().and_then(|n| n.to_str()).unwrap_or("");
        for entry in entries.flatten() {
            if entry.path().extension().and_then(|s| s.to_str()) != Some("json") {
                continue;
            }
            if let Ok(content) = fs::read_to_string(entry.path()) {
                if let Ok(val) = serde_json::from_str::<serde_json::Value>(&content) {
                    let ws = val.get("workspace").and_then(|s| s.as_str()).unwrap_or("");
                    let cwd = val.get("cwd").and_then(|s| s.as_str()).map(PathBuf::from);
                    let matches_repo = (!ws.is_empty() && ws.eq_ignore_ascii_case(repo_name))
                        || cwd.as_ref().map(|c| c.starts_with(&root)).unwrap_or(false);
                    if matches_repo {
                        result.available = true;
                        let updated = val.get("updated_at").and_then(|u| u.as_u64()).unwrap_or(0);
                        if now.saturating_sub(updated) <= 14 * 24 * 60 * 60 {
                            result.recent_activity += 1;
                        }
                        let text = format!("{} {} {}",
                            val.get("summary").and_then(|s| s.as_str()).unwrap_or(""),
                            val.get("extra").and_then(|s| s.as_str()).unwrap_or(""),
                            val.get("details").and_then(|s| s.as_str()).unwrap_or(""),
                        );
                        if contains_any(&text, &["architecture", "refactor", "agent", "orchestrat", "memory", "router", "scheduler", "workflow", "daemon", "model"]) {
                            result.title_matches += 1;
                        }
                    }
                }
            }
        }
    }

    result
}

fn git(repo: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
}

fn contains_any(value: &str, needles: &[&str]) -> bool {
    let value = value.to_ascii_lowercase();
    needles.iter().any(|needle| value.contains(needle))
}

use crate::Project;

#[cfg(test)]
mod tests {
    use super::*;

    fn input(paths: &[&str]) -> DurableInputs {
        DurableInputs {
            changed_paths: paths.iter().map(|path| (*path).into()).collect(),
            now: 1_000,
            ..Default::default()
        }
    }

    #[test]
    fn isolated_weak_match_stays_quiet() {
        assert_eq!(evaluate(input(&["src/router.rs"])).level, "quiet");
    }

    #[test]
    fn clustered_abstractions_become_elevated() {
        let mut value = input(&[
            "src/agent_registry.rs",
            "src/workflow_engine.rs",
            "systemd/task-fabric-svn-router.service",
        ]);
        value.codex_title_matches = 2;
        value.codex_recent_activity = 4;
        assert_eq!(evaluate(value).level, "elevated");
    }

    #[test]
    fn reality_signal_offsets_expansion() {
        let mut value = input(&["src/agent_registry.rs", "src/workflow_engine.rs"]);
        value.reality_events = vec![RealityEvent {
            id: "1".into(),
            project_id: "p".into(),
            kind: "external-user".into(),
            recorded_at: 1_000,
            note: None,
        }];
        assert_ne!(evaluate(value).level, "elevated");
    }

    #[test]
    fn deletion_is_a_positive_signal() {
        let mut value = input(&["src/workflow.rs"]);
        value.deleted_lines = 90;
        value.added_lines = 10;
        assert_eq!(evaluate(value).level, "quiet");
    }

    #[test]
    fn signal_kinds_and_note_bound_are_enforced() {
        assert!(validate_kind("payment"));
        assert!(!validate_kind("random"));
        let note = "x".repeat(241);
        assert!(append_event("p", "shipped", Some(&note)).is_err());
    }
}
