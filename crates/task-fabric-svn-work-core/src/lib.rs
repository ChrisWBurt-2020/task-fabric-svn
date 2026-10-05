//! Shared, dependency-light models and probes for the Task Fabric SVN.
//!
//! This crate deliberately does not write to Omarchy configuration or to a
//! project repository.  It is the read-only foundation used by the CLI and
//! the local status daemon.

use serde::{Deserialize, Serialize};
use std::fmt;
use std::fs;
use std::io;
use std::net::{TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use thiserror::Error;

pub mod attention_alert;
pub mod companion;
pub mod durable;
pub mod experiment;
pub mod improvement;
pub mod horizon;
pub mod notification_audio;
pub mod projections;
pub mod task_ledger;
pub mod world_miner;
pub mod worldstate;
pub use durable::{DurableMetrics, DurableValueStatus, RealityEvent};
pub use task_ledger::{RiskClass, TaskError, TaskEvent, TaskEventKind, TaskLedger, TaskListFilter, TaskRecord, TaskState, TaskSubmitRequest};

pub const CONFIG_RELATIVE_PATH: &str = "task-fabric-svn/projects.toml";
pub const SOCKET_NAME: &str = "task-fabric-svn-workd.sock";
pub const SOCKET_DIRECTORY: &str = "task-fabric-svn";

#[derive(Debug, Error)]
pub enum CoreError {
    #[error("could not read {path}: {source}")]
    Io { path: PathBuf, source: io::Error },
    #[error("could not parse TOML in {path}: {source}")]
    Toml {
        path: PathBuf,
        source: toml::de::Error,
    },
    #[error("configuration has no projects")]
    EmptyConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct ProjectConfig {
    pub version: u32,
    pub projects: Vec<Project>,
}

impl Default for ProjectConfig {
    fn default() -> Self {
        Self {
            version: 1,
            projects: Vec::new(),
        }
    }
}

impl ProjectConfig {
    pub fn from_toml(path: impl AsRef<Path>) -> Result<Self, CoreError> {
        let path = path.as_ref().to_path_buf();
        let text = fs::read_to_string(&path).map_err(|source| CoreError::Io {
            path: path.clone(),
            source,
        })?;
        Self::from_toml_str(&text, path)
    }

    pub fn from_toml_str(text: &str, path: impl AsRef<Path>) -> Result<Self, CoreError> {
        let path = path.as_ref().to_path_buf();
        let mut document: toml::Value =
            toml::from_str(text).map_err(|source| CoreError::Toml {
                path: path.clone(),
                source,
            })?;
        // Accept the more readable `repository` key while keeping `repo` as
        // the canonical typed field.  Normalizing here also permits a config
        // to carry both spellings during a migration without serde reporting
        // a duplicate field.
        if let Some(projects) = document
            .get_mut("projects")
            .and_then(toml::Value::as_array_mut)
        {
            for project in projects {
                if let Some(table) = project.as_table_mut() {
                    if !table.contains_key("repo") {
                        if let Some(repository) = table.get("repository").cloned() {
                            table.insert("repo".into(), repository);
                        }
                    }
                    table.remove("repository");
                }
            }
        }
        let config = document
            .try_into()
            .map_err(|source| CoreError::Toml { path, source })?;
        Ok(config)
    }

    pub fn from_path_or_example(path: impl AsRef<Path>) -> Result<(Self, bool), CoreError> {
        let path = path.as_ref();
        if path.exists() {
            Ok((Self::from_toml(path)?, false))
        } else {
            Ok((Self::example_pilot(), true))
        }
    }

    pub fn example_pilot() -> Self {
        Self {
            version: 1,
            projects: vec![Project {
                id: "example-project".into(),
                label: "Example Project".into(),
                aliases: vec!["example".into(), "auth".into()],
                repo: PathBuf::from("/home/user/Work/example-project"),
                herdr_session: Some("example".into()),
                objective: "An example project showing the registry shape".into(),
                phase: "foundation".into(),
                next_action: "Run the focused workspace checks".into(),
                semantic_profile: None,
                resume_action_id: None,
                agents: vec![
                    AgentConfig {
                        id: "codex".into(),
                        label: "Codex".into(),
                        kind: "herdr".into(),
                        identity: "codex".into(),
                        required: false,
                        owner_world_id: Some("example-project".into()),
                    },
                    AgentConfig {
                        id: "claude".into(),
                        label: "Claude".into(),
                        kind: "herdr".into(),
                        identity: "claude".into(),
                        required: false,
                        owner_world_id: Some("example-project".into()),
                    },
                ],
                services: vec![
                    ServiceConfig::systemd("edged", "task-fabric-svn-edged.service"),
                    ServiceConfig::systemd("runtime", "task-fabric-svn-runtime.service"),
                    ServiceConfig::systemd("hub", "task-fabric-svn-hub.service"),
                ],
                resources: vec![ResourceConfig {
                    id: "hub".into(),
                    kind: "browser".into(),
                    target: "http://127.0.0.1:3840/".into(),
                    surface_kind: None,
                    required: false,
                    owner_world_id: Some("example-project".into()),
                }],
                study_program_refs: Vec::new(),
                study_domain_refs: Vec::new(),
                research_question_refs: Vec::new(),
                actions: vec![
                    ActionConfig::new(
                        "continue",
                        "Continue",
                        &[
                            "omarchy-launch-tui",
                            "--app-id=org.task-fabric-svn.work.example",
                            "herdr",
                            "--session",
                            "example",
                        ],
                    ),
                    ActionConfig::new(
                        "open-repo",
                        "Open repository",
                        &["omarchy-launch-editor", "."],
                    ),
                    ActionConfig::new(
                        "open-hub",
                        "Open Hub",
                        &["omarchy-launch-webapp", "http://127.0.0.1:3840/"],
                    ),
                    ActionConfig::new(
                        "run-tests",
                        "Run workspace tests",
                        &[
                            "omarchy-launch-tui",
                            "--app-id=org.task-fabric-svn.work.tests",
                            "cargo",
                            "test",
                            "--workspace",
                        ],
                    ),
                    ActionConfig::new(
                        "ask-codex",
                        "Ask Codex",
                        &[
                            "omarchy-launch-tui",
                            "--app-id=org.task-fabric-svn.work.codex",
                            "codex",
                        ],
                    ),
                    ActionConfig::new("task-fabric-svn-test", "Run Task Fabric SVN Test", &["codex"]),
                    ActionConfig::new(
                        "focus-agent",
                        "Open Herdr session",
                        &[
                            "omarchy-launch-tui",
                            "--app-id=org.task-fabric-svn.work.example",
                            "herdr",
                            "--session",
                            "example",
                        ],
                    ),
                    ActionConfig::new(
                        "commit-checkpoint",
                        "Review commit checkpoint",
                        &[
                            "omarchy-launch-tui",
                            "--app-id=org.task-fabric-svn.work.checkpoint",
                            "git",
                            "status",
                            "--short",
                            "--branch",
                        ],
                    ),
                ],
                scratchpad: None,
                theme: None,
                resume: Vec::new(),
            }],
        }
    }

    pub fn first_project(&self) -> Option<&Project> {
        self.projects.first()
    }

    pub fn validate(&self) -> Vec<String> {
        let mut issues = Vec::new();
        if self.version != 1 {
            issues.push(format!(
                "unsupported World declaration version {}; expected 1",
                self.version
            ));
        }
        let mut project_names = std::collections::HashSet::new();
        for project in &self.projects {
            if project.id.trim().is_empty() {
                issues.push("project id must not be empty".into());
            }
            let project_key = project.id.to_ascii_lowercase();
            if !project_key.is_empty() && !project_names.insert(project_key) {
                issues.push(format!("duplicate project identifier {}", project.id));
            }
            for alias in &project.aliases {
                let alias_key = alias.to_ascii_lowercase();
                if alias_key.is_empty() {
                    issues.push(format!("{} has an empty alias", project.id));
                } else if !project_names.insert(alias_key) {
                    issues.push(format!("duplicate project identifier or alias {}", alias));
                }
            }
            if project.repo.as_os_str().is_empty() {
                issues.push(format!("{} has an empty repository path", project.id));
            }
            if project.herdr_session.as_deref().unwrap_or("").contains(' ') {
                issues.push(format!(
                    "{} has a Herdr session containing whitespace",
                    project.id
                ));
            }
            if let Some(profile) = &project.semantic_profile {
                let bounded = |value: &str, limit: usize| value.chars().count() <= limit;
                if profile.intention.as_deref().is_some_and(|value| !bounded(value, 20_000)) {
                    issues.push(format!("{} semantic intention exceeds 20000 characters", project.id));
                }
                for value in profile.commitments.iter().chain(profile.open_questions.iter()).chain(profile.hazards.iter()) {
                    if !bounded(value, 2_000) { issues.push(format!("{} semantic profile item exceeds 2000 characters", project.id)); break; }
                }
                for value in profile.known_concepts.iter().chain(profile.explain_concepts.iter()) {
                    if !bounded(value, 256) { issues.push(format!("{} semantic concept exceeds 256 characters", project.id)); break; }
                }
                if profile.commitments.len() > 64 || profile.open_questions.len() > 64 || profile.hazards.len() > 64 || profile.known_concepts.len() > 128 || profile.explain_concepts.len() > 128 {
                    issues.push(format!("{} semantic profile exceeds item bounds", project.id));
                }
            }
            let mut ids = std::collections::HashSet::new();
            for action in &project.actions {
                if !ids.insert(action.id.clone()) {
                    issues.push(format!("{} has duplicate action {}", project.id, action.id));
                }
                if !is_supported_action_risk(&action.risk) {
                    issues.push(format!(
                        "{} action {} has unknown risk {}; expected low or review",
                        project.id, action.id, action.risk
                    ));
                }
                if action.command.is_empty() {
                    issues.push(format!(
                        "{} action {} has no command",
                        project.id, action.id
                    ));
                }
            }
            let effective_resume = project.resume_action_id.as_deref().or_else(|| project.actions.first().map(|action| action.id.as_str()));
            if effective_resume.is_none() {
                issues.push(format!("{} has no declared action for resume", project.id));
            } else if !project.actions.iter().any(|action| Some(action.id.as_str()) == effective_resume) {
                issues.push(format!("{} resume_action_id does not reference a declared action", project.id));
            }
            let mut resume_ids = std::collections::HashSet::new();
            for entry in &project.resume {
                if entry.id.trim().is_empty() || !resume_ids.insert(entry.id.to_ascii_lowercase()) {
                    issues.push(format!("{} has invalid or duplicate resume entry {}", project.id, entry.id));
                }
                if !matches!(entry.kind.as_str(), "agent" | "herdr" | "portal" | "browser" | "surface") {
                    issues.push(format!("{} resume entry {} has unsupported kind {}", project.id, entry.id, entry.kind));
                }
                if entry.owner_world_id.as_deref().is_some_and(|owner| owner != project.id) {
                    issues.push(format!("{} resume entry {} has invalid owner", project.id, entry.id));
                }
                for dependency in &entry.depends_on {
                    if !project.resume.iter().any(|candidate| candidate.id == *dependency) {
                        issues.push(format!("{} resume entry {} depends on unknown entry {}", project.id, entry.id, dependency));
                    }
                }
            }
            for (group, entries) in [
                ("agent", project.agents.iter().map(|entry| (&entry.id, &entry.owner_world_id)).collect::<Vec<_>>()),
                ("service", project.services.iter().map(|entry| (&entry.id, &entry.owner_world_id)).collect::<Vec<_>>()),
                ("resource", project.resources.iter().map(|entry| (&entry.id, &entry.owner_world_id)).collect::<Vec<_>>()),
            ] {
                let mut ids = std::collections::HashSet::new();
                for (id, owner) in entries {
                    if !ids.insert(id.to_ascii_lowercase()) {
                        issues.push(format!("{} has duplicate {} {}", project.id, group, id));
                    }
                    if owner.as_deref().is_some_and(|value| value != project.id) {
                        issues.push(format!("{} {} {} has invalid owner", project.id, group, id));
                    }
                }
            }
            // Validate the complete resume graph before any transition is planned.
            let graph: std::collections::HashMap<_, _> = project.resume.iter().map(|e| (e.id.as_str(), e.depends_on.iter().map(String::as_str).collect::<Vec<_>>())).collect();
            fn visit<'a>(id: &'a str, graph: &std::collections::HashMap<&'a str, Vec<&'a str>>, active: &mut std::collections::HashSet<&'a str>, done: &mut std::collections::HashSet<&'a str>) -> bool {
                if active.contains(id) { return false; }
                if done.contains(id) { return true; }
                active.insert(id);
                for dep in graph.get(id).into_iter().flatten() { if !visit(dep, graph, active, done) { return false; } }
                active.remove(id); done.insert(id); true
            }
            let mut active = std::collections::HashSet::new(); let mut done = std::collections::HashSet::new();
            for id in graph.keys() { if !visit(id, &graph, &mut active, &mut done) { issues.push(format!("{} has a dependency cycle in resume entries", project.id)); break; } }
        }
        issues
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct Project {
    pub id: String,
    pub label: String,
    pub aliases: Vec<String>,
    pub repo: PathBuf,
    pub herdr_session: Option<String>,
    pub objective: String,
    pub phase: String,
    pub next_action: String,
    /// Optional owner-authored semantic profile.  It is never inferred from
    /// model output and is safe to omit from existing project registries.
    #[serde(default)]
    pub semantic_profile: Option<SemanticProfile>,
    /// Explicit resume action; when absent the first declared action is used
    /// as the compatibility default by validation and projections.
    #[serde(default)]
    pub resume_action_id: Option<String>,
    pub agents: Vec<AgentConfig>,
    pub services: Vec<ServiceConfig>,
    pub resources: Vec<ResourceConfig>,
    #[serde(default)]
    pub study_program_refs: Vec<String>,
    #[serde(default)]
    pub study_domain_refs: Vec<String>,
    #[serde(default)]
    pub research_question_refs: Vec<String>,
    pub actions: Vec<ActionConfig>,
    #[serde(default)]
    pub scratchpad: Option<ScratchpadConfig>,
    #[serde(default)]
    pub theme: Option<String>,
    #[serde(default)]
    pub resume: Vec<ResumeEntry>,
}

impl Default for Project {
    fn default() -> Self {
        Self {
            id: String::new(),
            label: String::new(),
            aliases: Vec::new(),
            repo: PathBuf::new(),
            herdr_session: None,
            objective: String::new(),
            phase: String::new(),
            next_action: String::new(),
            semantic_profile: None,
            resume_action_id: None,
            agents: Vec::new(),
            services: Vec::new(),
            resources: Vec::new(),
            study_program_refs: Vec::new(),
            study_domain_refs: Vec::new(),
            research_question_refs: Vec::new(),
            actions: Vec::new(),
            scratchpad: None,
            theme: None,
            resume: Vec::new(),
        }
    }
}

fn default_scratchpad_floating() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(default)]
pub struct ScratchpadConfig {
    pub enabled: bool,
    pub command: Option<String>,
    pub terminal: Option<String>,
    #[serde(default = "default_scratchpad_floating")]
    pub floating: bool,
}

/// A declared surface or dependency to restore when a world is resumed.
/// Entries are data only; the workstation remains the effect boundary.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(default)]
pub struct ResumeEntry {
    pub id: String,
    pub kind: String,
    pub target: Option<String>,
    pub session: Option<String>,
    pub resource: Option<String>,
    pub required: bool,
    pub depends_on: Vec<String>,
    #[serde(default)]
    pub owner_world_id: Option<String>,
}

impl Default for ResumeEntry {
    fn default() -> Self {
        Self { id: String::new(), kind: String::new(), target: None, session: None, resource: None, required: false, depends_on: Vec::new(), owner_world_id: None }
    }
}

impl Default for ScratchpadConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            command: None,
            terminal: None,
            floating: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash, Default)]
#[serde(default)]
pub struct SemanticProfile {
    pub intention: Option<String>,
    pub commitments: Vec<String>,
    pub open_questions: Vec<String>,
    pub known_concepts: Vec<String>,
    pub explain_concepts: Vec<String>,
    pub hazards: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct ServiceConfig {
    pub id: String,
    pub kind: String,
    pub target: String,
    pub unit: Option<String>,
    pub port: Option<u16>,
    #[serde(default)]
    pub required: bool,
    #[serde(default)]
    pub owner_world_id: Option<String>,
}

impl Default for ServiceConfig {
    fn default() -> Self {
        Self {
            id: String::new(),
            kind: "systemd".into(),
            target: String::new(),
            unit: None,
            port: None,
            required: false,
            owner_world_id: None,
        }
    }
}

impl ServiceConfig {
    pub fn systemd(id: &str, unit: &str) -> Self {
        Self {
            id: id.into(),
            kind: "systemd".into(),
            target: unit.into(),
            unit: Some(unit.into()),
            port: None,
            required: false,
            owner_world_id: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct ResourceConfig {
    pub id: String,
    pub kind: String,
    #[serde(alias = "path", alias = "url")]
    pub target: String,
    #[serde(default)]
    pub surface_kind: Option<String>,
    #[serde(default)]
    pub required: bool,
    #[serde(default)]
    pub owner_world_id: Option<String>,
}

impl Default for ResourceConfig {
    fn default() -> Self {
        Self {
            id: String::new(),
            kind: String::new(),
            target: String::new(),
            surface_kind: None,
            required: false,
            owner_world_id: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct ActionConfig {
    pub id: String,
    pub label: String,
    #[serde(alias = "argv")]
    pub command: Vec<String>,
    pub cwd: Option<PathBuf>,
    pub risk: String,
}

/// The action runner intentionally has a small, closed risk vocabulary.
/// `low` actions can run directly; `review` actions require explicit
/// confirmation after showing their exact argv and working directory.
pub fn is_supported_action_risk(risk: &str) -> bool {
    matches!(risk.to_ascii_lowercase().as_str(), "low" | "review")
}

impl Default for ActionConfig {
    fn default() -> Self {
        Self {
            id: String::new(),
            label: String::new(),
            command: Vec::new(),
            cwd: None,
            risk: "review".into(),
        }
    }
}

impl ActionConfig {
    pub fn new(id: &str, label: &str, command: &[&str]) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
            command: command.iter().map(|s| (*s).into()).collect(),
            cwd: None,
            risk: "review".into(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct AgentConfig {
    pub id: String,
    pub label: String,
    pub kind: String,
    pub identity: String,
    #[serde(default)]
    pub required: bool,
    #[serde(default)]
    pub owner_world_id: Option<String>,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            id: String::new(),
            label: String::new(),
            kind: "herdr".into(),
            identity: String::new(),
            required: false,
            owner_world_id: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum StatusSource {
    Daemon,
    LocalFallback,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum Health {
    Active,
    Inactive,
    Degraded,
    Unknown,
}

impl fmt::Display for Health {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Active => "active",
            Self::Inactive => "inactive",
            Self::Degraded => "degraded",
            Self::Unknown => "unknown",
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StatusSnapshot {
    pub schema_version: u32,
    pub generated_at_epoch_seconds: u64,
    pub source: StatusSource,
    pub config_path: String,
    pub using_example_config: bool,
    #[serde(default)]
    pub projects: Vec<ProjectSummary>,
    pub project: Option<ProjectStatus>,
    #[serde(default)]
    pub suggested_project: Option<ProjectSuggestion>,
    pub agents: Vec<AgentStatus>,
    pub services: Vec<ServiceStatus>,
    pub attention: Vec<AttentionItem>,
    pub actions: Vec<ActionSummary>,
    pub degraded: Vec<String>,
    #[serde(default)]
    pub durable_value: Option<DurableValueStatus>,
    /// World orientation is explicit so consumers never have to infer the
    /// active world from the first configured project.
    #[serde(default)]
    pub selected_world_id: Option<String>,
    #[serde(default)]
    pub requested_world_id: Option<String>,
    #[serde(default)]
    pub active_world_id: Option<String>,
    #[serde(default = "default_world_activation_status")]
    pub world_activation_status: String,
    #[serde(default)]
    pub active_trace_id: Option<String>,
    #[serde(default)]
    pub last_transition_id: Option<String>,
}

fn default_world_activation_status() -> String {
    "unknown".into()
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProjectSummary {
    pub id: String,
    pub label: String,
    pub phase: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProjectSuggestion {
    pub id: String,
    pub label: String,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProjectStatus {
    pub id: String,
    pub label: String,
    pub repo: String,
    pub branch: String,
    pub dirty: bool,
    pub ahead: Option<u32>,
    pub behind: Option<u32>,
    pub objective: String,
    pub phase: String,
    pub next_action: String,
    pub herdr_session: Option<String>,
    #[serde(default)]
    pub study_program_refs: Vec<String>,
    #[serde(default)]
    pub study_domain_refs: Vec<String>,
    #[serde(default)]
    pub research_question_refs: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentStatus {
    pub id: String,
    pub label: String,
    pub state: String,
    pub detail: Option<String>,
    #[serde(default)]
    pub raw_state: Option<String>,
    #[serde(default)]
    pub session: Option<String>,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub project_id: Option<String>,
    #[serde(default)]
    pub duration: Option<String>,
    #[serde(default)]
    pub updated_at: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ServiceStatus {
    pub id: String,
    pub kind: String,
    pub target: String,
    pub health: Health,
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AttentionItem {
    pub id: String,
    pub severity: String,
    pub title: String,
    pub detail: String,
    #[serde(default = "default_urgency")]
    pub urgency: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interference_class: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_event_id: Option<String>,
}

fn default_urgency() -> String {
    "ambient".into()
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ActionSummary {
    pub id: String,
    pub label: String,
    pub risk: String,
    #[serde(default)]
    pub command: Vec<String>,
    #[serde(default)]
    pub cwd: String,
}

impl StatusSnapshot {
    pub fn local(config: &ProjectConfig, config_path: &Path, using_example_config: bool) -> Self {
        Self::local_for_project(config, config_path, using_example_config, None)
    }

    /// Build a status snapshot for a named world.  The registry order remains
    /// stable; callers explicitly choose which project is the subject of the
    /// status probe.
    pub fn local_for_project(
        config: &ProjectConfig,
        config_path: &Path,
        using_example_config: bool,
        world_id: Option<&str>,
    ) -> Self {
        let mut degraded = Vec::new();
        let selected_project = world_id.and_then(|id| {
            config.projects.iter().find(|project| {
                project.id.eq_ignore_ascii_case(id)
                    || project.aliases.iter().any(|alias| alias.eq_ignore_ascii_case(id))
            })
        }).or_else(|| config.first_project());
        let project = selected_project.map(|p| project_status(p, &mut degraded));
        let durable_value = selected_project.map(|p| durable::durable_value_for(p, now_epoch_seconds()));
        let services: Vec<ServiceStatus> = selected_project
            .map(|p| p.services.iter().map(probe_service).collect())
            .unwrap_or_default();
        let actions = selected_project
            .map(|p| {
                p.actions
                    .iter()
                    .map(|a| ActionSummary {
                        id: a.id.clone(),
                        label: a.label.clone(),
                        risk: a.risk.clone(),
                        command: a.command.clone(),
                        cwd: a
                            .cwd
                            .as_deref()
                            .unwrap_or(p.repo.as_path())
                            .display()
                            .to_string(),
                    })
                    .collect()
            })
            .unwrap_or_default();
        let configured_agents: Vec<AgentStatus> = selected_project
            .map(|p| {
                p.agents
                    .iter()
                    .map(|a| AgentStatus {
                        id: a.id.clone(),
                        label: a.label.clone(),
                        state: "unknown".into(),
                        detail: Some("Herdr integration is not connected yet".into()),
                        raw_state: None,
                        session: p.herdr_session.clone(),
                        cwd: None,
                        project_id: p.herdr_session.as_ref().map(|_| p.id.clone()),
                        duration: None,
                        updated_at: None,
                    })
                    .collect()
            })
            .unwrap_or_default();
        let herdr_agents = selected_project
            .and_then(|project| project.herdr_session.as_deref())
            .map(|session| probe_herdr_agents(session, config))
            .unwrap_or_default();
        // Runtime agent observations are only attributable to a World that
        // explicitly declares a Herdr session. A local fallback must not
        // manufacture ownership from ambient registry files.
        let registry_agents = if selected_project
            .and_then(|project| project.herdr_session.as_deref())
            .is_some()
        {
            probe_registry_agents(config)
        } else {
            Vec::new()
        };
        let mut combined_agents = registry_agents;
        for ha in herdr_agents {
            if !combined_agents.iter().any(|a| a.id == ha.id || (a.session.is_some() && a.session == ha.session)) {
                combined_agents.push(ha);
            }
        }
        let agents = if !combined_agents.is_empty() {
            combined_agents
        } else if !configured_agents.is_empty() {
            configured_agents
        } else {
            vec![AgentStatus {
                id: "herdr".into(),
                label: "Herdr session state".into(),
                state: "unknown".into(),
                detail: Some("Herdr integration is not connected yet".into()),
                raw_state: None,
                session: selected_project.and_then(|p| p.herdr_session.clone()),
                cwd: None,
                project_id: selected_project
                    .and_then(|p| p.herdr_session.as_ref().map(|_| p.id.clone())),
                duration: None,
                updated_at: None,
            }]
        };
        let mut attention = Vec::new();
        if using_example_config {
            attention.push(AttentionItem {
                id: "config-missing".into(),
                severity: "info".into(),
                title: "Using pilot defaults".into(),
                detail: format!("Create {} to customize projects", config_path.display()),
                urgency: "ambient".into(),
                interference_class: None,
                source_event_id: None,
            });
        }
        if config.projects.is_empty() {
            degraded.push("configuration contains no projects".into());
        }
        for agent in &agents {
            if agent.state == "blocked" || agent.state == "done" || agent.state == "waiting_approval" {
                let severity = if agent.state == "done" { "info" } else { "warning" };
                let (urgency, interference_class) = match agent.state.as_str() {
                    "waiting_approval" => ("actionable", Some("approval".to_string())),
                    "blocked" => ("actionable", Some("blocked".to_string())),
                    _ => ("ambient", Some("completion".to_string())),
                };
                attention.push(AttentionItem {
                    id: format!("agent-{}", agent.id),
                    severity: severity.into(),
                    title: format!("{} is {}", agent.label, agent.state),
                    detail: agent
                        .detail
                        .clone()
                        .unwrap_or_else(|| "Agent reported an actionable state".into()),
                    urgency: urgency.into(),
                    interference_class,
                    source_event_id: agent.session.clone(),
                });
            }
        }
        for service in &services {
            if matches!(service.health, Health::Inactive | Health::Degraded) {
                let urgency = if service.health == Health::Inactive {
                    "actionable"
                } else {
                    "ambient"
                };
                attention.push(AttentionItem {
                    id: format!("service-{}", service.id),
                    severity: "warning".into(),
                    title: format!("Service {} is {}", service.id, service.health),
                    detail: service.detail.clone().unwrap_or_default(),
                    urgency: urgency.into(),
                    interference_class: Some("service".into()),
                    source_event_id: None,
                });
            }
        }
        Self {
            schema_version: 4,
            generated_at_epoch_seconds: now_epoch_seconds(),
            source: StatusSource::LocalFallback,
            config_path: config_path.display().to_string(),
            using_example_config,
            projects: config
                .projects
                .iter()
                .map(|project| ProjectSummary {
                    id: project.id.clone(),
                    label: project.label.clone(),
                    phase: project.phase.clone(),
                })
                .collect(),
            project,
            suggested_project: suggest_project(config, &agents),
            agents,
            services,
            attention,
            actions,
            degraded,
            durable_value,
            selected_world_id: selected_project.map(|p| p.id.clone()),
            requested_world_id: None,
            // Local status is an observation/context fallback, never proof of
            // activation. Only the daemon's persisted WorldState may set this.
            active_world_id: None,
            world_activation_status: "unknown".into(),
            active_trace_id: None,
            last_transition_id: None,
        }
    }

    pub fn with_source(mut self, source: StatusSource) -> Self {
        self.source = source;
        self
    }
}

fn project_status(project: &Project, degraded: &mut Vec<String>) -> ProjectStatus {
    let repo = project.repo.display().to_string();
    let is_git_repo = project.repo.join(".git").exists();
    let branch = git_text(&project.repo, &["branch", "--show-current"]).unwrap_or_else(|| {
        if is_git_repo {
            degraded.push(format!("could not inspect Git repository {}", repo));
            "unknown".into()
        } else {
            "not-a-repository".into()
        }
    });
    let dirty = git_text(&project.repo, &["status", "--porcelain"])
        .map(|s| !s.is_empty())
        .unwrap_or(false);
    let (ahead, behind) = git_ahead_behind(&project.repo);
    ProjectStatus {
        id: project.id.clone(),
        label: project.label.clone(),
        repo,
        branch: if branch.is_empty() {
            "detached".into()
        } else {
            branch
        },
        dirty,
        ahead,
        behind,
        objective: project.objective.clone(),
        phase: project.phase.clone(),
        next_action: project.next_action.clone(),
        herdr_session: project.herdr_session.clone(),
        study_program_refs: project.study_program_refs.clone(),
        study_domain_refs: project.study_domain_refs.clone(),
        research_question_refs: project.research_question_refs.clone(),
    }
}

fn git_text(repo: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn git_ahead_behind(repo: &Path) -> (Option<u32>, Option<u32>) {
    let text = match git_text(
        repo,
        &["rev-list", "--left-right", "--count", "HEAD...@{upstream}"],
    ) {
        Some(s) => s,
        None => return (None, None),
    };
    let mut parts = text
        .split_whitespace()
        .filter_map(|p| p.parse::<u32>().ok());
    (parts.next(), parts.next())
}

fn probe_service(service: &ServiceConfig) -> ServiceStatus {
    let target = if service.target.is_empty() {
        service.unit.clone().unwrap_or_default()
    } else {
        service.target.clone()
    };
    let (health, detail) = match service.kind.as_str() {
        "systemd" | "user-systemd" => probe_systemd(service.unit.as_deref().unwrap_or(&target)),
        "tcp" => probe_tcp(&target, service.port),
        "http" => probe_http(&target),
        _ => (
            Health::Unknown,
            Some(format!("no probe implemented for {}", service.kind)),
        ),
    };
    ServiceStatus {
        id: service.id.clone(),
        kind: service.kind.clone(),
        target,
        health,
        detail,
    }
}

fn probe_systemd(unit: &str) -> (Health, Option<String>) {
    let output = Command::new("systemctl")
        .args(["--user", "is-active", unit])
        .output();
    match output {
        Ok(output) if output.status.success() => (
            Health::Active,
            Some(String::from_utf8_lossy(&output.stdout).trim().into()),
        ),
        Ok(output) => {
            let state = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if state == "inactive" || state == "failed" || state == "deactivating" {
                (
                    Health::Inactive,
                    Some(if state.is_empty() {
                        "inactive".into()
                    } else {
                        state
                    }),
                )
            } else {
                (
                    Health::Unknown,
                    Some(if state.is_empty() {
                        "systemd did not return a state".into()
                    } else {
                        state
                    }),
                )
            }
        }
        Err(error) => (
            Health::Unknown,
            Some(format!("systemctl unavailable: {}", error)),
        ),
    }
}

fn probe_tcp(target: &str, port: Option<u16>) -> (Health, Option<String>) {
    let address = match port {
        Some(port) => format!("{}:{}", target, port),
        None => target.to_string(),
    };
    match address
        .to_socket_addrs()
        .ok()
        .and_then(|mut addresses| addresses.next())
        .and_then(|addr| TcpStream::connect_timeout(&addr, Duration::from_millis(250)).ok())
    {
        Some(_) => (Health::Active, None),
        None => (
            Health::Inactive,
            Some(format!("could not connect to {}", address)),
        ),
    }
}

fn probe_http(target: &str) -> (Health, Option<String>) {
    // Keep the first slice dependency-light: parse the URL and use a TCP
    // connect as a liveness check. Full HTTP response checks come later.
    let without_scheme = target
        .strip_prefix("http://")
        .or_else(|| target.strip_prefix("https://"))
        .unwrap_or(target);
    let host_port = without_scheme.split('/').next().unwrap_or(without_scheme);
    probe_tcp(host_port, None)
}

fn normalized_agent_state(raw: &str) -> &'static str {
    match raw.to_ascii_lowercase().as_str() {
        "blocked" | "waiting" | "approval" | "question" | "error" | "failed" => "blocked",
        "done" | "completed" | "ready" | "finished" => "done",
        "working" | "running" | "active" | "busy" | "streaming" | "pending" | "retry" => "working",
        "idle" => "idle",
        _ => "unknown",
    }
}

fn project_for_cwd<'a>(config: &'a ProjectConfig, cwd: &str) -> Option<&'a Project> {
    let cwd = Path::new(cwd)
        .canonicalize()
        .unwrap_or_else(|_| PathBuf::from(cwd));
    config
        .projects
        .iter()
        .filter_map(|project| {
            let repo = project.repo.canonicalize().ok()?;
            cwd.starts_with(&repo)
                .then_some((repo.components().count(), project))
        })
        .max_by_key(|(depth, _)| *depth)
        .map(|(_, project)| project)
}

fn suggest_project(config: &ProjectConfig, agents: &[AgentStatus]) -> Option<ProjectSuggestion> {
    agents.iter().find_map(|agent| {
        let cwd = agent.cwd.as_deref()?;
        let project = project_for_cwd(config, cwd)?;
        Some(ProjectSuggestion {
            id: project.id.clone(),
            label: project.label.clone(),
            reason: format!("Herdr agent cwd is {}", cwd),
        })
    })
}

fn probe_herdr_agents(session: &str, config: &ProjectConfig) -> Vec<AgentStatus> {
    let output = match Command::new("herdr")
        .args(["--session", session, "api", "snapshot"])
        .output()
    {
        Ok(output) if output.status.success() => output,
        _ => return Vec::new(),
    };
    let value: serde_json::Value = match serde_json::from_slice(&output.stdout) {
        Ok(value) => value,
        Err(_) => return Vec::new(),
    };
    let payload = value
        .get("result")
        .and_then(|result| result.get("snapshot"))
        .or_else(|| value.get("snapshot"))
        .unwrap_or(&value);
    payload
        .get("agents")
        .and_then(serde_json::Value::as_array)
        .map(|agents| {
            agents
                .iter()
                .filter_map(|agent| {
                    let id = agent
                        .get("name")
                        .or_else(|| agent.get("agent"))
                        .and_then(serde_json::Value::as_str)?;
                    let label = agent
                        .get("display_agent")
                        .or_else(|| agent.get("title"))
                        .or_else(|| agent.get("agent"))
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or(id);
                    let raw_state = agent
                        .get("agent_status")
                        .or_else(|| agent.get("status"))
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("unknown")
                        .to_string();
                    let cwd = agent
                        .get("cwd")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_string);
                    let matched_project =
                        cwd.as_deref().and_then(|cwd| project_for_cwd(config, cwd));
                    Some(AgentStatus {
                        id: id.to_string(),
                        label: label.to_string(),
                        state: normalized_agent_state(&raw_state).into(),
                        detail: cwd.clone(),
                        raw_state: Some(raw_state),
                        session: Some(session.to_string()),
                        cwd,
                        project_id: matched_project.map(|p| p.id.clone()),
                        duration: None,
                        updated_at: None,
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

pub fn probe_registry_agents(config: &ProjectConfig) -> Vec<AgentStatus> {
    let runtime = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::var_os("HOME")
                .map(|h| PathBuf::from(h).join(".cache"))
                .unwrap_or_else(|| PathBuf::from("/tmp"))
        });
    let agents_dir = runtime.join("task-fabric-svn").join("agents");
    let entries = match std::fs::read_dir(&agents_dir) {
        Ok(entries) => entries,
        Err(_) => return Vec::new(),
    };
    let now = now_epoch_seconds();
    let mut result = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        let content = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(_) => continue,
        };
        let val: serde_json::Value = match serde_json::from_str(&content) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let updated_at = val.get("updated_at").and_then(|u| u.as_u64()).unwrap_or(0);
        // Exclude entries older than 2 hours (7200s)
        if now.saturating_sub(updated_at) > 7200 {
            continue;
        }
        let id = val.get("id").and_then(|s| s.as_str()).unwrap_or("agent");
        let app = val.get("app").and_then(|s| s.as_str()).unwrap_or("Agent");
        let raw_state = val.get("state").and_then(|s| s.as_str()).unwrap_or("idle");
        let workspace = val.get("workspace").and_then(|s| s.as_str()).unwrap_or("");
        let extra = val.get("extra").and_then(|s| s.as_str());
        let details = val.get("details").and_then(|s| s.as_str());
        let duration = val.get("duration").and_then(|s| s.as_str()).map(str::to_string);
        let session = val.get("session_id").and_then(|s| s.as_str()).map(str::to_string);
        let cwd = val.get("cwd").and_then(|s| s.as_str()).map(str::to_string);

        let normalized_state = match raw_state {
            "waiting_approval" | "approval" | "input" => "waiting_approval",
            "running" | "active" => "running",
            "error" | "failed" | "blocked" => "blocked",
            _ => "idle",
        };

        let matched_project = config.projects.iter().find(|p| {
            p.id.eq_ignore_ascii_case(workspace)
                || p.repo.file_name().and_then(|n| n.to_str()).map(|n| n.eq_ignore_ascii_case(workspace)).unwrap_or(false)
                || cwd.as_deref().map(|c| Path::new(c).starts_with(&p.repo)).unwrap_or(false)
        });

        let detail_text = if let Some(d) = details.filter(|s| !s.trim().is_empty()) {
            Some(d.to_string())
        } else if let Some(e) = extra.filter(|s| !s.trim().is_empty()) {
            Some(e.to_string())
        } else if !workspace.is_empty() {
            Some(workspace.to_string())
        } else {
            cwd.clone()
        };

        let label = if !workspace.is_empty() {
            format!("{app} ({workspace})")
        } else {
            app.to_string()
        };

        result.push(AgentStatus {
            id: id.to_string(),
            label,
            state: normalized_state.to_string(),
            detail: detail_text,
            raw_state: Some(raw_state.to_string()),
            session,
            cwd,
            project_id: matched_project.map(|p| p.id.clone()),
            duration,
            updated_at: Some(updated_at),
        });
    }
    result
}

pub fn default_config_path() -> PathBuf {
    if let Some(path) = std::env::var_os("TASK_FABRIC_SVN_WORK_CONFIG") {
        return PathBuf::from(path);
    }
    if let Some(path) = std::env::var_os("XDG_CONFIG_HOME") {
        return PathBuf::from(path).join(CONFIG_RELATIVE_PATH);
    }
    if let Some(home) = std::env::var_os("HOME") {
        return PathBuf::from(home)
            .join(".config")
            .join(CONFIG_RELATIVE_PATH);
    }
    PathBuf::from(CONFIG_RELATIVE_PATH)
}

pub fn default_socket_path() -> PathBuf {
    if let Some(path) = std::env::var_os("TASK_FABRIC_SVN_WORK_SOCKET") {
        return PathBuf::from(path);
    }
    if let Some(runtime) = std::env::var_os("XDG_RUNTIME_DIR") {
        return PathBuf::from(runtime)
            .join(SOCKET_DIRECTORY)
            .join("daemon.sock");
    }
    PathBuf::from("/tmp").join(format!("{}-{}", SOCKET_NAME, std::process::id()))
}

pub fn now_epoch_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn herdr_states_are_normalized_for_attention_projection() {
        assert_eq!(normalized_agent_state("approval"), "blocked");
        assert_eq!(normalized_agent_state("streaming"), "working");
        assert_eq!(normalized_agent_state("completed"), "done");
        assert_eq!(normalized_agent_state("idle"), "idle");
        assert_eq!(normalized_agent_state("future-state"), "unknown");
    }

    #[test]
    fn herdr_cwd_suggests_the_deepest_registered_project() {
        // project_for_cwd canonicalizes repo paths, so this test needs real
        // directories on disk instead of placeholder paths.
        let base = std::env::temp_dir().join("task-fabric-svn-suggest-test");
        let outer_repo = base.join("example-project");
        let nested_repo = outer_repo.join("companion");
        let agent_cwd = nested_repo.join("android");
        std::fs::create_dir_all(&agent_cwd).expect("test dirs");
        let mut config = ProjectConfig::example_pilot();
        config.projects[0].repo = outer_repo.clone();
        config.projects.push(Project {
            id: "nested".into(),
            label: "Nested project".into(),
            aliases: Vec::new(),
            repo: nested_repo.clone(),
            herdr_session: Some("nested".into()),
            objective: String::new(),
            phase: String::new(),
            next_action: String::new(),
            semantic_profile: None,
            resume_action_id: None,
            agents: Vec::new(),
            services: Vec::new(),
            resources: Vec::new(),
            study_program_refs: Vec::new(),
            study_domain_refs: Vec::new(),
            research_question_refs: Vec::new(),
            actions: Vec::new(),
            scratchpad: None,
            theme: None,
            resume: Vec::new(),
        });
        let agent = AgentStatus {
            id: "codex".into(),
            label: "Codex".into(),
            state: "working".into(),
            detail: None,
            raw_state: Some("streaming".into()),
            session: Some("nested".into()),
            cwd: Some(agent_cwd.to_string_lossy().into_owned()),
            project_id: Some("nested".into()),
            duration: None,
            updated_at: None,
        };
        let suggestion = suggest_project(&config, &[agent]).expect("project suggestion");
        assert_eq!(suggestion.id, "nested");
        assert!(suggestion.reason.contains("Herdr agent cwd"));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn pilot_has_example_project_and_expected_actions() {
        let config = ProjectConfig::example_pilot();
        let project = config.first_project().expect("pilot project");
        assert_eq!(project.id, "example-project");
        assert_eq!(project.herdr_session.as_deref(), Some("example"));
        assert!(project
            .actions
            .iter()
            .any(|action| action.id == "run-tests"));
    }

    #[test]
    fn example_toml_round_trips() {
        let config = ProjectConfig::example_pilot();
        let text = toml::to_string(&config).expect("serialize config");
        let parsed: ProjectConfig = toml::from_str(&text).expect("parse config");
        assert_eq!(parsed, config);
    }

    #[test]
    fn missing_config_uses_example() {
        let path = std::env::temp_dir().join(format!("task-fabric-svn-work-missing-{}", std::process::id()));
        let (config, example) = ProjectConfig::from_path_or_example(&path).expect("fallback");
        assert!(example);
        assert_eq!(
            config.first_project().map(|p| p.id.as_str()),
            Some("example-project")
        );
    }

    #[test]
    fn config_validation_catches_empty_commands() {
        let mut config = ProjectConfig::example_pilot();
        config.projects[0].actions.push(ActionConfig::default());
        let issues = config.validate();
        assert!(issues.iter().any(|issue| issue.contains("no command")));
    }

    #[test]
    fn config_validation_rejects_unknown_action_risk() {
        let mut config = ProjectConfig::example_pilot();
        config.projects[0].actions[0].risk = "dangerous".into();
        let issues = config.validate();
        assert!(issues
            .iter()
            .any(|issue| issue.contains("unknown risk dangerous")));
    }

    #[test]
    fn config_validation_rejects_incompatible_world_contract_version() {
        let mut config = ProjectConfig::example_pilot();
        config.version = 2;
        let issues = config.validate();
        assert!(issues.iter().any(|issue| issue.contains("expected 1")));
    }

    #[test]
    fn config_validation_rejects_invalid_declaration_ownership() {
        let mut config = ProjectConfig::example_pilot();
        config.projects[0].resources[0].owner_world_id = Some("other-world".into());
        let issues = config.validate();
        assert!(issues.iter().any(|issue| issue.contains("invalid owner")));
    }

    #[test]
    fn config_validation_rejects_duplicate_project_identifiers_and_aliases() {
        let mut config = ProjectConfig::example_pilot();
        config.projects.push(Project {
            id: "other".into(),
            label: "Other".into(),
            aliases: vec!["EXAMPLE".into()],
            repo: PathBuf::from("/tmp/other"),
            ..Project::default()
        });
        let issues = config.validate();
        assert!(issues
            .iter()
            .any(|issue| issue.contains("duplicate project identifier or alias EXAMPLE")));
    }

    #[test]
    fn action_risk_vocabulary_is_closed() {
        assert!(is_supported_action_risk("low"));
        assert!(is_supported_action_risk("REVIEW"));
        assert!(!is_supported_action_risk("high"));
        assert!(!is_supported_action_risk(""));
    }

    #[test]
    fn local_status_is_json_serializable() {
        let config = ProjectConfig::example_pilot();
        let status = StatusSnapshot::local(&config, Path::new("/tmp/projects.toml"), true);
        let json = serde_json::to_string(&status).expect("json");
        assert!(json.contains("example-project"));
        assert!(json.contains("local_fallback"));
        assert_eq!(status.schema_version, 4);
        assert_eq!(status.projects.len(), 1);
        assert_eq!(status.projects[0].id, "example-project");
        assert!(status.durable_value.is_some());
        assert!(status
            .attention
            .iter()
            .all(|item| item.id != "dirty-worktree"));
    }

    #[test]
    fn unassigned_project_does_not_claim_an_agent_session() {
        let mut config = ProjectConfig::example_pilot();
        config.projects[0].herdr_session = None;
        let status = StatusSnapshot::local(&config, Path::new("/tmp/projects.toml"), false);
        assert!(status.agents.iter().all(|agent| agent.project_id.is_none()));
        assert!(status.active_world_id.is_none());
    }

    #[test]
    fn local_fallback_never_claims_activation_from_registry_order() {
        let config = ProjectConfig::example_pilot();
        let status = StatusSnapshot::local(&config, Path::new("/tmp/projects.toml"), false);
        assert_eq!(status.project.as_ref().map(|project| project.id.as_str()), Some("example-project"));
        assert!(status.active_world_id.is_none());
        assert_eq!(status.world_activation_status, "unknown");
    }

    #[test]
    fn xdg_config_path_keeps_workstation_namespace() {
        std::env::set_var("XDG_CONFIG_HOME", "/tmp/task-fabric-svn-xdg-config");
        std::env::remove_var("TASK_FABRIC_SVN_WORK_CONFIG");
        assert_eq!(
            default_config_path(),
            PathBuf::from("/tmp/task-fabric-svn-xdg-config/task-fabric-svn/projects.toml")
        );
    }

    #[test]
    fn scratchpad_config_roundtrips_and_defaults() {
        let toml_str = r#"
            version = 1
            [[projects]]
            id = "test-world"
            label = "Test World"
            repo = "/tmp/test"
            objective = "Test"
            phase = "active"
            next_action = "Inspect"
            theme = "catppuccin"

            [projects.scratchpad]
            enabled = true
            command = "claude"
        "#;
        let config: ProjectConfig = toml::from_str(toml_str).expect("parse toml");
        let project = &config.projects[0];
        assert_eq!(project.theme.as_deref(), Some("catppuccin"));
        let sp = project.scratchpad.as_ref().expect("scratchpad");
        assert!(sp.enabled);
        assert_eq!(sp.command.as_deref(), Some("claude"));
        assert!(sp.floating); // default true
    }
}
