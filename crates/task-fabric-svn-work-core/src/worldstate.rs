//! Versioned, owner-local Worldstate contracts and persistence helpers.
//!
//! The daemon is the authority for these records.  Hub receives only minimized
//! projections; the full records remain in the owner socket/state directory.

use crate::{
    notification_audio::TaskFabricSvnPresentation, now_epoch_seconds, Health, Project, ProjectConfig,
    ResumeEntry, StatusSnapshot,
};
use serde::{Deserialize, Serialize};
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::net::{IpAddr, TcpStream, ToSocketAddrs};
use std::process::Command;
use std::time::Duration;
use std::hash::{Hash, Hasher};

pub const WORLDSTATE_SCHEMA_VERSION: u32 = 1;
pub const MAX_EVENTS: usize = 2_000;
pub const MAX_RECEIPTS: usize = 100;
pub const MAX_STRING: usize = 4_096;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StudyContext {
    pub schema_version: String,
    pub active_project: Option<String>,
    pub project_label: Option<String>,
    pub objective: Option<String>,
    pub study_program_refs: Vec<String>,
    pub study_domain_refs: Vec<String>,
    pub research_question_refs: Vec<String>,
    pub recent_evidence_events: Vec<WorldEvent>,
}

pub fn study_context_for(config: &ProjectConfig, requested: Option<&str>) -> StudyContext {
    let project = requested
        .and_then(|id| config.projects.iter().find(|p| p.id == id))
        .or_else(|| config.first_project());
    let events = read_events(24)
        .unwrap_or_default()
        .into_iter()
        .filter(|event| !event.evidence_refs.is_empty())
        .take(12)
        .collect();
    StudyContext {
        schema_version: "task_fabric_svn.work.study-context.v1".into(),
        active_project: project.map(|p| p.id.clone()),
        project_label: project.map(|p| p.label.clone()),
        objective: project.map(|p| p.objective.clone()),
        study_program_refs: project
            .map(|p| p.study_program_refs.clone())
            .unwrap_or_default(),
        study_domain_refs: project
            .map(|p| p.study_domain_refs.clone())
            .unwrap_or_default(),
        research_question_refs: project
            .map(|p| p.research_question_refs.clone())
            .unwrap_or_default(),
        recent_evidence_events: events,
    }
}

/// Bounded, owner-local semantic input for the projection runtime.  This is
/// deliberately JSON-shaped so the workstation and companion repositories can
/// evolve independently while sharing the versioned wire contract.
pub fn semantic_context_for(config: &ProjectConfig, requested: Option<&str>) -> serde_json::Value {
    let project = requested
        .and_then(|id| config.projects.iter().find(|p| p.id == id))
        .or_else(|| config.first_project());
    let Some(project) = project else { return serde_json::json!({"schema_version":"task_fabric_svn.semantic-surface.v1","error":"no world configured"}); };
    let status_config = ProjectConfig { version: config.version, projects: vec![project.clone()] };
    let status = StatusSnapshot::local(&status_config, Path::new("projects.toml"), false);
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    project.id.hash(&mut hasher); project.objective.hash(&mut hasher); project.phase.hash(&mut hasher); project.next_action.hash(&mut hasher);
    project.resume_action_id.hash(&mut hasher); project.semantic_profile.hash(&mut hasher);
    for service in &status.services { service.id.hash(&mut hasher); service.health.hash(&mut hasher); }
    for action in &project.actions { action.id.hash(&mut hasher); action.label.hash(&mut hasher); action.risk.hash(&mut hasher); }
    let revision = format!("{}-{:016x}", project.id, hasher.finish());
    let mut facts = vec![
        serde_json::json!({"fact_id":"objective","role":"explanation","text":project.objective,"required_verbatim":true}),
        serde_json::json!({"fact_id":"phase","role":"status","text":project.phase,"required_verbatim":true}),
        serde_json::json!({"fact_id":"next-action","role":"action","text":project.next_action,"required_verbatim":true})
    ];
    for service in &status.services {
        facts.push(serde_json::json!({"fact_id":format!("service-{}", service.id),"role":if service.health == Health::Active {"status"} else {"warning"},"text":format!("Service {} is {}", service.id, service.health),"subject_ref":service.target,"required_verbatim":matches!(service.health, Health::Inactive|Health::Degraded),"canonical_only":false}));
    }
    let profile = project.semantic_profile.clone().unwrap_or_default();
    let resume_action_id = project.resume_action_id.clone().or_else(|| project.actions.first().map(|a| a.id.clone()));
    serde_json::json!({
        "schema_version":"task_fabric_svn.semantic-surface.v1", "world_id":project.id, "revision":revision,
        "label":project.label, "objective":project.objective, "phase":project.phase,
        "intention":profile.intention.or_else(|| Some(project.objective.clone())), "commitments":profile.commitments, "open_questions":profile.open_questions, "known_concepts":profile.known_concepts, "explain_concepts":profile.explain_concepts, "hazards":profile.hazards,
        "canonical_facts":facts,
        "actions":project.actions.iter().map(|a| serde_json::json!({"action_id":a.id,"label":a.label,"risk":a.risk,"consequence":format!("Run the declared {} action", a.label),"target_ref":a.cwd})).collect::<Vec<_>>(),
        "resume_action_id":resume_action_id,
        "evidence_refs":[], "updated_at":status.generated_at_epoch_seconds.to_string()
    })
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
struct SemanticBaseline {
    revision: String,
    facts: std::collections::BTreeMap<String, String>,
    recorded_at: u64,
}

fn semantic_baselines_path(state: &Path) -> PathBuf { state.join("semantic-baselines.json") }

fn read_semantic_baselines(state: &Path) -> std::collections::BTreeMap<String, SemanticBaseline> {
    fs::read_to_string(semantic_baselines_path(state)).ok()
        .and_then(|text| serde_json::from_str(&text).ok()).unwrap_or_default()
}

fn write_semantic_baselines(state: &Path, baselines: &std::collections::BTreeMap<String, SemanticBaseline>) {
    let _ = atomic_write(&semantic_baselines_path(state), &serde_json::to_value(baselines).unwrap_or_else(|_| serde_json::json!({})));
}

/// Adds canonical, deterministic changes since the last explicit World entry.
/// Reads do not advance the baseline; `record_semantic_baseline` does.
pub fn semantic_context_with_baseline(config: &ProjectConfig, requested: Option<&str>, state: &Path) -> serde_json::Value {
    let mut context = semantic_context_for(config, requested);
    let Some(world_id) = context.get("world_id").and_then(|v| v.as_str()) else { return context; };
    let current: std::collections::BTreeMap<String, String> = context.get("canonical_facts").and_then(|v| v.as_array()).into_iter().flatten()
        .filter_map(|fact| Some((fact.get("fact_id")?.as_str()?.to_string(), fact.get("text")?.as_str()?.to_string()))).collect();
    let baselines = read_semantic_baselines(state);
    let changes = baselines.get(world_id).map(|baseline| {
        let mut out = Vec::new();
        for (id, text) in &current {
            match baseline.facts.get(id) {
                Some(previous) if previous == text => {}
                Some(_) => out.push(format!("Changed {id}: {text}")),
                None => out.push(format!("Added {id}: {text}")),
            }
        }
        for id in baseline.facts.keys().filter(|id| !current.contains_key(*id)) { out.push(format!("Removed {id}")); }
        out
    }).unwrap_or_default();
    context["changes_since_last_entry"] = serde_json::json!(changes);
    context
}

pub fn record_semantic_baseline(context: &serde_json::Value, state: &Path) {
    let Some(world_id) = context.get("world_id").and_then(|v| v.as_str()) else { return; };
    let facts = context.get("canonical_facts").and_then(|v| v.as_array()).into_iter().flatten()
        .filter_map(|fact| Some((fact.get("fact_id")?.as_str()?.to_string(), fact.get("text")?.as_str()?.to_string()))).collect();
    let mut baselines = read_semantic_baselines(state);
    baselines.insert(world_id.to_string(), SemanticBaseline { revision: context.get("revision").and_then(|v| v.as_str()).unwrap_or_default().into(), facts, recorded_at: now_epoch_seconds() });
    write_semantic_baselines(state, &baselines);
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EffectOutcome {
    Completed,
    Skipped,
    Degraded,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct WorldSummary {
    pub id: String,
    pub label: String,
    pub repository_exists: bool,
    pub phase: String,
    #[serde(default)]
    pub observation: WorldObservation,
}

/// Expand legacy per-world declarations into the explicit resume graph. This
/// keeps existing registries dynamic while allowing owners to override entries
/// with richer `resume` declarations.
pub fn resume_entries_for(world: &Project) -> Vec<ResumeEntry> {
    let mut entries = world.resume.clone();
    if let Some(session) = world.herdr_session.as_deref().filter(|value| !value.is_empty()) {
        if !entries.iter().any(|entry| entry.kind == "agent" || entry.kind == "herdr") {
            entries.push(ResumeEntry { id: "herdr-session".into(), kind: "agent".into(), session: Some(session.into()), required: true, ..ResumeEntry::default() });
        }
    }
    for resource in world.resources.iter().filter(|resource| matches!(resource.kind.as_str(), "browser" | "portal")) {
        if !entries.iter().any(|entry| entry.resource.as_deref() == Some(resource.id.as_str())) {
            entries.push(ResumeEntry { id: format!("portal-{}", resource.id), kind: "portal".into(), resource: Some(resource.id.clone()), required: false, ..ResumeEntry::default() });
        }
    }
    entries
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(default)]
pub struct WorldObservation {
    pub git_branch: Option<String>,
    pub git_dirty: bool,
    pub head_commit: Option<String>,
    pub latest_commit_epoch_seconds: Option<u64>,
    pub observed_at_epoch_seconds: u64,
    pub portals: Vec<PortalObservation>,
    pub herdr_session: Option<String>,
    pub herdr_reachable: Option<bool>,
    pub requested_theme: Option<String>,
    pub observed_theme: Option<String>,
    pub theme_matches: Option<bool>,
    pub services: Vec<ServiceObservation>,
    /// Observation is evidence only; verification is recorded separately.
    #[serde(default)] pub evidence: Vec<ObservationEvidence>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(default)]
pub struct ObservationEvidence {
    pub subject: String,
    pub observed_at_epoch_seconds: u64,
    pub freshness_seconds: u64,
    pub method: String,
    pub evidence_ref: Option<String>,
    pub verification_level: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(default)]
pub struct ServiceObservation {
    pub service_id: String,
    pub target: String,
    pub ready: bool,
    pub state: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(default)]
pub struct PortalObservation {
    pub resource_id: String,
    pub target: String,
    pub reachable: bool,
    pub authentication_status: String,
}

impl Default for WorldSummary {
    fn default() -> Self {
        Self {
            id: String::new(),
            label: String::new(),
            repository_exists: false,
            phase: String::new(),
            observation: WorldObservation::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct AuthorityPosture {
    pub assurance: String,
    pub safe_mode: bool,
    pub workflows_enabled: bool,
    pub degraded_reason: Option<String>,
}

impl Default for AuthorityPosture {
    fn default() -> Self {
        Self {
            assurance: "unknown".into(),
            safe_mode: false,
            workflows_enabled: false,
            degraded_reason: Some("authority posture unavailable".into()),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct TransitionReference {
    pub id: String,
    pub outcome: String,
    pub started_at_epoch_seconds: u64,
    pub finished_at_epoch_seconds: Option<u64>,
    #[serde(default)]
    pub trace_id: Option<String>,
}

impl Default for TransitionReference {
    fn default() -> Self {
        Self {
            id: String::new(),
            outcome: String::new(),
            started_at_epoch_seconds: 0,
            finished_at_epoch_seconds: None,
            trace_id: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct WorldStateSnapshot {
    pub schema_version: u32,
    pub snapshot_id: String,
    pub generated_at_epoch_seconds: u64,
    pub runtime_version: String,
    pub source_commit: String,
    pub freshness_seconds: u64,
    pub degraded_reasons: Vec<String>,
    /// The world the owner has selected in a surface. Selection is intent,
    /// not proof that the desktop has activated it.
    #[serde(default)]
    pub selected_world_id: Option<String>,
    pub requested_world_id: Option<String>,
    pub active_world_id: Option<String>,
    #[serde(default = "default_activation_status")]
    pub world_activation_status: String,
    pub in_progress_transition_id: Option<String>,
    pub worlds: Vec<WorldSummary>,
    pub active_status: Option<StatusSnapshot>,
    pub authority: AuthorityPosture,
    pub last_transition: Option<TransitionReference>,
    pub attention_state: String,
    #[serde(default)]
    pub active_trace_id: Option<String>,
}

fn default_activation_status() -> String { "unknown".into() }

impl Default for WorldStateSnapshot {
    fn default() -> Self {
        Self {
            schema_version: WORLDSTATE_SCHEMA_VERSION,
            snapshot_id: String::new(),
            generated_at_epoch_seconds: 0,
            runtime_version: env!("CARGO_PKG_VERSION").into(),
            source_commit: option_env!("TASK_FABRIC_SVN_SOURCE_COMMIT")
                .unwrap_or("unknown")
                .into(),
            freshness_seconds: 0,
            degraded_reasons: Vec::new(),
            selected_world_id: None,
            requested_world_id: None,
            active_world_id: None,
            world_activation_status: "unknown".into(),
            in_progress_transition_id: None,
            worlds: Vec::new(),
            active_status: None,
            authority: AuthorityPosture::default(),
            last_transition: None,
            attention_state: "quiet".into(),
            active_trace_id: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum IntentCaller {
    HumanCli,
    VoiceGateway,
    AgentSession { agent_name: String, session_id: String },
    SystemTimer,
}

impl Default for IntentCaller {
    fn default() -> Self {
        Self::HumanCli
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct IntentRecord {
    pub id: String,
    pub caller: IntentCaller,
    pub summary: String,
    pub raw_prompt: Option<String>,
    pub parent_trace_id: Option<String>,
    pub authorization_ref: Option<String>,
    pub timestamp_epoch_seconds: u64,
}

impl Default for IntentRecord {
    fn default() -> Self {
        Self {
            id: String::new(),
            caller: IntentCaller::HumanCli,
            summary: String::new(),
            raw_prompt: None,
            parent_trace_id: None,
            authorization_ref: None,
            timestamp_epoch_seconds: 0,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct WorldEvent {
    pub schema_version: u32,
    pub id: String,
    pub timestamp_epoch_seconds: u64,
    pub world_id: Option<String>,
    pub transition_id: Option<String>,
    pub trace_id: Option<String>,
    pub intent_id: Option<String>,
    pub kind: String,
    pub source: String,
    pub summary: String,
    pub object_ref: Option<String>,
    pub effect_ref: Option<String>,
    pub outcome: Option<EffectOutcome>,
    /// Optional presentation metadata consumed by local notification adapters.
    /// Older records deserialize with no audio policy and remain compatible.
    pub presentation: Option<TaskFabricSvnPresentation>,
    pub evidence_refs: Vec<String>,
}

impl Default for WorldEvent {
    fn default() -> Self {
        Self {
            schema_version: WORLDSTATE_SCHEMA_VERSION,
            id: String::new(),
            timestamp_epoch_seconds: 0,
            world_id: None,
            transition_id: None,
            trace_id: None,
            intent_id: None,
            kind: String::new(),
            source: String::new(),
            summary: String::new(),
            object_ref: None,
            effect_ref: None,
            outcome: None,
            presentation: None,
            evidence_refs: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct WorldTransitionEffect {
    pub id: String,
    pub kind: String,
    pub dependencies: Vec<String>,
    pub critical: bool,
    pub authority_requirement: String,
    pub precondition: String,
    pub expected_postcondition: String,
    pub target_ref: String,
}

impl Default for WorldTransitionEffect {
    fn default() -> Self {
        Self {
            id: String::new(),
            kind: String::new(),
            dependencies: Vec::new(),
            critical: false,
            authority_requirement: "session_trust".into(),
            precondition: String::new(),
            expected_postcondition: String::new(),
            target_ref: String::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct WorldTransitionPlan {
    pub schema_version: u32,
    pub id: String,
    pub world_id: String,
    pub base_snapshot_id: String,
    pub created_at_epoch_seconds: u64,
    pub trace_id: Option<String>,
    pub intent: Option<IntentRecord>,
    pub effects: Vec<WorldTransitionEffect>,
}

impl Default for WorldTransitionPlan {
    fn default() -> Self {
        Self {
            schema_version: WORLDSTATE_SCHEMA_VERSION,
            id: String::new(),
            world_id: String::new(),
            base_snapshot_id: String::new(),
            created_at_epoch_seconds: 0,
            trace_id: None,
            intent: None,
            effects: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct WorldTransitionEffectResult {
    pub effect_id: String,
    pub outcome: EffectOutcome,
    pub detail: String,
    pub evidence_refs: Vec<String>,
}

impl Default for WorldTransitionEffectResult {
    fn default() -> Self {
        Self {
            effect_id: String::new(),
            outcome: EffectOutcome::Skipped,
            detail: String::new(),
            evidence_refs: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct WorldTransitionReceipt {
    pub schema_version: u32,
    pub id: String,
    pub transition_id: String,
    pub world_id: String,
    pub started_at_epoch_seconds: u64,
    pub finished_at_epoch_seconds: u64,
    pub duration_ms: u128,
    pub outcome: String,
    pub trace_id: Option<String>,
    pub intent: Option<IntentRecord>,
    pub before_snapshot: WorldStateSnapshot,
    pub after_snapshot: WorldStateSnapshot,
    pub event_ids: Vec<String>,
    pub effects: Vec<WorldTransitionEffectResult>,
}

impl Default for WorldTransitionReceipt {
    fn default() -> Self {
        Self {
            schema_version: WORLDSTATE_SCHEMA_VERSION,
            id: String::new(),
            transition_id: String::new(),
            world_id: String::new(),
            started_at_epoch_seconds: 0,
            finished_at_epoch_seconds: 0,
            duration_ms: 0,
            outcome: "failed".into(),
            trace_id: None,
            intent: None,
            before_snapshot: WorldStateSnapshot::default(),
            after_snapshot: WorldStateSnapshot::default(),
            event_ids: Vec::new(),
            effects: Vec::new(),
        }
    }
}

pub fn state_dir() -> PathBuf {
    if let Some(path) = std::env::var_os("TASK_FABRIC_SVN_WORK_STATE") {
        return PathBuf::from(path);
    }
    if let Some(path) = std::env::var_os("XDG_STATE_HOME") {
        return PathBuf::from(path).join("task-fabric-svn");
    }
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".local/state/task-fabric-svn")
}

pub fn read_last_transition_reference() -> Option<TransitionReference> {
    fs::read(state_dir().join("last-transition.json"))
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
}

fn observe_world(project: &Project) -> WorldObservation {
    let mut observation = WorldObservation {
        observed_at_epoch_seconds: now_epoch_seconds(),
        ..WorldObservation::default()
    };
    observation.portals = project.resources.iter().filter(|resource| matches!(resource.kind.as_str(), "browser" | "portal")).map(|resource| observe_portal(&resource.id, &resource.target)).collect();
    observation.herdr_session = project.herdr_session.clone();
    observation.herdr_reachable = project.herdr_session.as_deref().map(|session| {
        Command::new("herdr").args(["--session", session, "api", "snapshot"]).output().map(|output| output.status.success()).unwrap_or(false)
    });
    observation.requested_theme = project.theme.clone();
    observation.observed_theme = probe_active_theme();
    observation.theme_matches = match (observation.requested_theme.as_deref(), observation.observed_theme.as_deref()) {
        (Some(requested), Some(observed)) => Some(normalize_theme(requested) == normalize_theme(observed)),
        (None, _) => None,
        (Some(_), None) => Some(false),
    };
    observation.services = project.services.iter().map(|service| {
        let unit = service.unit.as_deref().unwrap_or(&service.target);
        let systemd = matches!(service.kind.as_str(), "systemd" | "user-systemd") || service.unit.is_some();
        let ready = if systemd {
            Command::new("systemctl").args(["--user", "is-active", "--quiet", unit]).status().map(|status| status.success()).unwrap_or(false)
        } else if let Some(port) = service.port {
            ("127.0.0.1", port).to_socket_addrs().ok().and_then(|mut addresses| addresses.next()).and_then(|address| TcpStream::connect_timeout(&address, Duration::from_millis(150)).ok()).is_some()
        } else {
            false
        };
        ServiceObservation { service_id: service.id.clone(), target: service.target.clone(), ready, state: if ready { "ready".into() } else { "unavailable".into() } }
    }).collect();
    if !project.repo.is_dir() {
        return observation;
    }
    let git = |args: &[&str]| -> Option<String> {
        Command::new("git").arg("-C").arg(&project.repo).args(args).output().ok().and_then(|output| {
            if output.status.success() { Some(String::from_utf8_lossy(&output.stdout).trim().to_string()) } else { None }
        }).filter(|value| !value.is_empty())
    };
    observation.git_branch = git(&["branch", "--show-current"]);
    observation.git_dirty = Command::new("git").arg("-C").arg(&project.repo).args(["status", "--porcelain"]).output().map(|output| output.status.success() && !output.stdout.is_empty()).unwrap_or(false);
    observation.head_commit = git(&["rev-parse", "--short", "HEAD"]);
    observation.latest_commit_epoch_seconds = git(&["log", "-1", "--format=%ct"]).and_then(|value| value.parse().ok());
    observation
}

fn observe_portal(resource_id: &str, target: &str) -> PortalObservation {
    let (host, port) = target.split_once("://").and_then(|(_, rest)| {
        let authority = rest.split('/').next().unwrap_or(rest);
        let (host, port) = authority.rsplit_once(':').map(|(host, port)| (host, port.parse().ok())).unwrap_or((authority, None));
        Some((host, port.unwrap_or(443)))
    }).unwrap_or(("", 0));
    let local_target = host == "localhost" || host.parse::<IpAddr>().map(|address| address.is_loopback()).unwrap_or(false);
    let reachable = local_target && !host.is_empty() && port > 0 && (host, port).to_socket_addrs().ok().and_then(|mut addresses| addresses.next()).and_then(|address| TcpStream::connect_timeout(&address, Duration::from_millis(250)).ok()).is_some();
    PortalObservation { resource_id: resource_id.into(), target: target.into(), reachable, authentication_status: if reachable { "unknown".into() } else if local_target { "unavailable".into() } else { "unknown".into() } }
}

pub fn new_id(prefix: &str) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    format!("{}-{}-{}", prefix, nanos, std::process::id())
}

pub fn new_trace_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

pub fn snapshot_for(
    config: &ProjectConfig,
    requested: Option<&str>,
    active: Option<&str>,
    in_progress: Option<&str>,
    last: Option<TransitionReference>,
) -> WorldStateSnapshot {
    snapshot_for_with_state(config, requested, requested, active, in_progress, last)
}

/// Build a snapshot with explicit selection, request, and activation state.
/// Keeping these identifiers separate prevents a UI selection from being
/// mistaken for a successful desktop transition.
pub fn snapshot_for_with_state(
    config: &ProjectConfig,
    selected: Option<&str>,
    requested: Option<&str>,
    active: Option<&str>,
    in_progress: Option<&str>,
    last: Option<TransitionReference>,
) -> WorldStateSnapshot {
    let now = now_epoch_seconds();
    // Probe the requested/active World, never the first registry entry.  The
    // first-project fallback is retained for old callers that do not provide
    // an active identifier.
    let status_config = active
        .or(requested)
        .and_then(|id| config.projects.iter().find(|project| project.id == id))
        .map(|project| ProjectConfig { version: config.version, projects: vec![project.clone()] })
        .unwrap_or_else(|| config.clone());
    let status = status_config
        .first_project()
        .map(|_| StatusSnapshot::local(&status_config, Path::new("projects.toml"), false));
    let mut degraded = status
        .as_ref()
        .map(|s| s.degraded.clone())
        .unwrap_or_default();
    let worlds = config
        .projects
        .iter()
        .map(|p| WorldSummary {
            id: p.id.clone(),
            label: p.label.clone(),
            repository_exists: p.repo.is_dir(),
            phase: p.phase.clone(),
            observation: observe_world(p),
        })
        .collect();
    if config.projects.is_empty() {
        degraded.push("no worlds configured".into());
    }
    let world_activation_status = match (requested, active, in_progress, last.as_ref()) {
        (_, _, Some(_), _) => "activating",
        (Some(requested), Some(active), _, _) if requested == active => match last.as_ref().map(|r| r.outcome.as_str()) {
            Some("degraded") => "degraded",
            Some("failed") => "failed",
            _ => "active",
        },
        (Some(_), Some(_), _, Some(reference)) if reference.outcome == "failed" => "failed",
        (Some(_), Some(_), _, Some(reference)) if reference.outcome == "degraded" => "degraded",
        (Some(requested), Some(active), _, _) if requested != active => "degraded",
        (Some(_), Some(_), _, _) => "active",
        (Some(_), None, _, _) => "degraded",
        (None, Some(_), _, _) => "active",
        _ => "unavailable",
    }.into();
    WorldStateSnapshot {
        schema_version: WORLDSTATE_SCHEMA_VERSION,
        snapshot_id: new_id("snapshot"),
        generated_at_epoch_seconds: now,
        runtime_version: env!("CARGO_PKG_VERSION").into(),
        source_commit: option_env!("TASK_FABRIC_SVN_SOURCE_COMMIT")
            .unwrap_or("unknown")
            .into(),
        freshness_seconds: 0,
        degraded_reasons: degraded,
        selected_world_id: selected.map(str::to_string),
        requested_world_id: requested.map(str::to_string),
        active_world_id: active.map(str::to_string),
        world_activation_status,
        in_progress_transition_id: in_progress.map(str::to_string),
        worlds,
        active_status: status,
        authority: authority_posture(),
        last_transition: last.clone(),
        attention_state: "quiet".into(),
        active_trace_id: last.as_ref().and_then(|reference| reference.trace_id.clone()),
    }
}

fn authority_posture() -> AuthorityPosture {
    let output = Command::new("heron").args(["status", "--explain"]).output();
    let Ok(output) = output else {
        return AuthorityPosture::default();
    };
    let text = String::from_utf8_lossy(&output.stdout);
    let assurance = text
        .lines()
        .find_map(|line| {
            line.trim()
                .strip_prefix("\"assurance\":")
                .or_else(|| line.trim().strip_prefix("assurance:"))
        })
        .map(|value| value.trim().trim_matches(['"', ',']).to_string())
        .unwrap_or_else(|| "unknown".into());
    let safe_mode = text
        .lines()
        .any(|line| line.contains("safe_mode") && line.contains("true"));
    let workflows_enabled = text
        .lines()
        .any(|line| line.contains("local_workflows") && line.contains("enabled"));
    AuthorityPosture {
        assurance,
        safe_mode,
        workflows_enabled,
        degraded_reason: if output.status.success() {
            None
        } else {
            Some("authority status command failed".into())
        },
    }
}

pub fn plan_for(world: &Project, base_snapshot_id: &str) -> WorldTransitionPlan {
    plan_for_with_intent(world, base_snapshot_id, None, None)
}

pub fn plan_for_with_intent(
    world: &Project,
    base_snapshot_id: &str,
    trace_id: Option<String>,
    intent: Option<IntentRecord>,
) -> WorldTransitionPlan {
    let mut effects = vec![effect(
        "record-intent",
        "intent.requested",
        true,
        "always",
        &world.id,
        "requested world recorded",
    )];
    effects.push(effect(
        "validate-world",
        "world.validate",
        true,
        "session_trust",
        &world.repo.display().to_string(),
        "world definition and repository validated",
    ));
    effects.push(effect(
        "activate-world",
        "world.activate",
        true,
        "session_trust",
        &world.id,
        "active world record updated",
    ));
    for service in &world.services {
        effects.push(effect(
            &format!("service-{}", service.id),
            "service.ensure",
            false,
            "session_trust",
            &service.id,
            "declared service active",
        ));
    }
    if let Some(session) = &world.herdr_session {
        effects.push(effect(
            "herdr-session",
            "herdr.restore",
            false,
            "session_trust",
            session,
            "Herdr session reachable",
        ));
    }
    effects.push(effect(
        "observe-workspaces",
        "herdr.observe",
        false,
        "session_trust",
        world.herdr_session.as_deref().unwrap_or("none"),
        "real agent/workspace state observed",
    ));
    effects.push(effect(
        "hub-project",
        "hub.focus",
        false,
        "session_trust",
        &format!("project:{}", world.id),
        "Hub project route focused",
    ));
    if let Some(sp) = &world.scratchpad {
        if sp.enabled {
            effects.push(effect(
                "scratchpad-provision",
                "scratchpad.ensure",
                false,
                "session_trust",
                &format!("special:world-{}", world.id),
                "dedicated world scratchpad provisioned",
            ));
        }
    }
    if let Some(theme) = &world.theme {
        effects.push(effect(
            "theme-harmonize",
            "theme.apply",
            false,
            "session_trust",
            theme,
            "visual theme harmonized with world context",
        ));
    }
    for entry in resume_entries_for(world) {
        let mut resume_effect = effect(
            &format!("resume-{}", entry.id),
            &format!("resume.{}", entry.kind),
            entry.required,
            "session_trust",
            entry.target.as_deref().or(entry.session.as_deref()).or(entry.resource.as_deref()).unwrap_or(&entry.id),
            "declared world dependency or surface restored",
        );
        resume_effect.dependencies = entry.depends_on.iter().map(|id| format!("resume-{id}")).collect();
        effects.push(resume_effect);
    }
    effects.push(effect(
        "publish-state",
        "world.publish",
        true,
        "session_trust",
        &world.id,
        "after snapshot and receipt persisted",
    ));
    WorldTransitionPlan {
        schema_version: WORLDSTATE_SCHEMA_VERSION,
        id: new_id("plan"),
        world_id: world.id.clone(),
        base_snapshot_id: base_snapshot_id.into(),
        created_at_epoch_seconds: now_epoch_seconds(),
        trace_id,
        intent,
        effects,
    }
}

fn effect(
    id: &str,
    kind: &str,
    critical: bool,
    authority: &str,
    target: &str,
    post: &str,
) -> WorldTransitionEffect {
    WorldTransitionEffect {
        id: id.into(),
        kind: kind.into(),
        dependencies: Vec::new(),
        critical,
        authority_requirement: authority.into(),
        precondition: "effect has not already succeeded".into(),
        expected_postcondition: post.into(),
        target_ref: target.into(),
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SimulatedOutcome {
    Completed { detail: String },
    Skipped { reason: String },
    Conflict { port: u16, conflicting_process: Option<String>, remediation: String },
    Failed { reason: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SimulatedEffect {
    pub id: String,
    pub kind: String,
    pub target_ref: String,
    pub predicted_outcome: SimulatedOutcome,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PreflightLevel {
    Ready,
    Warning,
    Blocked,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PreflightItem {
    pub category: String,
    pub label: String,
    pub passed: bool,
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SimulationReport {
    pub world_id: String,
    pub world_label: String,
    #[serde(default)]
    pub requested_world_id: String,
    #[serde(default)]
    pub observed_active_world_id: Option<String>,
    #[serde(default)]
    pub projected_active_world_id: String,
    #[serde(default)]
    pub required_approval: bool,
    #[serde(default)]
    pub expected_receipt: String,
    pub level: PreflightLevel,
    pub ready: bool,
    pub preflight_checks: Vec<PreflightItem>,
    pub predicted_effects: Vec<SimulatedEffect>,
    pub blockers: Vec<String>,
    pub warnings: Vec<String>,
    pub remediation_hints: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActionDryRunReport {
    pub world_id: String,
    pub action_id: String,
    pub label: String,
    pub command: Vec<String>,
    pub resolved_binary: Option<String>,
    pub binary_exists: bool,
    pub binary_executable: bool,
    pub cwd: PathBuf,
    pub cwd_exists: bool,
    pub risk: String,
    pub ready: bool,
    pub warnings: Vec<String>,
    pub blockers: Vec<String>,
}

pub fn probe_port_in_use(port: u16) -> (bool, Option<String>) {
    let in_use = std::net::TcpListener::bind(("127.0.0.1", port)).is_err()
        || std::net::TcpListener::bind(("0.0.0.0", port)).is_err();
    if in_use {
        let detail = Command::new("ss")
            .args(["-tlpn", &format!("sport = :{port}")])
            .output()
            .ok()
            .and_then(|out| {
                let text = String::from_utf8_lossy(&out.stdout);
                text.lines().nth(1).map(|l| l.trim().to_string())
            });
        (true, detail)
    } else {
        (false, None)
    }
}

pub fn probe_systemd_unit(unit: &str) -> (bool, String) {
    if let Ok(out) = Command::new("systemctl")
        .args(["--user", "is-active", unit])
        .output()
    {
        let status = String::from_utf8_lossy(&out.stdout).trim().to_string();
        let is_active = status == "active";
        (is_active, status)
    } else {
        (false, "systemctl unavailable".to_string())
    }
}

pub fn probe_git_cleanliness(repo: &Path) -> (bool, usize, String) {
    if !repo.exists() {
        return (false, 0, format!("path not found: {}", repo.display()));
    }
    if let Ok(out) = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["status", "--porcelain"])
        .output()
    {
        let lines: Vec<_> = String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| l.to_string())
            .collect();
        let count = lines.len();
        if count == 0 {
            (true, 0, "clean working tree".to_string())
        } else {
            (false, count, format!("{count} uncommitted file(s)"))
        }
    } else {
        (false, 0, "git command failed".to_string())
    }
}

pub fn probe_active_theme() -> Option<String> {
    if let Some(home) = std::env::var_os("HOME") {
        let path = PathBuf::from(home)
            .join(".local/state/omarchy/current/theme.name");
        if let Ok(content) = fs::read_to_string(&path) {
            return Some(content.trim().to_string());
        }
    }
    None
}

pub fn normalize_theme(s: &str) -> String {
    s.to_lowercase().replace([' ', '-', '_'], "")
}

pub fn simulate_transition(
    world: &Project,
    active_world_id: Option<&str>,
    active_clients: Option<&[serde_json::Value]>,
) -> SimulationReport {
    let mut preflight_checks = Vec::new();
    let mut blockers = Vec::new();
    let mut warnings = Vec::new();
    let mut remediation_hints = Vec::new();
    let mut predicted_effects = Vec::new();

    // 1. Filesystem check
    let repo_exists = world.repo.is_dir();
    preflight_checks.push(PreflightItem {
        category: "filesystem".into(),
        label: "Repository directory".into(),
        passed: repo_exists,
        detail: if repo_exists {
            world.repo.display().to_string()
        } else {
            let msg = format!("Directory not found: {}", world.repo.display());
            blockers.push(msg.clone());
            remediation_hints.push(format!("Create or clone repository at '{}'", world.repo.display()));
            msg
        },
    });

    // 2. Git working tree cleanliness
    let (git_clean, uncommitted_count, git_detail) = probe_git_cleanliness(&world.repo);
    preflight_checks.push(PreflightItem {
        category: "git".into(),
        label: "Working tree state".into(),
        passed: git_clean,
        detail: git_detail.clone(),
    });
    if !git_clean && repo_exists {
        warnings.push(format!("Repository has {uncommitted_count} uncommitted change(s)"));
        remediation_hints.push("Commit or stash working tree changes before switching worlds".into());
    }

    // 3. Predicted effects
    // record-intent
    predicted_effects.push(SimulatedEffect {
        id: "record-intent".into(),
        kind: "intent.requested".into(),
        target_ref: world.id.clone(),
        predicted_outcome: SimulatedOutcome::Completed {
            detail: "persists IntentRecord into immutable audit stream".into(),
        },
    });

    // validate-world
    predicted_effects.push(SimulatedEffect {
        id: "validate-world".into(),
        kind: "world.validate".into(),
        target_ref: world.repo.display().to_string(),
        predicted_outcome: if repo_exists {
            SimulatedOutcome::Completed {
                detail: "world definition and repository validated".into(),
            }
        } else {
            SimulatedOutcome::Failed {
                reason: format!("repository path '{}' does not exist", world.repo.display()),
            }
        },
    });

    // activate-world
    let already_active = active_world_id == Some(&world.id);
    predicted_effects.push(SimulatedEffect {
        id: "activate-world".into(),
        kind: "world.activate".into(),
        target_ref: world.id.clone(),
        predicted_outcome: if already_active {
            SimulatedOutcome::Skipped {
                reason: "world is already active in workstation state".into(),
            }
        } else {
            SimulatedOutcome::Completed {
                detail: format!("records active world as '{}'", world.id),
            }
        },
    });

    // Services & Ports
    for service in &world.services {
        let (is_active, unit_status) = probe_systemd_unit(&service.target);
        if let Some(port) = service.port {
            let (port_in_use, pid_detail) = probe_port_in_use(port);
            if port_in_use && is_active {
                preflight_checks.push(PreflightItem {
                    category: "ports".into(),
                    label: format!("Service '{}' port {port}", service.id),
                    passed: true,
                    detail: format!("listening on port {port} (unit '{}' active)", service.target),
                });
                predicted_effects.push(SimulatedEffect {
                    id: format!("service-{}", service.id),
                    kind: "service.ensure".into(),
                    target_ref: service.id.clone(),
                    predicted_outcome: SimulatedOutcome::Skipped {
                        reason: format!("service already active on port {port}"),
                    },
                });
            } else if port_in_use && !is_active {
                let detail_msg = pid_detail.as_deref().unwrap_or("unknown process");
                preflight_checks.push(PreflightItem {
                    category: "ports".into(),
                    label: format!("Service '{}' port {port}", service.id),
                    passed: false,
                    detail: format!("PORT OCCUPIED by {detail_msg} while unit is inactive"),
                });
                warnings.push(format!("Port {port} is occupied by an external listener ({detail_msg})"));
                remediation_hints.push(format!("Port reconciler will auto-terminate conflicting PID or assign ephemeral port for '{}'", service.id));
                predicted_effects.push(SimulatedEffect {
                    id: format!("service-{}", service.id),
                    kind: "service.ensure".into(),
                    target_ref: service.id.clone(),
                    predicted_outcome: SimulatedOutcome::Conflict {
                        port,
                        conflicting_process: pid_detail,
                        remediation: "run `task-fabric-svn-work enter` to invoke auto port reconciler".into(),
                    },
                });
            } else {
                preflight_checks.push(PreflightItem {
                    category: "ports".into(),
                    label: format!("Service '{}' port {port}", service.id),
                    passed: true,
                    detail: format!("port {port} free, unit '{}' is {unit_status}", service.target),
                });
                predicted_effects.push(SimulatedEffect {
                    id: format!("service-{}", service.id),
                    kind: "service.ensure".into(),
                    target_ref: service.id.clone(),
                    predicted_outcome: SimulatedOutcome::Completed {
                        detail: format!("starts systemd user unit '{}' on free port {port}", service.target),
                    },
                });
            }
        } else {
            preflight_checks.push(PreflightItem {
                category: "systemd".into(),
                label: format!("Service '{}'", service.id),
                passed: is_active,
                detail: format!("unit '{}' is {unit_status}", service.target),
            });
            predicted_effects.push(SimulatedEffect {
                id: format!("service-{}", service.id),
                kind: "service.ensure".into(),
                target_ref: service.id.clone(),
                predicted_outcome: if is_active {
                    SimulatedOutcome::Skipped {
                        reason: format!("unit '{}' already active", service.target),
                    }
                } else {
                    SimulatedOutcome::Completed {
                        detail: format!("activates systemd unit '{}'", service.target),
                    }
                },
            });
        }
    }

    // Herdr session
    if let Some(session) = &world.herdr_session {
        predicted_effects.push(SimulatedEffect {
            id: "herdr-session".into(),
            kind: "herdr.restore".into(),
            target_ref: session.clone(),
            predicted_outcome: SimulatedOutcome::Completed {
                detail: format!("re-attaches or inspects persistent Herdr session '{session}'"),
            },
        });
    }

    // Hub project
    predicted_effects.push(SimulatedEffect {
        id: "hub-project".into(),
        kind: "hub.focus".into(),
        target_ref: format!("project:{}", world.id),
        predicted_outcome: SimulatedOutcome::Completed {
            detail: format!("focuses Hub project route 'project:{}'", world.id),
        },
    });

    // Scratchpad
    if let Some(sp) = &world.scratchpad {
        if sp.enabled {
            let special_name = format!("special:world-{}", world.id);
            let has_client = active_clients.map_or(false, |clients| {
                clients.iter().any(|c| {
                    c.get("workspace")
                        .and_then(|w| w.get("name"))
                        .and_then(serde_json::Value::as_str)
                        == Some(&special_name)
                })
            });
            preflight_checks.push(PreflightItem {
                category: "scratchpad".into(),
                label: "Dedicated scratchpad".into(),
                passed: true,
                detail: if has_client {
                    format!("already running on {special_name}")
                } else {
                    format!("will spawn {}-scratchpad on {special_name}", world.id)
                },
            });
            predicted_effects.push(SimulatedEffect {
                id: "scratchpad-provision".into(),
                kind: "scratchpad.ensure".into(),
                target_ref: special_name.clone(),
                predicted_outcome: if has_client {
                    SimulatedOutcome::Skipped {
                        reason: format!("scratchpad already active on {special_name}"),
                    }
                } else {
                    SimulatedOutcome::Completed {
                        detail: format!("provisions {} scratchpad onto {special_name}", sp.terminal.as_deref().unwrap_or("foot")),
                    }
                },
            });
        }
    }

    // Theme harmonization
    if let Some(target_theme) = &world.theme {
        let current_theme = probe_active_theme();
        let matches = current_theme.as_ref().map_or(false, |cur| {
            normalize_theme(cur) == normalize_theme(target_theme)
        });
        preflight_checks.push(PreflightItem {
            category: "theme".into(),
            label: "Visual theme alignment".into(),
            passed: true,
            detail: if matches {
                format!("already synchronized with '{target_theme}'")
            } else {
                format!("will harmonize from '{}' to '{target_theme}'", current_theme.as_deref().unwrap_or("default"))
            },
        });
        predicted_effects.push(SimulatedEffect {
            id: "theme-harmonize".into(),
            kind: "theme.apply".into(),
            target_ref: target_theme.clone(),
            predicted_outcome: if matches {
                SimulatedOutcome::Skipped {
                    reason: format!("theme '{target_theme}' already active"),
                }
            } else {
                SimulatedOutcome::Completed {
                    detail: format!("executes `omarchy-theme-set '{target_theme}'`"),
                }
            },
        });
    }

    for entry in resume_entries_for(world) {
        let target = entry.target.as_deref().or(entry.session.as_deref()).or(entry.resource.as_deref()).unwrap_or(&entry.id);
        let valid = match entry.kind.as_str() {
            "agent" | "herdr" => entry.session.as_deref().is_some_and(|session| !session.is_empty()),
            "portal" | "browser" | "surface" => entry.target.as_deref().or_else(|| entry.resource.as_deref()).is_some(),
            _ => false,
        };
        if !valid && entry.required {
            blockers.push(format!("resume entry '{}' is incomplete", entry.id));
        } else if !valid {
            warnings.push(format!("optional resume entry '{}' is incomplete", entry.id));
        }
        preflight_checks.push(PreflightItem {
            category: "resume".into(),
            label: format!("Resume {}", entry.id),
            passed: valid,
            detail: if valid { format!("{} '{}' will be restored", entry.kind, target) } else { "declaration is incomplete".into() },
        });
        let mut simulated = SimulatedEffect {
            id: format!("resume-{}", entry.id),
            kind: format!("resume.{}", entry.kind),
            target_ref: target.to_string(),
            predicted_outcome: if valid { SimulatedOutcome::Completed { detail: "declared dependency or surface will be restored".into() } } else { SimulatedOutcome::Failed { reason: "incomplete declaration".into() } },
        };
        if !entry.depends_on.is_empty() {
            simulated.target_ref = format!("{} (after {})", target, entry.depends_on.join(", "));
        }
        predicted_effects.push(simulated);
    }

    // publish-state
    predicted_effects.push(SimulatedEffect {
        id: "publish-state".into(),
        kind: "world.publish".into(),
        target_ref: world.id.clone(),
        predicted_outcome: SimulatedOutcome::Completed {
            detail: "persists WorldTransitionReceipt and publishes state".into(),
        },
    });

    let level = if !blockers.is_empty() {
        PreflightLevel::Blocked
    } else if !warnings.is_empty() {
        PreflightLevel::Warning
    } else {
        PreflightLevel::Ready
    };
    let ready = blockers.is_empty();

    SimulationReport {
        world_id: world.id.clone(),
        world_label: world.label.clone(),
        requested_world_id: world.id.clone(),
        observed_active_world_id: active_world_id.map(str::to_string),
        projected_active_world_id: world.id.clone(),
        required_approval: true,
        expected_receipt: "world-transition receipt with before/after snapshots".into(),
        level,
        ready,
        preflight_checks,
        predicted_effects,
        blockers,
        warnings,
        remediation_hints,
    }
}

pub fn simulate_action_dry_run(
    world: &Project,
    action_id: &str,
) -> Result<ActionDryRunReport, String> {
    let action = world
        .actions
        .iter()
        .find(|a| a.id.eq_ignore_ascii_case(action_id))
        .ok_or_else(|| format!("action '{}' not found in world '{}'", action_id, world.id))?;

    let cwd = action.cwd.clone().unwrap_or_else(|| world.repo.clone());
    let cwd_exists = cwd.is_dir();

    let mut warnings = Vec::new();
    let mut blockers = Vec::new();

    if !cwd_exists {
        blockers.push(format!("working directory does not exist: {}", cwd.display()));
    }

    let binary = action.command.first().map(|s| s.as_str()).unwrap_or("");
    let mut binary_exists = false;
    let mut binary_executable = false;
    let mut resolved_binary = None;

    if binary.is_empty() {
        blockers.push("action command is empty".to_string());
    } else {
        let bin_path = PathBuf::from(binary);
        if bin_path.is_absolute() || binary.starts_with('.') {
            let target = if bin_path.is_absolute() {
                bin_path
            } else {
                cwd.join(bin_path)
            };
            if target.is_file() {
                binary_exists = true;
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    if let Ok(meta) = target.metadata() {
                        binary_executable = meta.permissions().mode() & 0o111 != 0;
                    }
                }
                #[cfg(not(unix))]
                {
                    binary_executable = true;
                }
                resolved_binary = Some(target.display().to_string());
            }
        } else if let Some(paths) = std::env::var_os("PATH") {
            for dir in std::env::split_paths(&paths) {
                let candidate = dir.join(binary);
                if candidate.is_file() {
                    binary_exists = true;
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::PermissionsExt;
                        if let Ok(meta) = candidate.metadata() {
                            if meta.permissions().mode() & 0o111 != 0 {
                                binary_executable = true;
                                resolved_binary = Some(candidate.display().to_string());
                                break;
                            }
                        }
                    }
                    #[cfg(not(unix))]
                    {
                        binary_executable = true;
                        resolved_binary = Some(candidate.display().to_string());
                        break;
                    }
                }
            }
        }

        if !binary_exists {
            blockers.push(format!("executable '{binary}' not found in PATH or working directory"));
        } else if !binary_executable {
            blockers.push(format!("binary '{binary}' is not executable (missing +x permission)"));
        }
    }

    if action.risk == "destructive" {
        warnings.push("action is classified as destructive; user confirmation required before execution".to_string());
    } else if action.risk == "review" {
        warnings.push("action requires explicit review before execution".to_string());
    }

    let ready = blockers.is_empty();

    Ok(ActionDryRunReport {
        world_id: world.id.clone(),
        action_id: action.id.clone(),
        label: action.label.clone(),
        command: action.command.clone(),
        resolved_binary,
        binary_exists,
        binary_executable,
        cwd,
        cwd_exists,
        risk: action.risk.clone(),
        ready,
        warnings,
        blockers,
    })
}

// =========================================================================
// Phase 4: Compounding Provenance — The Friction Compiler & The Tutor
// =========================================================================

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FrictionKind {
    PortConflict,
    ServiceDegraded,
    MissingScratchpad,
    DirtyWorkingTree,
    MissingAction,
    MissingAlias,
    RepositoryMissing,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FrictionSeverity {
    Suggestion,
    Warning,
    Blocker,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FrictionPattern {
    pub kind: FrictionKind,
    pub severity: FrictionSeverity,
    pub world_id: String,
    pub target_ref: String,
    pub title: String,
    pub rationale: String,
    pub proposed_toml_patch: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FrictionReport {
    pub timestamp_epoch_seconds: u64,
    pub patterns: Vec<FrictionPattern>,
    pub overall_friction_score: u32,
    pub analyzed_events_count: usize,
}

pub fn analyze_workstation_friction(
    config: &ProjectConfig,
    events: &[WorldEvent],
    filter_world: Option<&str>,
) -> FrictionReport {
    let mut patterns = Vec::new();
    let now = crate::now_epoch_seconds();

    // 1. Scan historical events for degraded / failed effect outcomes
    for event in events {
        if let Some(target) = filter_world {
            if event.world_id.as_deref() != Some(target) {
                continue;
            }
        }
        if event.outcome == Some(EffectOutcome::Degraded) || event.outcome == Some(EffectOutcome::Failed) {
            let wid = event.world_id.clone().unwrap_or_else(|| "unknown".into());
            let effect = event.effect_ref.clone().unwrap_or_else(|| event.kind.clone());
            patterns.push(FrictionPattern {
                kind: FrictionKind::ServiceDegraded,
                severity: FrictionSeverity::Warning,
                world_id: wid,
                target_ref: effect,
                title: format!("Historical effect failure: {}", event.summary),
                rationale: "Recent transition event recorded degraded or failed outcome; consider auditing service health or config".into(),
                proposed_toml_patch: None,
            });
        }
    }

    // 2. Scan world definitions for declarative improvements and machine invariants
    for project in &config.projects {
        if let Some(target) = filter_world {
            if !project.id.eq_ignore_ascii_case(target)
                && !project.aliases.iter().any(|a| a.eq_ignore_ascii_case(target))
            {
                continue;
            }
        }

        // Repository existence
        if !project.repo.is_dir() {
            patterns.push(FrictionPattern {
                kind: FrictionKind::RepositoryMissing,
                severity: FrictionSeverity::Blocker,
                world_id: project.id.clone(),
                target_ref: project.repo.display().to_string(),
                title: format!("Repository directory missing: {}", project.repo.display()),
                rationale: "Declared project repository path does not exist on disk".into(),
                proposed_toml_patch: None,
            });
        } else {
            // Git cleanliness
            let (is_clean, uncommitted, _) = probe_git_cleanliness(&project.repo);
            if !is_clean && uncommitted > 0 {
                patterns.push(FrictionPattern {
                    kind: FrictionKind::DirtyWorkingTree,
                    severity: FrictionSeverity::Warning,
                    world_id: project.id.clone(),
                    target_ref: project.repo.display().to_string(),
                    title: format!("Working tree dirty: {uncommitted} uncommitted changes"),
                    rationale: "Uncommitted changes can cause friction during cross-world context switches".into(),
                    proposed_toml_patch: None,
                });
            }
        }

        // Scratchpad coverage
        if project.scratchpad.is_none() {
            let patch = format!(
                "[[projects]]\n# in world '{}':\n[projects.scratchpad]\nenabled = true\nterminal = true\nfloating = true\ncommand = \"foot -a org.task_fabric_svn.world.{}-scratchpad -T '{} (Scratchpad)'\"\n",
                project.id, project.id, project.label
            );
            patterns.push(FrictionPattern {
                kind: FrictionKind::MissingScratchpad,
                severity: FrictionSeverity::Suggestion,
                world_id: project.id.clone(),
                target_ref: format!("special:world-{}", project.id),
                title: "Missing dedicated scratchpad declaration".into(),
                rationale: "Declaring a dedicated scratchpad enables zero-friction terminal overlays with SUPER+S".into(),
                proposed_toml_patch: Some(patch),
            });
        }

        // Action coverage: open-repo
        if !project.actions.iter().any(|a| a.id.eq_ignore_ascii_case("open-repo")) {
            let patch = format!(
                "[[projects.actions]]\nid = \"open-repo\"\nlabel = \"Open repository\"\ncommand = [\"omarchy-launch-editor\", \".\"]\nrisk = \"review\"\n"
            );
            patterns.push(FrictionPattern {
                kind: FrictionKind::MissingAction,
                severity: FrictionSeverity::Suggestion,
                world_id: project.id.clone(),
                target_ref: "open-repo".into(),
                title: "Missing standard 'open-repo' action".into(),
                rationale: "Standardizing 'open-repo' enables instant editor launch via action dispatch and menu provider".into(),
                proposed_toml_patch: Some(patch),
            });
        }

        // Action coverage: task-fabric-svn-test
        if !project.actions.iter().any(|a| a.id.eq_ignore_ascii_case("task-fabric-svn-test")) {
            patterns.push(FrictionPattern {
                kind: FrictionKind::MissingAction,
                severity: FrictionSeverity::Suggestion,
                world_id: project.id.clone(),
                target_ref: "task-fabric-svn-test".into(),
                title: "Missing 'task-fabric-svn-test' durable validation action".into(),
                rationale: "Providing a durable testing action enables the workstation doctor to evaluate durable value".into(),
                proposed_toml_patch: None,
            });
        }

        // Alias coverage
        if project.aliases.is_empty() {
            patterns.push(FrictionPattern {
                kind: FrictionKind::MissingAlias,
                severity: FrictionSeverity::Suggestion,
                world_id: project.id.clone(),
                target_ref: "aliases".into(),
                title: "No shortcut aliases configured".into(),
                rationale: "Adding short aliases allows fast tab-completion and switching via `task-fabric-svn-work enter <alias>`".into(),
                proposed_toml_patch: Some(format!("aliases = [\"{}\"]\n", project.id.split('-').next().unwrap_or(&project.id))),
            });
        }
    }

    let mut overall_friction_score = 0;
    for p in &patterns {
        match p.severity {
            FrictionSeverity::Blocker => overall_friction_score += 10,
            FrictionSeverity::Warning => overall_friction_score += 3,
            FrictionSeverity::Suggestion => overall_friction_score += 1,
        }
    }

    FrictionReport {
        timestamp_epoch_seconds: now,
        patterns,
        overall_friction_score,
        analyzed_events_count: events.len(),
    }
}

// -------------------------------------------------------------------------
// The Tutor: Socratic Active-Recall Drill Generator
// -------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SocraticCategory {
    MachineInvariant,
    CausalLineage,
    StrategicAction,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SocraticQuestion {
    pub category: SocraticCategory,
    pub question: String,
    pub hint: String,
    pub ground_truth: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SocraticDrill {
    pub world_id: String,
    pub world_label: String,
    pub phase: String,
    pub objective: String,
    pub questions: Vec<SocraticQuestion>,
}

pub fn generate_socratic_drill(
    world: &Project,
    events: &[WorldEvent],
    memories: &[String],
) -> SocraticDrill {
    let mut questions = Vec::new();

    // 1. Machine Invariant Question
    let repo_str = world.repo.display().to_string();
    let theme_str = world.theme.as_deref().unwrap_or("Omarchy system default");
    let services_summary = if world.services.is_empty() {
        "none declared".to_string()
    } else {
        world
            .services
            .iter()
            .map(|s| {
                let p = s.port.map(|port| format!(" (port {port})")).unwrap_or_default();
                format!("'{}'{p}", s.id)
            })
            .collect::<Vec<_>>()
            .join(", ")
    };
    let scratchpad_summary = if let Some(sp) = &world.scratchpad {
        if sp.enabled {
            format!("active on special:world-{}", world.id)
        } else {
            "disabled".to_string()
        }
    } else {
        "unconfigured".to_string()
    };

    questions.push(SocraticQuestion {
        category: SocraticCategory::MachineInvariant,
        question: format!(
            "What are the foundational machine invariants for world '{}' (repository location, services, and dedicated scratchpad)?",
            world.label
        ),
        hint: "Inspect the project definition in projects.toml and Hyprland special workspaces.".into(),
        ground_truth: format!(
            "Repository: {repo_str} | Services: {services_summary} | Scratchpad: {scratchpad_summary} | Visual Theme: {theme_str}"
        ),
    });

    // 2. Causal Lineage Question
    let latest_event = events
        .iter()
        .find(|e| e.world_id.as_deref() == Some(&world.id));

    let lineage_answer = if let Some(ev) = latest_event {
        let trace = ev.trace_id.as_deref().unwrap_or("ambient");
        let intent = ev.intent_id.as_deref().unwrap_or("none");
        format!(
            "Latest event: '{}' (kind: {}, trace: {}, intent: {})",
            ev.summary, ev.kind, trace, intent
        )
    } else {
        format!("No prior transition events recorded in world-events.jsonl for world '{}'", world.id)
    };

    questions.push(SocraticQuestion {
        category: SocraticCategory::CausalLineage,
        question: format!(
            "What was the most recent recorded operational event or causal transition recorded for world '{}'?",
            world.label
        ),
        hint: format!("Use `task-fabric-svn-work why {}` to inspect the causal trace and transition receipt.", world.id),
        ground_truth: lineage_answer,
    });

    // 3. Strategic Action & Memory Question
    let memory_context = if let Some(first_mem) = memories.first() {
        format!(" Exocortex memory context: \"{}\"", first_mem.chars().take(100).collect::<String>())
    } else {
        String::new()
    };

    questions.push(SocraticQuestion {
        category: SocraticCategory::StrategicAction,
        question: format!(
            "In phase '{}', what is the strategic objective for '{}', and what is the prescribed next action to advance it?",
            world.phase, world.label
        ),
        hint: "Recall the objective and next_action fields in projects.toml.".into(),
        ground_truth: format!(
            "Objective: '{}' | Next Action: '{}'.{memory_context}",
            world.objective, world.next_action
        ),
    });

    SocraticDrill {
        world_id: world.id.clone(),
        world_label: world.label.clone(),
        phase: world.phase.clone(),
        objective: world.objective.clone(),
        questions,
    }
}

pub fn validate_record<T: Serialize>(record: &T) -> Result<Vec<u8>, String> {
    let bytes = serde_json::to_vec_pretty(record).map_err(|e| e.to_string())?;
    if bytes.len() > 1_000_000 {
        return Err("worldstate record exceeds bounded size".into());
    }
    Ok(bytes)
}

pub fn atomic_write<T: Serialize>(path: &Path, record: &T) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let bytes = validate_record(record).map_err(io::Error::other)?;
    let temp = path.with_extension("tmp");
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(&temp)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    file.write_all(&bytes)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    fs::rename(temp, path)
}

pub fn append_event(event: &WorldEvent) -> io::Result<()> {
    let path = state_dir().join("world-events.jsonl");
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut file = OpenOptions::new().create(true).append(true).open(&path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    // JSONL records must remain one physical line; pretty JSON is reserved
    // for human-readable snapshots and receipts.
    let bytes = serde_json::to_vec(event).map_err(io::Error::other)?;
    file.write_all(&bytes)?;
    file.write_all(b"\n")
}

pub fn read_events(limit: usize) -> io::Result<Vec<WorldEvent>> {
    let path = state_dir().join("world-events.jsonl");
    let text = fs::read_to_string(path).unwrap_or_default();
    let mut values = text
        .lines()
        .filter_map(|line| serde_json::from_str::<WorldEvent>(line).ok())
        .collect::<Vec<_>>();
    values.sort_by(|a, b| b.timestamp_epoch_seconds.cmp(&a.timestamp_epoch_seconds));
    values.truncate(limit.min(MAX_EVENTS));
    Ok(values)
}

/// Read world events in append order for durable consumers. UI timelines use
/// `read_events`' newest-first ordering; publication checkpoints must follow
/// the JSONL log order even when many events share one epoch-second timestamp.
pub fn read_events_append_order(limit: usize) -> io::Result<Vec<WorldEvent>> {
    let path = state_dir().join("world-events.jsonl");
    let text = fs::read_to_string(path).unwrap_or_default();
    let values = text
        .lines()
        .filter_map(|line| serde_json::from_str::<WorldEvent>(line).ok())
        .collect::<Vec<_>>();
    let start = values.len().saturating_sub(limit.min(MAX_EVENTS));
    Ok(values.into_iter().skip(start).collect())
}

pub fn write_receipt(receipt: &WorldTransitionReceipt) -> io::Result<()> {
    let path = state_dir()
        .join("world-transitions")
        .join(format!("{}.json", receipt.id));
    atomic_write(&path, receipt)
}

pub fn read_receipt(id: &str) -> io::Result<Option<WorldTransitionReceipt>> {
    if id.len() > MAX_STRING || id.contains('/') {
        return Ok(None);
    }
    let path = state_dir()
        .join("world-transitions")
        .join(format!("{}.json", id));
    match fs::read(path) {
        Ok(bytes) => Ok(serde_json::from_slice(&bytes).ok()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

pub fn validate_version<T: for<'de> Deserialize<'de>>(bytes: &[u8]) -> Result<T, String> {
    let value: serde_json::Value = serde_json::from_slice(bytes).map_err(|e| e.to_string())?;
    if value.get("schema_version").and_then(|v| v.as_u64())
        != Some(WORLDSTATE_SCHEMA_VERSION as u64)
    {
        return Err("unsupported worldstate schema version".into());
    }
    serde_json::from_value(value).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ResumeEntry;

    #[test]
    fn plan_contains_typed_effects_without_commands() {
        let config = ProjectConfig::example_pilot();
        let project = config.first_project().unwrap();
        let plan = plan_for(project, "snapshot-test");
        assert_eq!(plan.schema_version, WORLDSTATE_SCHEMA_VERSION);
        assert!(plan
            .effects
            .iter()
            .any(|effect| effect.kind == "herdr.restore"));
        let encoded = serde_json::to_string(&plan).unwrap();
        assert!(!encoded.contains("omarchy-launch-tui"));
        assert!(!encoded.contains("command"));
    }

    #[test]
    fn plan_includes_declared_resume_graph_dependencies() {
        let mut config = ProjectConfig::example_pilot();
        let project = config.projects.first_mut().unwrap();
        project.resume.push(ResumeEntry {
            id: "hub-portal".into(),
            kind: "portal".into(),
            resource: Some("hub".into()),
            depends_on: vec!["example-session".into()],
            ..ResumeEntry::default()
        });
        project.resume.push(ResumeEntry { id: "example-session".into(), kind: "agent".into(), session: Some("example".into()), ..ResumeEntry::default() });
        let plan = plan_for(project, "resume-test");
        let portal = plan.effects.iter().find(|e| e.id == "resume-hub-portal").unwrap();
        assert_eq!(portal.dependencies, vec!["resume-example-session"]);
    }

    #[test]
    fn legacy_world_declarations_expand_into_resume_entries() {
        let config = ProjectConfig::example_pilot();
        let project = config.first_project().unwrap();
        let entries = resume_entries_for(project);
        assert!(entries.iter().any(|entry| entry.kind == "agent" && entry.session.as_deref() == Some("example")));
        assert!(entries.iter().any(|entry| entry.kind == "portal" && entry.resource.as_deref() == Some("hub")));
    }

    #[test]
    fn version_validation_rejects_unknown_schema() {
        let bytes = br#"{"schema_version":2,"id":"x"}"#;
        assert!(validate_version::<WorldEvent>(bytes).is_err());
    }

    #[test]
    fn unknown_fields_are_forward_compatible() {
        let bytes = br#"{"schema_version":1,"id":"event-1","future_field":"ignored"}"#;
        let event: WorldEvent = validate_version(bytes).unwrap();
        assert_eq!(event.id, "event-1");
    }

    #[test]
    fn semantic_baseline_reports_changes_without_advancing_on_read() {
        let dir = std::env::temp_dir().join(format!("task-fabric-svn-semantic-baseline-{}", new_id("test")));
        fs::create_dir_all(&dir).unwrap();
        let config = ProjectConfig::example_pilot();
        let first = semantic_context_with_baseline(&config, Some("example-project"), dir.as_path());
        assert!(first["changes_since_last_entry"].as_array().unwrap().is_empty());
        record_semantic_baseline(&first, dir.as_path());
        let mut changed = config.clone();
        changed.projects[0].phase = "review".into();
        let second = semantic_context_with_baseline(&changed, Some("example-project"), dir.as_path());
        assert!(second["changes_since_last_entry"].as_array().unwrap().iter().any(|v| v.as_str().unwrap().contains("phase")));
        let third = semantic_context_with_baseline(&changed, Some("example-project"), dir.as_path());
        assert_eq!(second["changes_since_last_entry"], third["changes_since_last_entry"]);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn semantic_context_is_bounded_and_contains_typed_actions() {
        let config = ProjectConfig::example_pilot();
        let value = semantic_context_for(&config, Some("example-project"));
        assert_eq!(value["schema_version"], "task_fabric_svn.semantic-surface.v1");
        assert_eq!(value["world_id"], "example-project");
        assert!(value["canonical_facts"].as_array().unwrap().iter().any(|fact| fact["fact_id"] == "objective"));
        assert!(value["actions"].as_array().unwrap().iter().any(|action| action["action_id"] == "continue"));
    }

    #[test]
    fn intent_record_and_trace_id_round_trip() {
        let intent = IntentRecord {
            id: "intent-123".into(),
            caller: IntentCaller::VoiceGateway,
            summary: "voice: enter example-paper".into(),
            raw_prompt: Some("switch to the paper world".into()),
            parent_trace_id: Some("parent-trace-0".into()),
            authorization_ref: Some("auth-token-1".into()),
            timestamp_epoch_seconds: 1741797600,
        };
        let config = ProjectConfig::example_pilot();
        let project = config.first_project().unwrap();
        let plan = plan_for_with_intent(
            project,
            "snapshot-1",
            Some("trace-abc".into()),
            Some(intent.clone()),
        );
        assert_eq!(plan.trace_id.as_deref(), Some("trace-abc"));
        assert_eq!(plan.intent.as_ref().unwrap().caller, IntentCaller::VoiceGateway);

        let encoded = serde_json::to_string(&plan).unwrap();
        let decoded: WorldTransitionPlan = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded.trace_id.as_deref(), Some("trace-abc"));
        assert_eq!(decoded.intent.as_ref().unwrap().id, "intent-123");
        assert_eq!(
            decoded.intent.as_ref().unwrap().raw_prompt.as_deref(),
            Some("switch to the paper world")
        );
    }

    #[test]
    fn world_event_carries_trace_and_intent() {
        let event = WorldEvent {
            schema_version: WORLDSTATE_SCHEMA_VERSION,
            id: "event-trace-test".into(),
            timestamp_epoch_seconds: 1741797600,
            world_id: Some("example-paper".into()),
            transition_id: Some("transition-1".into()),
            trace_id: Some("trace-xyz".into()),
            intent_id: Some("intent-123".into()),
            kind: "service.ensure".into(),
            source: "control_plane".into(),
            summary: "Service verified".into(),
            object_ref: None,
            effect_ref: Some("effect-1".into()),
            outcome: Some(EffectOutcome::Completed),
            presentation: None,
            evidence_refs: vec!["evidence-1".into()],
        };
        let encoded = serde_json::to_string(&event).unwrap();
        let decoded: WorldEvent = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded.trace_id.as_deref(), Some("trace-xyz"));
        assert_eq!(decoded.intent_id.as_deref(), Some("intent-123"));
    }

    #[test]
    fn simulation_report_evaluates_machine_preconditions() {
        let mut config = ProjectConfig::example_pilot();
        config.projects[0].resume.push(crate::ResumeEntry { id: "session".into(), kind: "agent".into(), session: Some("example".into()), required: true, ..crate::ResumeEntry::default() });
        let project = config.first_project().unwrap();
        let report = simulate_transition(project, Some("example-project"), None);
        assert_eq!(report.world_id, "example-project");
        assert_eq!(report.world_label, "Example Project");
        assert_eq!(report.requested_world_id, report.world_id);
        assert_eq!(report.projected_active_world_id, report.world_id);
        assert!(report.required_approval);
        assert!(!report.expected_receipt.is_empty());
        assert!(report.preflight_checks.iter().any(|c| c.category == "filesystem"));
        assert!(report.predicted_effects.iter().any(|e| e.id == "record-intent"));
        assert!(report.predicted_effects.iter().any(|e| e.id == "validate-world"));
        assert!(report.predicted_effects.iter().any(|e| e.id == "resume-session"));
        let json = serde_json::to_string(&report).unwrap();
        assert!(json.contains("predicted_effects"));
    }

    #[test]
    fn snapshot_keeps_selection_request_and_activation_distinct() {
        let config = ProjectConfig::example_pilot();
        let snapshot = snapshot_for_with_state(
            &config,
            Some("example-paper"),
            Some("example-paper"),
            Some("example-project"),
            None,
            None,
        );
        assert_eq!(snapshot.selected_world_id.as_deref(), Some("example-paper"));
        assert_eq!(snapshot.requested_world_id.as_deref(), Some("example-paper"));
        assert_eq!(snapshot.active_world_id.as_deref(), Some("example-project"));
        assert_eq!(snapshot.world_activation_status, "degraded");
    }

    #[test]
    fn snapshot_marks_failed_transition_and_preserves_trace() {
        let config = ProjectConfig::example_pilot();
        let last = TransitionReference {
            id: "transition-failed".into(),
            outcome: "failed".into(),
            started_at_epoch_seconds: 1,
            finished_at_epoch_seconds: Some(2),
            trace_id: Some("trace-failed".into()),
        };
        let snapshot = snapshot_for_with_state(
            &config,
            Some("example-paper"),
            Some("example-paper"),
            Some("example-project"),
            None,
            Some(last),
        );
        assert_eq!(snapshot.world_activation_status, "failed");
        assert_eq!(snapshot.active_trace_id.as_deref(), Some("trace-failed"));
    }

    #[test]
    fn snapshot_exposes_live_repository_observation_per_world() {
        let mut config = ProjectConfig::example_pilot();
        config.projects[0].theme = Some("Quantum Heron".into());
        let snapshot = snapshot_for_with_state(&config, Some("example-project"), Some("example-project"), Some("example-project"), None, None);
        let world = snapshot.worlds.iter().find(|world| world.id == "example-project").unwrap();
        assert!(world.observation.observed_at_epoch_seconds > 0);
        assert!(world.observation.head_commit.is_some() || !world.repository_exists);
        assert!(world.observation.portals.iter().any(|portal| portal.resource_id == "hub"));
        assert_eq!(world.observation.herdr_session.as_deref(), Some("example"));
        assert!(world.observation.herdr_reachable.is_some());
        assert_eq!(world.observation.requested_theme.as_deref(), Some("Quantum Heron"));
        assert!(world.observation.theme_matches.is_some());
        assert!(world.observation.services.iter().any(|service| service.service_id == "edged"));
    }

    #[test]
    fn action_dry_run_checks_binary_and_cwd() {
        let config = ProjectConfig::example_pilot();
        let project = config.first_project().unwrap();
        let report = simulate_action_dry_run(project, "open-repo").unwrap();
        assert_eq!(report.action_id, "open-repo");
        assert_eq!(report.risk, "review");
        assert!(report.command.contains(&"omarchy-launch-editor".to_string()));
    }

    #[test]
    fn friction_analysis_detects_patterns_and_patches() {
        let config = ProjectConfig::example_pilot();
        let report = analyze_workstation_friction(&config, &[], None);
        assert!(report.patterns.len() > 0);
        assert!(report.patterns.iter().any(|p| p.kind == FrictionKind::MissingScratchpad));
        let scratchpad_pat = report.patterns.iter().find(|p| p.kind == FrictionKind::MissingScratchpad).unwrap();
        assert!(scratchpad_pat.proposed_toml_patch.is_some());
    }

    #[test]
    fn socratic_drill_generates_three_grounded_questions() {
        let config = ProjectConfig::example_pilot();
        let project = config.first_project().unwrap();
        let drill = generate_socratic_drill(project, &[], &["Key decision memory".into()]);
        assert_eq!(drill.world_id, "example-project");
        assert_eq!(drill.questions.len(), 3);
        assert_eq!(drill.questions[0].category, SocraticCategory::MachineInvariant);
        assert_eq!(drill.questions[1].category, SocraticCategory::CausalLineage);
        assert_eq!(drill.questions[2].category, SocraticCategory::StrategicAction);
    }
}
