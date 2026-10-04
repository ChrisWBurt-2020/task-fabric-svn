//! Typed, fail-closed records for a bounded Governor improvement experiment.
//!
//! This module owns the workstation's experiment lifecycle and lineage only.
//! Governor run-state, router receipts, and role verdicts remain external
//! evidence and must be referenced by stable digests rather than copied here.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

pub const EXPERIMENT_SCHEMA_VERSION: u32 = 1;
const MAX_MANIFEST_FILE_BYTES: u64 = 8 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ManifestEntry {
    pub kind: String,
    pub bytes: u64,
    pub sha256: Option<String>,
    pub symlink_target: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BaselineManifest {
    pub schema_version: u32,
    pub entries: BTreeMap<String, ManifestEntry>,
    pub omitted_paths: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GovernorCriterionSummary {
    pub id: String,
    pub status: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GovernorRunStateSummary {
    pub schema: u32,
    #[serde(rename = "runId")]
    pub run_id: String,
    pub project: String,
    #[serde(default)]
    pub scope: String,
    pub status: String,
    #[serde(rename = "acceptance_criteria", default)]
    pub acceptance_criteria: Vec<GovernorCriterionSummary>,
    #[serde(default)]
    pub session_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GovernorTelemetrySummary {
    pub source_digest: String,
    pub total_events: u64,
    pub intervention_events: u64,
    pub malformed_lines: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct InterventionOccurrence {
    pub occurrence_id: String,
    pub session_id: String,
    pub heuristic: String,
    pub evidence: String,
    pub first_at: String,
    pub queued_count: u64,
    pub delivered_count: u64,
    pub owner_impact: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct InterventionMiningReport {
    pub source_digest: String,
    pub raw_intervention_events: u64,
    pub unique_occurrences: Vec<InterventionOccurrence>,
    pub malformed_lines: u64,
}

/// Minimal, redacted shape accepted from a 9router execution-receipt export.
/// The adapter intentionally requires an explicit experiment identity before
/// usage can contribute to cost accounting; timestamp proximity is never
/// treated as attribution.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RouterReceiptSummary {
    pub source_digest: String,
    pub total_rows: u64,
    pub attributed_rows: u64,
    pub unattributed_rows: u64,
    pub malformed_lines: u64,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub cost_micros: Option<u64>,
    pub unknown_cost_rows: u64,
    pub execution_ids: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct OpenCodeEventSummary {
    pub source_digest: String,
    pub total_events: u64,
    pub malformed_lines: u64,
    pub session_ids: Vec<String>,
    pub step_finish_events: u64,
    pub latest_total_tokens: Option<u64>,
    pub latest_input_tokens: Option<u64>,
    pub latest_output_tokens: Option<u64>,
    pub latest_reasoning_tokens: Option<u64>,
}

pub fn summarize_opencode_events(path: &Path) -> io::Result<OpenCodeEventSummary> {
    let bytes = fs::read(path)?;
    let text = String::from_utf8_lossy(&bytes);
    let mut summary = OpenCodeEventSummary {
        source_digest: hex_digest(&bytes),
        total_events: 0,
        malformed_lines: 0,
        session_ids: Vec::new(),
        step_finish_events: 0,
        latest_total_tokens: None,
        latest_input_tokens: None,
        latest_output_tokens: None,
        latest_reasoning_tokens: None,
    };
    for line in text.lines().filter(|line| !line.trim().is_empty()) {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            summary.malformed_lines += 1;
            continue;
        };
        summary.total_events += 1;
        if let Some(session_id) = string_field(&value, &["sessionID", "session_id"]) {
            summary.session_ids.push(session_id);
        }
        if value.get("type").and_then(serde_json::Value::as_str) != Some("step_finish") {
            continue;
        }
        summary.step_finish_events += 1;
        let Some(tokens) = value.get("part").and_then(|part| part.get("tokens")) else {
            continue;
        };
        summary.latest_total_tokens = u64_field(tokens, &["total"]);
        summary.latest_input_tokens = u64_field(tokens, &["input"]);
        summary.latest_output_tokens = u64_field(tokens, &["output"]);
        summary.latest_reasoning_tokens = u64_field(tokens, &["reasoning"]);
    }
    summary.session_ids.sort();
    summary.session_ids.dedup();
    Ok(summary)
}

fn string_field(value: &serde_json::Value, names: &[&str]) -> Option<String> {
    names.iter().find_map(|name| {
        value
            .get(*name)
            .and_then(serde_json::Value::as_str)
            .map(ToOwned::to_owned)
    })
}

fn u64_field(value: &serde_json::Value, names: &[&str]) -> Option<u64> {
    names.iter().find_map(|name| {
        value.get(*name).and_then(|candidate| {
            candidate
                .as_u64()
                .or_else(|| candidate.as_str().and_then(|text| text.parse().ok()))
        })
    })
}

fn experiment_identity_field(value: &serde_json::Value) -> Option<String> {
    string_field(value, &["experiment_id", "experimentId"]).or_else(|| {
        value
            .get("task_fabric_svn")
            .and_then(|task_fabric_svn| string_field(task_fabric_svn, &["work_id", "workId"]))
    })
}

fn decimal_micros(value: &serde_json::Value) -> Option<u64> {
    let text = value.as_str().map(str::to_owned).or_else(|| {
        value.as_number().map(ToString::to_string)
    })?;
    let text = text.strip_prefix('+').unwrap_or(&text);
    if text.starts_with('-') {
        return None;
    }
    if text.contains('e') || text.contains('E') {
        let amount = text.parse::<f64>().ok()?;
        if !amount.is_finite() || amount < 0.0 {
            return None;
        }
        let micros = (amount * 1_000_000.0).round();
        return (micros <= u64::MAX as f64).then_some(micros as u64);
    }
    let (whole, fraction) = text.split_once('.').unwrap_or((text, ""));
    let whole = whole.parse::<u64>().ok()?;
    if fraction.chars().any(|character| !character.is_ascii_digit()) {
        return None;
    }
    let mut digits = fraction.chars();
    let mut micros = 0_u64;
    for _ in 0..6 {
        micros = micros.saturating_mul(10).saturating_add(
            digits.next().and_then(|character| character.to_digit(10)).unwrap_or(0) as u64,
        );
    }
    if digits.next().and_then(|character| character.to_digit(10)).unwrap_or(0) >= 5 {
        micros = micros.saturating_add(1);
    }
    whole
        .checked_mul(1_000_000)
        .and_then(|amount| amount.checked_add(micros))
}

fn receipt_usage_field(value: &serde_json::Value, names: &[&str]) -> Option<u64> {
    u64_field(value, names).or_else(|| value.get("usage").and_then(|usage| u64_field(usage, names)))
}

fn receipt_cost_micros(value: &serde_json::Value) -> Option<u64> {
    u64_field(value, &["cost_micros", "costMicros"]).or_else(|| {
        let cost = value.get("cost")?;
        let state = string_field(cost, &["state"])?;
        if matches!(state.as_str(), "unknown" | "") {
            return None;
        }
        decimal_micros(cost.get("amount")?)
    })
}

/// Summarize a newline-delimited redacted receipt export. Only rows carrying
/// `experiment_id`/`experimentId` equal to `expected_experiment_id` are
/// attributable. Missing cost remains unknown and is counted explicitly.
pub fn summarize_router_receipts(
    path: &Path,
    expected_experiment_id: &str,
) -> io::Result<RouterReceiptSummary> {
    let bytes = fs::read(path)?;
    let text = String::from_utf8_lossy(&bytes);
    let mut summary = RouterReceiptSummary {
        source_digest: hex_digest(&bytes),
        total_rows: 0,
        attributed_rows: 0,
        unattributed_rows: 0,
        malformed_lines: 0,
        prompt_tokens: 0,
        completion_tokens: 0,
        cost_micros: Some(0),
        unknown_cost_rows: 0,
        execution_ids: Vec::new(),
    };
    for line in text.lines().filter(|line| !line.trim().is_empty()) {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            summary.malformed_lines += 1;
            continue;
        };
        summary.total_rows += 1;
        // Native 9router receipts carry Task Fabric SVN correlation fields under
        // `task_fabric_svn`; the lab uses work_id as the explicit experiment slot.
        // Bounded exports may carry experiment_id directly.
        let experiment_id = experiment_identity_field(&value);
        if experiment_id.as_deref() != Some(expected_experiment_id) {
            summary.unattributed_rows += 1;
            continue;
        }
        summary.attributed_rows += 1;
        if let Some(execution_id) = string_field(&value, &["execution_id", "executionId"]) {
            summary.execution_ids.push(execution_id);
        }
        summary.prompt_tokens +=
            receipt_usage_field(&value, &["prompt_tokens", "promptTokens"]).unwrap_or(0);
        summary.completion_tokens +=
            receipt_usage_field(&value, &["completion_tokens", "completionTokens"]).unwrap_or(0);
        if let Some(cost) = receipt_cost_micros(&value) {
            if let Some(total) = summary.cost_micros.as_mut() {
                *total = total.saturating_add(cost);
            }
        } else {
            summary.unknown_cost_rows += 1;
            summary.cost_micros = None;
        }
    }
    summary.execution_ids.sort();
    summary.execution_ids.dedup();
    Ok(summary)
}

/// Attach a receipt snapshot to an evaluation's usage and monetary fields.
/// Every row must belong to the experiment and carry a known cost; otherwise
/// the evaluation remains incomplete rather than silently treating a partial
/// export as the experiment total.
pub fn attach_router_receipt_cost(
    record: &mut ExperimentRecord,
    path: &Path,
    acquired_at_epoch_seconds: u64,
) -> io::Result<RouterReceiptSummary> {
    let summary = summarize_router_receipts(path, &record.identity.experiment_id)?;
    if summary.total_rows == 0 {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "router receipt export is empty"));
    }
    if summary.malformed_lines != 0
        || summary.unattributed_rows != 0
        || summary.attributed_rows != summary.total_rows
        || summary.unknown_cost_rows != 0
        || summary.cost_micros.is_none()
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "router receipt export is not a complete identity-matched cost set",
        ));
    }
    let evidence = EvidenceRef {
        id: format!("router-receipts:{}", summary.source_digest),
        digest: summary.source_digest.clone(),
        acquired_at_epoch_seconds,
        state: EvidenceState::Verified,
    };
    let evaluation = record
        .evaluation
        .as_mut()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "experiment has no evaluation"))?;
    evaluation.cost.usage_tokens = Some(
        summary
            .prompt_tokens
            .saturating_add(summary.completion_tokens),
    );
    evaluation.cost.monetary_micros = summary.cost_micros;
    if !evaluation.cost.evidence.iter().any(|item| item.id == evidence.id) {
        evaluation.cost.evidence.push(evidence.clone());
    }
    evaluation.cost.unknown_components.retain(|item| {
        !item.contains("router") && !item.contains("usage") && !item.contains("monetary")
    });
    if !record.trial_artifacts.iter().any(|item| item.id == evidence.id) {
        record.trial_artifacts.push(evidence);
    }
    Ok(summary)
}

/// Read only the stable Governor fields needed for experiment association.
/// Event bodies and role prompts are intentionally not copied into the
/// workstation record.
pub fn load_governor_run_state(path: &Path) -> io::Result<GovernorRunStateSummary> {
    let bytes = fs::read(path)?;
    serde_json::from_slice(&bytes)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

pub fn summarize_governor_telemetry(path: &Path) -> io::Result<GovernorTelemetrySummary> {
    let bytes = fs::read(path)?;
    let text = String::from_utf8_lossy(&bytes);
    let mut total_events = 0;
    let mut intervention_events = 0;
    let mut malformed_lines = 0;
    for line in text.lines().filter(|line| !line.trim().is_empty()) {
        match serde_json::from_str::<serde_json::Value>(line) {
            Ok(event) => {
                total_events += 1;
                if event.get("kind").and_then(|value| value.as_str()) == Some("intervention") {
                    intervention_events += 1;
                }
            }
            Err(_) => malformed_lines += 1,
        }
    }
    Ok(GovernorTelemetrySummary {
        source_digest: hex_digest(&bytes),
        total_events,
        intervention_events,
        malformed_lines,
    })
}

/// Mine only intervention metadata and collapse Governor's queued/delivered
/// pair into one occurrence. Owner impact is intentionally unknown until a
/// replay or matched trial proves it.
pub fn mine_governor_interventions(path: &Path) -> io::Result<InterventionMiningReport> {
    let bytes = fs::read(path)?;
    let text = String::from_utf8_lossy(&bytes);
    let mut occurrences: BTreeMap<String, InterventionOccurrence> = BTreeMap::new();
    let mut raw_intervention_events = 0;
    let mut malformed_lines = 0;
    for line in text.lines().filter(|line| !line.trim().is_empty()) {
        let Ok(event) = serde_json::from_str::<serde_json::Value>(line) else {
            malformed_lines += 1;
            continue;
        };
        if event.get("kind").and_then(|value| value.as_str()) != Some("intervention") {
            continue;
        }
        raw_intervention_events += 1;
        let session_id = event
            .get("sessionID")
            .or_else(|| event.get("session_id"))
            .and_then(|value| value.as_str())
            .unwrap_or("unknown")
            .to_string();
        let heuristic = event
            .get("heuristic")
            .and_then(|value| value.as_str())
            .unwrap_or("unknown")
            .to_string();
        let evidence = event
            .get("evidence")
            .and_then(|value| value.as_str())
            .unwrap_or("")
            .to_string();
        let key = format!("{session_id}\0{heuristic}\0{evidence}");
        let occurrence_id = hex_digest(key.as_bytes());
        let at = event
            .get("at")
            .and_then(|value| value.as_str())
            .unwrap_or("")
            .to_string();
        let delivery = event
            .get("delivery")
            .and_then(|value| value.as_str())
            .unwrap_or("unknown");
        let entry = occurrences
            .entry(key)
            .or_insert_with(|| InterventionOccurrence {
                occurrence_id,
                session_id,
                heuristic,
                evidence,
                first_at: at,
                queued_count: 0,
                delivered_count: 0,
                owner_impact: "unknown_until_replay_or_trial".into(),
            });
        match delivery {
            "queued" => entry.queued_count += 1,
            "delivered" => entry.delivered_count += 1,
            _ => {}
        }
    }
    Ok(InterventionMiningReport {
        source_digest: hex_digest(&bytes),
        raw_intervention_events,
        unique_occurrences: occurrences.into_values().collect(),
        malformed_lines,
    })
}

impl BaselineManifest {
    pub fn digest(&self) -> String {
        let bytes = serde_json::to_vec(self).expect("manifest serialization is infallible");
        hex_digest(&bytes)
    }
}

fn hex_digest(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn sensitive_path(path: &Path) -> bool {
    let lower = path.to_string_lossy().to_ascii_lowercase();
    lower.contains("/secrets/")
        || lower.ends_with("/auth.json")
        || lower.contains("token")
        || lower.contains("password")
        || lower.contains("credential")
}

fn add_manifest_path(
    path: &Path,
    entries: &mut BTreeMap<String, ManifestEntry>,
    omitted: &mut Vec<String>,
) -> io::Result<()> {
    if sensitive_path(path) {
        omitted.push(path.display().to_string());
        return Ok(());
    }
    let metadata = fs::symlink_metadata(path)?;
    let key = path.display().to_string();
    if metadata.file_type().is_symlink() {
        entries.insert(
            key,
            ManifestEntry {
                kind: "symlink".into(),
                bytes: 0,
                sha256: None,
                symlink_target: fs::read_link(path)
                    .ok()
                    .map(|target| target.display().to_string()),
            },
        );
    } else if metadata.is_file() {
        if metadata.len() > MAX_MANIFEST_FILE_BYTES {
            omitted.push(format!(
                "{} (file exceeds manifest size limit)",
                path.display()
            ));
        } else {
            let bytes = fs::read(path)?;
            entries.insert(
                key,
                ManifestEntry {
                    kind: "file".into(),
                    bytes: bytes.len() as u64,
                    sha256: Some(hex_digest(&bytes)),
                    symlink_target: None,
                },
            );
        }
    } else if metadata.is_dir() {
        for child in fs::read_dir(path)? {
            add_manifest_path(&child?.path(), entries, omitted)?;
        }
    } else {
        omitted.push(format!("{} (unsupported file type)", path.display()));
    }
    Ok(())
}

/// Build a deterministic manifest from owner-selected paths. Sensitive files
/// are recorded as omitted, never read, and never hashed.
pub fn manifest_for_paths(paths: &[PathBuf]) -> io::Result<BaselineManifest> {
    let mut entries = BTreeMap::new();
    let mut omitted_paths = Vec::new();
    for path in paths {
        add_manifest_path(path, &mut entries, &mut omitted_paths)?;
    }
    omitted_paths.sort();
    Ok(BaselineManifest {
        schema_version: EXPERIMENT_SCHEMA_VERSION,
        entries,
        omitted_paths,
    })
}

fn safe_experiment_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

pub fn experiment_record_path(state: &Path, experiment_id: &str) -> io::Result<PathBuf> {
    if !safe_experiment_id(experiment_id) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid experiment id",
        ));
    }
    Ok(state
        .join("experiments")
        .join(format!("{experiment_id}.json")))
}

pub fn baseline_manifest_path(state: &Path, manifest_id: &str) -> io::Result<PathBuf> {
    if !safe_experiment_id(manifest_id) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid manifest id",
        ));
    }
    Ok(state
        .join("experiments")
        .join(format!("{manifest_id}.baseline.json")))
}

pub fn save_manifest(
    state: &Path,
    manifest_id: &str,
    manifest: &BaselineManifest,
) -> io::Result<()> {
    let path = baseline_manifest_path(state, manifest_id)?;
    crate::worldstate::atomic_write(&path, manifest)
}

pub fn save_record(state: &Path, record: &ExperimentRecord) -> io::Result<()> {
    let path = experiment_record_path(state, &record.identity.experiment_id)?;
    crate::worldstate::atomic_write(&path, record)
}

pub fn load_record(state: &Path, experiment_id: &str) -> io::Result<Option<ExperimentRecord>> {
    let path = experiment_record_path(state, experiment_id)?;
    match fs::read(&path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

/// Hash a bounded external cost ledger or receipt snapshot without copying
/// its contents into the experiment record.
pub fn evidence_ref_for_file(
    path: &Path,
    id: impl Into<String>,
    acquired_at_epoch_seconds: u64,
    state: EvidenceState,
) -> io::Result<EvidenceRef> {
    let bytes = fs::read(path)?;
    Ok(EvidenceRef {
        id: id.into(),
        digest: hex_digest(&bytes),
        acquired_at_epoch_seconds,
        state,
    })
}

pub fn attach_trial_artifact(
    record: &mut ExperimentRecord,
    path: &Path,
    id: impl Into<String>,
    acquired_at_epoch_seconds: u64,
    state: EvidenceState,
) -> io::Result<EvidenceRef> {
    let evidence = evidence_ref_for_file(path, id, acquired_at_epoch_seconds, state)?;
    if let Some(existing) = record.trial_artifacts.iter().find(|item| item.id == evidence.id) {
        return Ok(existing.clone());
    }
    record.trial_artifacts.push(evidence.clone());
    Ok(evidence)
}

/// Attach a successful worker result only when every experiment identity is
/// exact. A result that lacks identity, failed, or ran under another manifest
/// is retained outside the record and fails closed here.
pub fn attach_worker_trial_result(
    record: &mut ExperimentRecord,
    path: &Path,
    expected_release_manifest_digest: Option<&str>,
    acquired_at_epoch_seconds: u64,
) -> io::Result<EvidenceRef> {
    let bytes = fs::read(path)?;
    let result: WorkerTrialResultSummary = serde_json::from_slice(&bytes)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    if result.schema_version != EXPERIMENT_SCHEMA_VERSION || result.status != "succeeded" {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "worker result is not a successful supported result"));
    }
    if result.experiment_id.as_deref() != Some(record.identity.experiment_id.as_str())
        || result.run_id.as_deref() != Some(record.identity.run_id.as_str())
        || result.session_id.as_deref() != Some(record.identity.session_id.as_str())
        || result.task_revision.as_deref() != Some(record.identity.task_revision.as_str())
        || result.harness_manifest_digest.as_deref() != Some(record.identity.harness_manifest_digest.as_str())
    {
        return Err(io::Error::new(io::ErrorKind::PermissionDenied, "worker result identity does not match experiment"));
    }
    if let Some(expected) = expected_release_manifest_digest {
        if result.release_manifest_digest.as_deref() != Some(expected) {
            return Err(io::Error::new(io::ErrorKind::PermissionDenied, "worker result release manifest does not match expected artifact"));
        }
    }
    let evidence = EvidenceRef {
        id: format!("worker-result:{}", result.job_id),
        digest: hex_digest(&bytes),
        acquired_at_epoch_seconds,
        state: EvidenceState::Verified,
    };
    if let Some(existing) = record.trial_artifacts.iter().find(|item| item.id == evidence.id) {
        return Ok(existing.clone());
    }
    record.trial_artifacts.push(evidence.clone());
    Ok(evidence)
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ExperimentState {
    Proposed,
    Evaluating,
    Inconclusive,
    Rejected,
    AwaitingApproval,
    Approved,
    Releasing,
    Active,
    RolledBack,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceState {
    Verified,
    Accepted,
    Inferred,
    Incomplete,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EvidenceRef {
    pub id: String,
    pub digest: String,
    pub acquired_at_epoch_seconds: u64,
    pub state: EvidenceState,
}

/// Bounded result shape emitted by the separately supervised trial worker.
/// The worker's full stdout/stderr remains outside the experiment record.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkerTrialResultSummary {
    pub schema_version: u32,
    pub job_id: String,
    pub experiment_id: Option<String>,
    pub trial_id: Option<String>,
    pub run_id: Option<String>,
    pub session_id: Option<String>,
    pub task_revision: Option<String>,
    pub harness_manifest_digest: Option<String>,
    pub status: String,
    pub release_manifest_digest: Option<String>,
    #[serde(default)]
    pub router_receipt_source_digest: Option<String>,
    #[serde(default)]
    pub router_prompt_tokens: Option<u64>,
    #[serde(default)]
    pub router_completion_tokens: Option<u64>,
    #[serde(default)]
    pub router_cost_micros: Option<u64>,
    #[serde(default)]
    pub router_unknown_cost_rows: Option<u64>,
    #[serde(default)]
    pub router_receipts_error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ExperimentIdentity {
    pub experiment_id: String,
    pub case_id: String,
    pub candidate_id: String,
    pub run_id: String,
    pub session_id: String,
    pub task_revision: String,
    pub harness_manifest_digest: String,
    pub trace_id: Option<String>,
}

/// Cost coverage is part of the acceptance contract. Missing usage or price
/// data is represented as `None`, never as zero, so an experiment cannot be
/// approved while its total budget is only partially observed.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CostAccounting {
    pub acquisition_seconds: Option<u64>,
    pub generation_seconds: Option<u64>,
    pub evaluation_seconds: Option<u64>,
    pub review_seconds: Option<u64>,
    pub maintenance_seconds: Option<u64>,
    pub usage_tokens: Option<u64>,
    pub monetary_micros: Option<u64>,
    #[serde(default)]
    pub evidence: Vec<EvidenceRef>,
    #[serde(default)]
    pub unknown_components: Vec<String>,
}

impl Default for CostAccounting {
    fn default() -> Self {
        Self {
            acquisition_seconds: None,
            generation_seconds: None,
            evaluation_seconds: None,
            review_seconds: None,
            maintenance_seconds: None,
            usage_tokens: None,
            monetary_micros: None,
            evidence: Vec::new(),
            unknown_components: vec!["cost coverage not recorded".into()],
        }
    }
}

impl CostAccounting {
    pub fn is_complete(&self) -> bool {
        self.acquisition_seconds.is_some()
            && self.generation_seconds.is_some()
            && self.evaluation_seconds.is_some()
            && self.review_seconds.is_some()
            && self.maintenance_seconds.is_some()
            && self.usage_tokens.is_some()
            && self.monetary_micros.is_some()
            && !self.evidence.is_empty()
            && self.unknown_components.is_empty()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EvaluationSummary {
    pub evaluator_digest: String,
    pub results_digest: String,
    pub primary_friction_measure: String,
    pub baseline_value: Option<u64>,
    pub candidate_value: Option<u64>,
    pub quality_regression: bool,
    pub essential_escalation_regression: bool,
    pub matched_conditions: bool,
    pub evidence_state: EvidenceState,
    #[serde(default)]
    pub cost: CostAccounting,
}

impl EvaluationSummary {
    /// Approval cannot turn missing or mismatched evidence into a win.
    pub fn is_eligible_for_approval(&self) -> bool {
        self.matched_conditions
            && !self.quality_regression
            && !self.essential_escalation_regression
            && self.evidence_state == EvidenceState::Verified
            && self.baseline_value.is_some()
            && self.candidate_value.is_some()
            && self.cost.is_complete()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReleaseAuthorization {
    pub candidate_id: String,
    pub parent_manifest_digest: String,
    pub evaluator_digest: String,
    pub results_digest: String,
    pub target: String,
    pub expires_at_epoch_seconds: u64,
    #[serde(default)]
    pub owner_id: String,
    #[serde(default)]
    pub review_reference: String,
    #[serde(default)]
    pub approved_at_epoch_seconds: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ActiveReleasePointer {
    pub schema_version: u32,
    pub target: String,
    pub active_manifest_digest: String,
    pub predecessor_manifest_digest: Option<String>,
    pub activated_at_epoch_seconds: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StagedRelease {
    pub schema_version: u32,
    pub target: String,
    pub candidate_manifest_digest: String,
    pub parent_manifest_digest: String,
    pub evaluator_digest: String,
    pub results_digest: String,
    pub source_path: String,
    pub staged_path: String,
    pub staged_at_epoch_seconds: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReleaseLoadProof {
    pub schema_version: u32,
    pub target: String,
    pub candidate_manifest_digest: String,
    pub run_id: String,
    pub loaded_at_epoch_seconds: u64,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CanaryState {
    Running,
    Passed,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CanaryRunObservation {
    pub run_id: String,
    pub task_revision: String,
    pub manifest_digest: String,
    pub outcome_digest: String,
    pub quality_regression: bool,
    pub observed_at_epoch_seconds: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CanaryWindow {
    pub manifest_digest: String,
    pub started_at_epoch_seconds: u64,
    pub ends_at_epoch_seconds: u64,
    pub required_fresh_runs: u32,
    pub state: CanaryState,
    pub observations: Vec<CanaryRunObservation>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ExperimentRecord {
    pub schema_version: u32,
    pub state: ExperimentState,
    pub identity: ExperimentIdentity,
    pub parent_manifest_digest: String,
    pub candidate_manifest_digest: String,
    pub hypothesis: String,
    pub predicted_observable_effect: String,
    pub evidence: Vec<EvidenceRef>,
    #[serde(default)]
    pub trial_artifacts: Vec<EvidenceRef>,
    pub evaluation: Option<EvaluationSummary>,
    pub decision: Option<String>,
    pub successor_manifest_digest: Option<String>,
    pub rollback_parent_digest: Option<String>,
    pub release_authorization: Option<ReleaseAuthorization>,
    #[serde(default)]
    pub parent_experiment_id: Option<String>,
    #[serde(default)]
    pub canary: Option<CanaryWindow>,
}

impl ExperimentRecord {
    pub fn new(
        identity: ExperimentIdentity,
        parent: String,
        candidate: String,
        hypothesis: String,
        effect: String,
    ) -> Self {
        Self {
            schema_version: EXPERIMENT_SCHEMA_VERSION,
            state: ExperimentState::Proposed,
            identity,
            parent_manifest_digest: parent,
            candidate_manifest_digest: candidate,
            hypothesis,
            predicted_observable_effect: effect,
            evidence: Vec::new(),
            trial_artifacts: Vec::new(),
            evaluation: None,
            decision: None,
            successor_manifest_digest: None,
            rollback_parent_digest: None,
            release_authorization: None,
            parent_experiment_id: None,
            canary: None,
        }
    }

    pub fn transition(&mut self, next: ExperimentState) -> Result<(), String> {
        let allowed = matches!(
            (self.state, next),
            (ExperimentState::Proposed, ExperimentState::Evaluating)
                | (ExperimentState::Inconclusive, ExperimentState::Evaluating)
                | (ExperimentState::Rejected, ExperimentState::Evaluating)
                | (ExperimentState::Evaluating, ExperimentState::Inconclusive)
                | (ExperimentState::Evaluating, ExperimentState::Rejected)
                | (
                    ExperimentState::Evaluating,
                    ExperimentState::AwaitingApproval
                )
                | (ExperimentState::AwaitingApproval, ExperimentState::Approved)
                | (ExperimentState::AwaitingApproval, ExperimentState::Rejected)
                | (ExperimentState::AwaitingApproval, ExperimentState::Inconclusive)
                | (ExperimentState::Approved, ExperimentState::Releasing)
                | (ExperimentState::Releasing, ExperimentState::Active)
                | (ExperimentState::Releasing, ExperimentState::RolledBack)
                | (ExperimentState::Active, ExperimentState::RolledBack)
        );
        if !allowed {
            return Err(format!(
                "invalid experiment transition {:?} -> {:?}",
                self.state, next
            ));
        }
        if next == ExperimentState::AwaitingApproval
            && !self
                .evaluation
                .as_ref()
                .is_some_and(EvaluationSummary::is_eligible_for_approval)
        {
            return Err("evaluation is not eligible for approval".into());
        }
        if next == ExperimentState::Approved && self.release_authorization.is_none() {
            return Err("approval requires a release authorization".into());
        }
        if next == ExperimentState::Approved
            && !self
                .evaluation
                .as_ref()
                .is_some_and(EvaluationSummary::is_eligible_for_approval)
        {
            return Err("approval requires complete, verified evaluation and cost accounting".into());
        }
        self.state = next;
        Ok(())
    }

    pub fn authorization_matches(&self, now: u64, target: &str) -> bool {
        let Some(auth) = &self.release_authorization else {
            return false;
        };
        let Some(eval) = &self.evaluation else {
            return false;
        };
        auth.candidate_id == self.identity.candidate_id
            && auth.parent_manifest_digest == self.parent_manifest_digest
            && auth.evaluator_digest == eval.evaluator_digest
            && auth.results_digest == eval.results_digest
            && auth.target == target
            && now <= auth.expires_at_epoch_seconds
            && !auth.owner_id.is_empty()
            && !auth.review_reference.is_empty()
            && auth.approved_at_epoch_seconds > 0
    }

    /// Deny the exact evaluated artifact and invalidate any pending release
    /// authorization so it cannot be replayed after an owner decision.
    pub fn deny(&mut self, reason: impl Into<String>) -> Result<(), String> {
        if self.state != ExperimentState::AwaitingApproval {
            return Err("only an experiment awaiting approval can be denied".into());
        }
        self.release_authorization = None;
        self.decision = Some(format!("rejected: {}", reason.into()));
        self.transition(ExperimentState::Rejected)
    }
}

fn safe_release_target(target: &str) -> bool {
    !target.is_empty()
        && target.len() <= 128
        && target
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

pub fn release_pointer_path(state: &Path, target: &str) -> io::Result<PathBuf> {
    if !safe_release_target(target) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid release target",
        ));
    }
    Ok(state
        .join("experiments")
        .join(format!("{target}.active-release.json")))
}

fn staged_release_path(state: &Path, target: &str, candidate: &str) -> io::Result<PathBuf> {
    if !safe_release_target(target) || !safe_experiment_id(candidate) {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "invalid release identity"));
    }
    Ok(state.join("experiments").join("releases").join(target).join(candidate))
}

fn staged_release_marker_path(state: &Path, target: &str, candidate: &str) -> io::Result<PathBuf> {
    Ok(staged_release_path(state, target, candidate)?.join("release.json"))
}

fn release_load_proof_path(state: &Path, target: &str, candidate: &str) -> io::Result<PathBuf> {
    Ok(staged_release_path(state, target, candidate)?.join("loaded.json"))
}

fn copy_release_tree(source: &Path, destination: &Path) -> io::Result<()> {
    let metadata = fs::symlink_metadata(source)?;
    if metadata.is_dir() {
        fs::create_dir_all(destination)?;
        for entry in fs::read_dir(source)? {
            let entry = entry?;
            let name = entry.file_name();
            if name == ".git" || sensitive_path(&entry.path()) {
                continue;
            }
            copy_release_tree(&entry.path(), &destination.join(name))?;
        }
    } else if metadata.is_file() {
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::copy(source, destination)?;
    } else {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "release contains unsupported file type"));
    }
    Ok(())
}

/// Stage an immutable owner-local bundle after exact approval. Repeated calls
/// with the same identities are idempotent; changed identities fail closed.
pub fn stage_release(
    state: &Path,
    record: &ExperimentRecord,
    target: &str,
    source: &Path,
    now: u64,
) -> io::Result<StagedRelease> {
    if record.state != ExperimentState::Approved || !record.authorization_matches(now, target) {
        return Err(io::Error::new(io::ErrorKind::PermissionDenied, "release is not exactly authorized"));
    }
    if !source.is_dir() {
        return Err(io::Error::new(io::ErrorKind::NotFound, "release source directory is missing"));
    }
    let destination = staged_release_path(state, target, &record.candidate_manifest_digest)?;
    let marker_path = staged_release_marker_path(state, target, &record.candidate_manifest_digest)?;
    if marker_path.is_file() {
        let bytes = fs::read(&marker_path)?;
        let existing: StagedRelease = serde_json::from_slice(&bytes)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        if existing.parent_manifest_digest != record.parent_manifest_digest
            || existing.evaluator_digest != record.evaluation.as_ref().map(|e| e.evaluator_digest.clone()).unwrap_or_default()
            || existing.results_digest != record.evaluation.as_ref().map(|e| e.results_digest.clone()).unwrap_or_default()
        {
            return Err(io::Error::new(io::ErrorKind::AlreadyExists, "staged release identity changed"));
        }
        return Ok(existing);
    }
    copy_release_tree(source, &destination)?;
    let evaluation = record.evaluation.as_ref().ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "approved release has no evaluation"))?;
    let staged = StagedRelease {
        schema_version: EXPERIMENT_SCHEMA_VERSION,
        target: target.into(),
        candidate_manifest_digest: record.candidate_manifest_digest.clone(),
        parent_manifest_digest: record.parent_manifest_digest.clone(),
        evaluator_digest: evaluation.evaluator_digest.clone(),
        results_digest: evaluation.results_digest.clone(),
        source_path: source.display().to_string(),
        staged_path: destination.display().to_string(),
        staged_at_epoch_seconds: now,
    };
    crate::worldstate::atomic_write(&marker_path, &staged)?;
    Ok(staged)
}

/// Record proof that a fresh process loaded the staged candidate. This is
/// deliberately separate from copying files or changing the release pointer.
pub fn prove_release_loaded(
    state: &Path,
    target: &str,
    candidate_manifest_digest: &str,
    run_id: &str,
    now: u64,
) -> io::Result<ReleaseLoadProof> {
    let pointer_path = release_pointer_path(state, target)?;
    let pointer: ActiveReleasePointer = serde_json::from_slice(&fs::read(pointer_path)?)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    if pointer.active_manifest_digest != candidate_manifest_digest {
        return Err(io::Error::new(io::ErrorKind::PermissionDenied, "fresh run loaded a different release"));
    }
    let marker = staged_release_marker_path(state, target, candidate_manifest_digest)?;
    if !marker.is_file() {
        return Err(io::Error::new(io::ErrorKind::NotFound, "complete staged release is missing"));
    }
    let proof = ReleaseLoadProof {
        schema_version: EXPERIMENT_SCHEMA_VERSION,
        target: target.into(),
        candidate_manifest_digest: candidate_manifest_digest.into(),
        run_id: run_id.into(),
        loaded_at_epoch_seconds: now,
    };
    crate::worldstate::atomic_write(&release_load_proof_path(state, target, candidate_manifest_digest)?, &proof)?;
    Ok(proof)
}

/// Start a bounded canary only after the controlled pointer is active and
/// points at this record's successor. It records no external side effects.
pub fn start_canary(
    state: &Path,
    record: &mut ExperimentRecord,
    target: &str,
    now: u64,
    duration_seconds: u64,
    required_fresh_runs: u32,
) -> io::Result<CanaryWindow> {
    if record.state != ExperimentState::Active {
        return Err(io::Error::new(io::ErrorKind::PermissionDenied, "canary requires an active release"));
    }
    if duration_seconds == 0 || required_fresh_runs == 0 {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "canary duration and required runs must be positive"));
    }
    let pointer: ActiveReleasePointer = serde_json::from_slice(&fs::read(release_pointer_path(state, target)?)?)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    let Some(successor) = &record.successor_manifest_digest else {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "active experiment has no successor manifest"));
    };
    if pointer.active_manifest_digest != *successor {
        return Err(io::Error::new(io::ErrorKind::PermissionDenied, "active pointer does not match successor"));
    }
    let window = CanaryWindow {
        manifest_digest: successor.clone(),
        started_at_epoch_seconds: now,
        ends_at_epoch_seconds: now.saturating_add(duration_seconds),
        required_fresh_runs,
        state: CanaryState::Running,
        observations: Vec::new(),
    };
    record.canary = Some(window.clone());
    save_record(state, record)?;
    Ok(window)
}

/// Add one attributable fresh-run result to the canary. Duplicate run ids,
/// changed manifests, expired windows, and regressions fail closed.
pub fn observe_canary(
    record: &mut ExperimentRecord,
    observation: CanaryRunObservation,
) -> Result<(), String> {
    let Some(window) = record.canary.as_mut() else {
        return Err("no canary window is active".into());
    };
    if window.state != CanaryState::Running {
        return Err("canary window is already terminal".into());
    }
    if observation.manifest_digest != window.manifest_digest {
        return Err("canary observation uses a different manifest".into());
    }
    if window.observations.iter().any(|item| item.run_id == observation.run_id) {
        return Err("duplicate canary run id".into());
    }
    if observation.quality_regression {
        window.state = CanaryState::Failed;
        return Err("canary quality regression".into());
    }
    if observation.observed_at_epoch_seconds > window.ends_at_epoch_seconds {
        window.state = CanaryState::Failed;
        return Err("canary window expired".into());
    }
    window.observations.push(observation);
    if window.observations.len() >= window.required_fresh_runs as usize {
        window.state = CanaryState::Passed;
    }
    Ok(())
}

/// Begin the next round only from a passed canary, preserving explicit
/// experiment lineage and using the active successor as the new parent.
pub fn fork_successor_record(
    active: &ExperimentRecord,
    identity: ExperimentIdentity,
    candidate_manifest_digest: String,
    hypothesis: String,
    effect: String,
) -> Result<ExperimentRecord, String> {
    if active.state != ExperimentState::Active {
        return Err("successor round requires an active predecessor".into());
    }
    if active.canary.as_ref().map(|window| window.state) != Some(CanaryState::Passed) {
        return Err("successor round requires a passed canary".into());
    }
    let mut successor = ExperimentRecord::new(
        identity,
        active.candidate_manifest_digest.clone(),
        candidate_manifest_digest,
        hypothesis,
        effect,
    );
    successor.parent_experiment_id = Some(active.identity.experiment_id.clone());
    successor.decision = Some("inherited_from_passed_successor_canary".into());
    Ok(successor)
}

/// Promote only an exactly authorized evaluated record through the lab's
/// controlled pointer. This pointer is deliberately owner-local; it does not
/// claim that a live Governor session has loaded the release.
pub fn promote_record(
    state: &Path,
    record: &mut ExperimentRecord,
    target: &str,
    now: u64,
) -> io::Result<ActiveReleasePointer> {
    if record.state != ExperimentState::Approved {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "experiment is not approved",
        ));
    }
    if !record.authorization_matches(now, target) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "release authorization does not match candidate, evaluation, target, or expiry",
        ));
    }
    let path = release_pointer_path(state, target)?;
    let predecessor = fs::read(&path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<ActiveReleasePointer>(&bytes).ok())
        .map(|pointer| pointer.active_manifest_digest);
    record.rollback_parent_digest = predecessor.clone();
    record
        .transition(ExperimentState::Releasing)
        .map_err(io::Error::other)?;
    let pointer = ActiveReleasePointer {
        schema_version: EXPERIMENT_SCHEMA_VERSION,
        target: target.into(),
        active_manifest_digest: record.candidate_manifest_digest.clone(),
        predecessor_manifest_digest: predecessor,
        activated_at_epoch_seconds: now,
    };
    crate::worldstate::atomic_write(&path, &pointer)?;
    record.successor_manifest_digest = Some(record.candidate_manifest_digest.clone());
    record
        .transition(ExperimentState::Active)
        .map_err(io::Error::other)?;
    save_record(state, record)?;
    Ok(pointer)
}

/// Stage and activate one exactly authorized bundle as one owner-local
/// transaction. The staged marker is written before the active pointer; an
/// interrupted call therefore leaves the predecessor active and recoverable.
pub fn promote_staged_record(
    state: &Path,
    record: &mut ExperimentRecord,
    target: &str,
    source: &Path,
    now: u64,
) -> io::Result<(ActiveReleasePointer, StagedRelease)> {
    let staged = stage_release(state, record, target, source, now)?;
    let pointer = promote_record(state, record, target, now)?;
    Ok((pointer, staged))
}

pub fn rollback_record(
    state: &Path,
    record: &mut ExperimentRecord,
    target: &str,
    now: u64,
) -> io::Result<ActiveReleasePointer> {
    if record.state != ExperimentState::Active {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "only an active experiment can be rolled back",
        ));
    }
    let path = release_pointer_path(state, target)?;
    let bytes = fs::read(&path)?;
    let current: ActiveReleasePointer = serde_json::from_slice(&bytes)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    let predecessor = current.predecessor_manifest_digest.clone().ok_or_else(|| {
        io::Error::new(io::ErrorKind::NotFound, "no predecessor release to restore")
    })?;
    let restored = ActiveReleasePointer {
        schema_version: EXPERIMENT_SCHEMA_VERSION,
        target: target.into(),
        active_manifest_digest: predecessor,
        predecessor_manifest_digest: Some(current.active_manifest_digest),
        activated_at_epoch_seconds: now,
    };
    crate::worldstate::atomic_write(&path, &restored)?;
    record
        .transition(ExperimentState::RolledBack)
        .map_err(io::Error::other)?;
    save_record(state, record)?;
    Ok(restored)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn record() -> ExperimentRecord {
        ExperimentRecord::new(
            ExperimentIdentity {
                experiment_id: "exp-1".into(),
                case_id: "case-1".into(),
                candidate_id: "cand-1".into(),
                run_id: "run-1".into(),
                session_id: "session-1".into(),
                task_revision: "task-1".into(),
                harness_manifest_digest: "harness".into(),
                trace_id: Some("trace-1".into()),
            },
            "parent".into(),
            "candidate".into(),
            "hypothesis".into(),
            "effect".into(),
        )
    }

    fn complete_cost() -> CostAccounting {
        CostAccounting {
            acquisition_seconds: Some(1),
            generation_seconds: Some(2),
            evaluation_seconds: Some(3),
            review_seconds: Some(4),
            maintenance_seconds: Some(5),
            usage_tokens: Some(6),
            monetary_micros: Some(7),
            evidence: vec![EvidenceRef {
                id: "cost-ledger".into(),
                digest: "ledger".into(),
                acquired_at_epoch_seconds: 1,
                state: EvidenceState::Verified,
            }],
            unknown_components: Vec::new(),
        }
    }

    #[test]
    fn missing_or_unverified_evidence_fails_closed() {
        let mut r = record();
        r.transition(ExperimentState::Evaluating).unwrap();
        r.evaluation = Some(EvaluationSummary {
            evaluator_digest: "eval".into(),
            results_digest: "results".into(),
            primary_friction_measure: "interventions".into(),
            baseline_value: Some(4),
            candidate_value: Some(3),
            quality_regression: false,
            essential_escalation_regression: false,
            matched_conditions: true,
            evidence_state: EvidenceState::Accepted,
            cost: CostAccounting::default(),
        });
        assert!(r.transition(ExperimentState::AwaitingApproval).is_err());
    }

    #[test]
    fn incomplete_cost_coverage_fails_closed() {
        let mut r = record();
        r.transition(ExperimentState::Evaluating).unwrap();
        r.evaluation = Some(EvaluationSummary {
            evaluator_digest: "eval".into(),
            results_digest: "results".into(),
            primary_friction_measure: "interventions".into(),
            baseline_value: Some(4),
            candidate_value: Some(3),
            quality_regression: false,
            essential_escalation_regression: false,
            matched_conditions: true,
            evidence_state: EvidenceState::Verified,
            cost: CostAccounting::default(),
        });
        assert!(r.transition(ExperimentState::AwaitingApproval).is_err());
    }

    #[test]
    fn inconclusive_record_can_be_re_evaluated_before_approval() {
        let mut r = record();
        r.transition(ExperimentState::Evaluating).unwrap();
        r.transition(ExperimentState::Inconclusive).unwrap();
        r.transition(ExperimentState::Evaluating).unwrap();
        r.evaluation = Some(EvaluationSummary {
            evaluator_digest: "eval-2".into(),
            results_digest: "results-2".into(),
            primary_friction_measure: "measure".into(),
            baseline_value: Some(3),
            candidate_value: Some(0),
            quality_regression: false,
            essential_escalation_regression: false,
            matched_conditions: true,
            evidence_state: EvidenceState::Verified,
            cost: complete_cost(),
        });
        r.transition(ExperimentState::AwaitingApproval).unwrap();
    }

    #[test]
    fn owner_denial_invalidates_pending_authorization() {
        let mut r = record();
        r.transition(ExperimentState::Evaluating).unwrap();
        r.evaluation = Some(EvaluationSummary {
            evaluator_digest: "eval".into(),
            results_digest: "results".into(),
            primary_friction_measure: "measure".into(),
            baseline_value: Some(3),
            candidate_value: Some(0),
            quality_regression: false,
            essential_escalation_regression: false,
            matched_conditions: true,
            evidence_state: EvidenceState::Verified,
            cost: complete_cost(),
        });
        r.release_authorization = Some(ReleaseAuthorization {
            candidate_id: "cand-1".into(),
            parent_manifest_digest: "parent".into(),
            evaluator_digest: "eval".into(),
            results_digest: "results".into(),
            target: "governor".into(),
            expires_at_epoch_seconds: 10,
            owner_id: "owner-1".into(),
            review_reference: "review-1".into(),
            approved_at_epoch_seconds: 1,
        });
        r.transition(ExperimentState::AwaitingApproval).unwrap();
        r.deny("owner declined canary").unwrap();
        assert_eq!(r.state, ExperimentState::Rejected);
        assert!(r.release_authorization.is_none());
        assert_eq!(r.decision.as_deref(), Some("rejected: owner declined canary"));
    }

    #[test]
    fn canary_regression_and_expiry_are_terminal_failures() {
        let mut r = record();
        r.canary = Some(CanaryWindow {
            manifest_digest: "candidate".into(),
            started_at_epoch_seconds: 1,
            ends_at_epoch_seconds: 5,
            required_fresh_runs: 1,
            state: CanaryState::Running,
            observations: Vec::new(),
        });
        assert!(observe_canary(
            &mut r,
            CanaryRunObservation {
                run_id: "regression".into(),
                task_revision: "task".into(),
                manifest_digest: "candidate".into(),
                outcome_digest: "outcome".into(),
                quality_regression: true,
                observed_at_epoch_seconds: 2,
            },
        )
        .is_err());
        assert_eq!(r.canary.as_ref().unwrap().state, CanaryState::Failed);
        let mut expired = record();
        expired.canary = Some(CanaryWindow {
            manifest_digest: "candidate".into(),
            started_at_epoch_seconds: 1,
            ends_at_epoch_seconds: 5,
            required_fresh_runs: 1,
            state: CanaryState::Running,
            observations: Vec::new(),
        });
        assert!(observe_canary(
            &mut expired,
            CanaryRunObservation {
                run_id: "late".into(),
                task_revision: "task".into(),
                manifest_digest: "candidate".into(),
                outcome_digest: "outcome".into(),
                quality_regression: false,
                observed_at_epoch_seconds: 6,
            },
        )
        .is_err());
        assert_eq!(expired.canary.as_ref().unwrap().state, CanaryState::Failed);
    }

    #[test]
    fn negative_control_is_not_eligible() {
        let mut r = record();
        r.transition(ExperimentState::Evaluating).unwrap();
        r.evaluation = Some(EvaluationSummary {
            evaluator_digest: "eval".into(),
            results_digest: "results".into(),
            primary_friction_measure: "interventions".into(),
            baseline_value: Some(4),
            candidate_value: Some(1),
            quality_regression: false,
            essential_escalation_regression: true,
            matched_conditions: true,
            evidence_state: EvidenceState::Verified,
            cost: complete_cost(),
        });
        assert!(r.transition(ExperimentState::AwaitingApproval).is_err());
    }

    #[test]
    fn authorization_is_bound_to_exact_evaluation_and_target() {
        let mut r = record();
        r.evaluation = Some(EvaluationSummary {
            evaluator_digest: "eval".into(),
            results_digest: "results".into(),
            primary_friction_measure: "interventions".into(),
            baseline_value: Some(4),
            candidate_value: Some(3),
            quality_regression: false,
            essential_escalation_regression: false,
            matched_conditions: true,
            evidence_state: EvidenceState::Verified,
            cost: complete_cost(),
        });
        r.release_authorization = Some(ReleaseAuthorization {
            candidate_id: "cand-1".into(),
            parent_manifest_digest: "parent".into(),
            evaluator_digest: "eval".into(),
            results_digest: "results".into(),
            target: "governor".into(),
            expires_at_epoch_seconds: 10,
            owner_id: "owner-1".into(),
            review_reference: "review-1".into(),
            approved_at_epoch_seconds: 1,
        });
        assert!(r.authorization_matches(10, "governor"));
        assert!(!r.authorization_matches(11, "governor"));
        assert!(!r.authorization_matches(10, "other"));
    }

    #[test]
    fn manifest_is_deterministic_and_excludes_sensitive_files() {
        let root = std::env::temp_dir().join(format!(
            "task-fabric-svn-experiment-manifest-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(root.join("secrets")).unwrap();
        fs::write(root.join("harness.md"), b"stable harness").unwrap();
        fs::write(root.join("secrets/token"), b"must not be read").unwrap();
        let first = manifest_for_paths(std::slice::from_ref(&root)).unwrap();
        let second = manifest_for_paths(std::slice::from_ref(&root)).unwrap();
        assert_eq!(first, second);
        assert!(first
            .entries
            .contains_key(&root.join("harness.md").display().to_string()));
        assert!(first
            .omitted_paths
            .iter()
            .any(|path| path.ends_with("secrets/token")));
        assert_eq!(first.digest(), second.digest());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn records_round_trip_in_owner_state_and_reject_traversal_ids() {
        let root = std::env::temp_dir().join(format!(
            "task-fabric-svn-experiment-record-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let r = record();
        save_record(&root, &r).unwrap();
        assert_eq!(load_record(&root, "exp-1").unwrap(), Some(r));
        assert!(experiment_record_path(&root, "../escape").is_err());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn trial_artifact_attachment_is_hashed_and_idempotent() {
        let root = std::env::temp_dir().join(format!(
            "task-fabric-svn-trial-artifact-{}",
            SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()
        ));
        fs::create_dir_all(&root).unwrap();
        let path = root.join("events.jsonl");
        fs::write(&path, b"event\n").unwrap();
        let mut record = record();
        let first = attach_trial_artifact(
            &mut record,
            &path,
            "events-1",
            10,
            EvidenceState::Verified,
        )
        .unwrap();
        let second = attach_trial_artifact(
            &mut record,
            &path,
            "events-1",
            11,
            EvidenceState::Verified,
        )
        .unwrap();
        assert_eq!(first, second);
        assert_eq!(record.trial_artifacts.len(), 1);
        assert!(!first.digest.is_empty());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn governor_adapter_preserves_evidence_quality_and_source_digest() {
        let root = std::env::temp_dir().join(format!(
            "task-fabric-svn-governor-adapter-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&root).unwrap();
        let state = root.join("run-state.json");
        let telemetry = root.join("telemetry.jsonl");
        fs::write(
            &state,
            r#"{"schema":2,"runId":"run-1","project":"/repo","status":"ACCEPTED","acceptance_criteria":[{"id":"C1","status":"INFERRED"}]}"#,
        )
        .unwrap();
        fs::write(&telemetry, b"{\"kind\":\"intervention\"}\nnot-json\n").unwrap();
        let loaded = load_governor_run_state(&state).unwrap();
        let summary = summarize_governor_telemetry(&telemetry).unwrap();
        assert_eq!(loaded.run_id, "run-1");
        assert_eq!(loaded.acceptance_criteria[0].status, "INFERRED");
        assert_eq!(summary.total_events, 1);
        assert_eq!(summary.intervention_events, 1);
        assert_eq!(summary.malformed_lines, 1);
        assert!(!summary.source_digest.is_empty());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn router_receipt_adapter_requires_explicit_experiment_identity() {
        let root = std::env::temp_dir().join(format!(
            "task-fabric-svn-router-receipts-{}",
            SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()
        ));
        fs::create_dir_all(&root).unwrap();
        let path = root.join("receipts.jsonl");
        fs::write(
            &path,
            concat!(
                "{\"execution_id\":\"unrelated\",\"prompt_tokens\":99,\"completion_tokens\":4,\"cost_micros\":12}\n",
                "{\"execution_id\":\"matched\",\"experiment_id\":\"exp-1\",\"prompt_tokens\":10,\"completion_tokens\":2,\"cost_micros\":7}\n",
                "{\"execution_id\":\"missing-cost\",\"experiment_id\":\"exp-1\",\"prompt_tokens\":3,\"completion_tokens\":1}\n",
                "not-json\n"
            ),
        )
        .unwrap();
        let summary = summarize_router_receipts(&path, "exp-1").unwrap();
        assert_eq!(summary.total_rows, 3);
        assert_eq!(summary.attributed_rows, 2);
        assert_eq!(summary.unattributed_rows, 1);
        assert_eq!(summary.malformed_lines, 1);
        assert_eq!(summary.prompt_tokens, 13);
        assert_eq!(summary.completion_tokens, 3);
        assert_eq!(summary.cost_micros, None);
        assert_eq!(summary.unknown_cost_rows, 1);
        assert_eq!(summary.execution_ids, vec!["matched", "missing-cost"]);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn router_receipt_cost_attachment_is_atomic_and_attributed() {
        let root = std::env::temp_dir().join(format!(
            "task-fabric-svn-router-cost-{}",
            SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()
        ));
        fs::create_dir_all(&root).unwrap();
        let path = root.join("receipts.jsonl");
        fs::write(
            &path,
            concat!(
                "{\"execution_id\":\"one\",\"experiment_id\":\"exp-1\",\"prompt_tokens\":10,\"completion_tokens\":2,\"cost_micros\":7}\n",
                "{\"execution_id\":\"two\",\"experiment_id\":\"exp-1\",\"prompt_tokens\":3,\"completion_tokens\":4,\"cost_micros\":5}\n"
            ),
        )
        .unwrap();
        let mut record = record();
        record.evaluation = Some(EvaluationSummary {
            evaluator_digest: "eval".into(),
            results_digest: "results".into(),
            primary_friction_measure: "friction".into(),
            baseline_value: Some(2),
            candidate_value: Some(1),
            quality_regression: false,
            essential_escalation_regression: false,
            matched_conditions: true,
            evidence_state: EvidenceState::Verified,
            cost: CostAccounting {
                unknown_components: vec!["cost coverage not recorded".into()],
                ..CostAccounting::default()
            },
        });
        let summary = attach_router_receipt_cost(&mut record, &path, 42).unwrap();
        assert_eq!(summary.attributed_rows, 2);
        assert_eq!(record.evaluation.as_ref().unwrap().cost.usage_tokens, Some(19));
        assert_eq!(record.evaluation.as_ref().unwrap().cost.monetary_micros, Some(12));
        assert_eq!(record.evaluation.as_ref().unwrap().cost.evidence.len(), 1);
        assert_eq!(
            record.evaluation.as_ref().unwrap().cost.unknown_components,
            vec!["cost coverage not recorded"]
        );
        assert_eq!(record.trial_artifacts.len(), 1);

        fs::write(
            &path,
            "{\"execution_id\":\"foreign\",\"experiment_id\":\"other\",\"cost_micros\":1}\n",
        )
        .unwrap();
        let before = record.evaluation.clone();
        assert!(attach_router_receipt_cost(&mut record, &path, 43).is_err());
        assert_eq!(record.evaluation, before);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn native_router_task_fabric_svn_work_id_can_be_attributed_explicitly() {
        let root = std::env::temp_dir().join(format!(
            "task-fabric-svn-router-native-receipt-{}",
            SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()
        ));
        fs::create_dir_all(&root).unwrap();
        let path = root.join("receipts.jsonl");
        fs::write(
            &path,
            "{\"execution_id\":\"native\",\"task_fabric_svn\":{\"work_id\":\"exp-1\"},\"usage\":{\"prompt_tokens\":11,\"completion_tokens\":2},\"cost\":{\"amount\":0.000003,\"state\":\"known\"}}\n",
        )
        .unwrap();
        let summary = summarize_router_receipts(&path, "exp-1").unwrap();
        assert_eq!(summary.attributed_rows, 1);
        assert_eq!(summary.unattributed_rows, 0);
        assert_eq!(summary.prompt_tokens, 11);
        assert_eq!(summary.completion_tokens, 2);
        assert_eq!(summary.cost_micros, Some(3));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn opencode_event_adapter_preserves_malformed_and_token_evidence() {
        let root = std::env::temp_dir().join(format!(
            "task-fabric-svn-opencode-events-{}",
            SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()
        ));
        fs::create_dir_all(&root).unwrap();
        let path = root.join("events.jsonl");
        fs::write(
            &path,
            concat!(
                "mise warning\n",
                "{\"type\":\"step_start\",\"sessionID\":\"session-1\"}\n",
                "{\"type\":\"step_finish\",\"sessionID\":\"session-1\",\"part\":{\"tokens\":{\"total\":13,\"input\":10,\"output\":2,\"reasoning\":1}}}\n"
            ),
        )
        .unwrap();
        let summary = summarize_opencode_events(&path).unwrap();
        assert_eq!(summary.total_events, 2);
        assert_eq!(summary.malformed_lines, 1);
        assert_eq!(summary.session_ids, vec!["session-1"]);
        assert_eq!(summary.latest_total_tokens, Some(13));
        assert_eq!(summary.latest_output_tokens, Some(2));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn worker_result_attachment_requires_all_identities_and_is_idempotent() {
        let root = std::env::temp_dir().join(format!(
            "task-fabric-svn-worker-result-{}",
            SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()
        ));
        fs::create_dir_all(&root).unwrap();
        let path = root.join("result.json");
        fs::write(
            &path,
            serde_json::to_vec(&serde_json::json!({
                "schema_version": EXPERIMENT_SCHEMA_VERSION,
                "job_id": "job-1",
                "experiment_id": "exp-1",
                "trial_id": "trial-1",
                "run_id": "run-1",
                "session_id": "session-1",
                "task_revision": "task-1",
                "harness_manifest_digest": "harness",
                "status": "succeeded",
                "release_manifest_digest": "candidate"
            }))
            .unwrap(),
        )
        .unwrap();
        let mut record = record();
        let evidence = attach_worker_trial_result(&mut record, &path, Some("candidate"), 10).unwrap();
        assert_eq!(evidence.state, EvidenceState::Verified);
        assert_eq!(record.trial_artifacts.len(), 1);
        let duplicate = attach_worker_trial_result(&mut record, &path, Some("candidate"), 11).unwrap();
        assert_eq!(duplicate, evidence);
        assert_eq!(record.trial_artifacts.len(), 1);
        let bad = root.join("bad.json");
        fs::write(
            &bad,
            serde_json::to_vec(&serde_json::json!({
                "schema_version": EXPERIMENT_SCHEMA_VERSION,
                "job_id": "job-2",
                "experiment_id": "other",
                "run_id": "run-1",
                "session_id": "session-1",
                "task_revision": "task-1",
                "harness_manifest_digest": "harness",
                "status": "succeeded"
            }))
            .unwrap(),
        )
        .unwrap();
        assert!(attach_worker_trial_result(&mut record, &bad, None, 12).is_err());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn intervention_mining_deduplicates_delivery_pair_without_claiming_impact() {
        let root = std::env::temp_dir().join(format!(
            "task-fabric-svn-intervention-mining-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&root).unwrap();
        let telemetry = root.join("telemetry.jsonl");
        fs::write(
            &telemetry,
            b"{\"kind\":\"intervention\",\"sessionID\":\"s\",\"heuristic\":\"idle\",\"evidence\":\"gap\",\"delivery\":\"queued\"}\n{\"kind\":\"intervention\",\"sessionID\":\"s\",\"heuristic\":\"idle\",\"evidence\":\"gap\",\"delivery\":\"delivered\"}\n",
        )
        .unwrap();
        let report = mine_governor_interventions(&telemetry).unwrap();
        assert_eq!(report.raw_intervention_events, 2);
        assert_eq!(report.unique_occurrences.len(), 1);
        assert_eq!(report.unique_occurrences[0].queued_count, 1);
        assert_eq!(report.unique_occurrences[0].delivered_count, 1);
        assert_eq!(
            report.unique_occurrences[0].owner_impact,
            "unknown_until_replay_or_trial"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn promotion_requires_exact_authorization_and_rollback_restores_predecessor() {
        let root = std::env::temp_dir().join(format!(
            "task-fabric-svn-release-pointer-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let mut r = record();
        r.transition(ExperimentState::Evaluating).unwrap();
        r.evaluation = Some(EvaluationSummary {
            evaluator_digest: "eval".into(),
            results_digest: "results".into(),
            primary_friction_measure: "measure".into(),
            baseline_value: Some(2),
            candidate_value: Some(1),
            quality_regression: false,
            essential_escalation_regression: false,
            matched_conditions: true,
            evidence_state: EvidenceState::Verified,
            cost: complete_cost(),
        });
        r.release_authorization = Some(ReleaseAuthorization {
            candidate_id: "cand-1".into(),
            parent_manifest_digest: "parent".into(),
            evaluator_digest: "eval".into(),
            results_digest: "results".into(),
            target: "governor".into(),
            expires_at_epoch_seconds: 10,
            owner_id: "owner-1".into(),
            review_reference: "review-1".into(),
            approved_at_epoch_seconds: 1,
        });
        r.transition(ExperimentState::AwaitingApproval).unwrap();
        r.transition(ExperimentState::Approved).unwrap();
        let pointer_path = release_pointer_path(&root, "governor").unwrap();
        crate::worldstate::atomic_write(
            &pointer_path,
            &ActiveReleasePointer {
                schema_version: EXPERIMENT_SCHEMA_VERSION,
                target: "governor".into(),
                active_manifest_digest: "old-parent".into(),
                predecessor_manifest_digest: None,
                activated_at_epoch_seconds: 1,
            },
        )
        .unwrap();
        let promoted = promote_record(&root, &mut r, "governor", 10).unwrap();
        assert_eq!(promoted.active_manifest_digest, "candidate");
        assert_eq!(
            promoted.predecessor_manifest_digest.as_deref(),
            Some("old-parent")
        );
        assert!(!r.authorization_matches(11, "governor"));
        let restored = rollback_record(&root, &mut r, "governor", 11).unwrap();
        assert_eq!(restored.active_manifest_digest, "old-parent");
        assert_eq!(r.state, ExperimentState::RolledBack);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn staging_is_idempotent_and_fresh_load_requires_active_candidate() {
        let root = std::env::temp_dir().join(format!(
            "task-fabric-svn-release-stage-{}",
            SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()
        ));
        let source = root.join("candidate-source");
        fs::create_dir_all(source.join("nested")).unwrap();
        fs::write(source.join("nested/governor.mjs"), b"candidate").unwrap();
        fs::create_dir_all(source.join("secrets")).unwrap();
        fs::write(source.join("secrets/token"), b"must not stage").unwrap();
        let mut r = record();
        r.transition(ExperimentState::Evaluating).unwrap();
        r.evaluation = Some(EvaluationSummary {
            evaluator_digest: "eval".into(),
            results_digest: "results".into(),
            primary_friction_measure: "measure".into(),
            baseline_value: Some(3),
            candidate_value: Some(0),
            quality_regression: false,
            essential_escalation_regression: false,
            matched_conditions: true,
            evidence_state: EvidenceState::Verified,
            cost: complete_cost(),
        });
        r.release_authorization = Some(ReleaseAuthorization {
            candidate_id: "cand-1".into(),
            parent_manifest_digest: "parent".into(),
            evaluator_digest: "eval".into(),
            results_digest: "results".into(),
            target: "governor".into(),
            expires_at_epoch_seconds: 20,
            owner_id: "owner-1".into(),
            review_reference: "review-1".into(),
            approved_at_epoch_seconds: 1,
        });
        r.transition(ExperimentState::AwaitingApproval).unwrap();
        r.transition(ExperimentState::Approved).unwrap();
        let staged = stage_release(&root, &r, "governor", &source, 10).unwrap();
        let duplicate = stage_release(&root, &r, "governor", &source, 11).unwrap();
        assert_eq!(staged, duplicate);
        assert!(Path::new(&staged.staged_path).join("nested/governor.mjs").is_file());
        assert!(!Path::new(&staged.staged_path).join("secrets/token").exists());
        let (pointer, _) = promote_staged_record(&root, &mut r, "governor", &source, 12).unwrap();
        assert_eq!(pointer.active_manifest_digest, "candidate");
        let proof = prove_release_loaded(&root, "governor", "candidate", "fresh-run-1", 13).unwrap();
        assert_eq!(proof.run_id, "fresh-run-1");
        let window = start_canary(&root, &mut r, "governor", 14, 100, 2).unwrap();
        assert_eq!(window.state, CanaryState::Running);
        observe_canary(
            &mut r,
            CanaryRunObservation {
                run_id: "fresh-run-1".into(),
                task_revision: "task-a".into(),
                manifest_digest: "candidate".into(),
                outcome_digest: "outcome-a".into(),
                quality_regression: false,
                observed_at_epoch_seconds: 15,
            },
        )
        .unwrap();
        assert!(observe_canary(
            &mut r,
            CanaryRunObservation {
                run_id: "fresh-run-1".into(),
                task_revision: "task-a".into(),
                manifest_digest: "candidate".into(),
                outcome_digest: "outcome-a".into(),
                quality_regression: false,
                observed_at_epoch_seconds: 16,
            },
        )
        .is_err());
        observe_canary(
            &mut r,
            CanaryRunObservation {
                run_id: "fresh-run-2".into(),
                task_revision: "task-b".into(),
                manifest_digest: "candidate".into(),
                outcome_digest: "outcome-b".into(),
                quality_regression: false,
                observed_at_epoch_seconds: 17,
            },
        )
        .unwrap();
        assert_eq!(r.canary.as_ref().unwrap().state, CanaryState::Passed);
        let successor = fork_successor_record(
            &r,
            ExperimentIdentity {
                experiment_id: "exp-2".into(),
                case_id: "case-2".into(),
                candidate_id: "cand-2".into(),
                run_id: "run-2".into(),
                session_id: "session-2".into(),
                task_revision: "task-2".into(),
                harness_manifest_digest: "harness-2".into(),
                trace_id: None,
            },
            "candidate-2".into(),
            "next hypothesis".into(),
            "next effect".into(),
        )
        .unwrap();
        assert_eq!(successor.parent_manifest_digest, "candidate");
        assert_eq!(successor.parent_experiment_id.as_deref(), Some("exp-1"));
        assert!(stage_release(&root, &r, "governor", &source, 21).is_err());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn release_failure_paths_fail_closed() {
        let root = std::env::temp_dir().join(format!(
            "task-fabric-svn-release-failures-{}",
            SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()
        ));
        let source = root.join("candidate-source");
        fs::create_dir_all(&source).unwrap();
        fs::write(source.join("governor.mjs"), b"candidate").unwrap();
        let mut r = record();
        r.transition(ExperimentState::Evaluating).unwrap();
        r.evaluation = Some(EvaluationSummary {
            evaluator_digest: "eval".into(),
            results_digest: "results".into(),
            primary_friction_measure: "measure".into(),
            baseline_value: Some(3),
            candidate_value: Some(0),
            quality_regression: false,
            essential_escalation_regression: false,
            matched_conditions: true,
            evidence_state: EvidenceState::Verified,
            cost: complete_cost(),
        });
        r.release_authorization = Some(ReleaseAuthorization {
            candidate_id: "cand-1".into(),
            parent_manifest_digest: "parent".into(),
            evaluator_digest: "eval".into(),
            results_digest: "results".into(),
            target: "governor".into(),
            expires_at_epoch_seconds: 10,
            owner_id: "owner-1".into(),
            review_reference: "review-1".into(),
            approved_at_epoch_seconds: 1,
        });
        r.transition(ExperimentState::AwaitingApproval).unwrap();
        r.transition(ExperimentState::Approved).unwrap();
        assert!(stage_release(&root, &r, "other-target", &source, 5).is_err());
        assert!(stage_release(&root, &r, "governor", &source, 11).is_err());
        stage_release(&root, &r, "governor", &source, 5).unwrap();
        let mut changed = r.clone();
        changed.parent_manifest_digest = "changed-parent".into();
        assert!(stage_release(&root, &changed, "governor", &source, 5).is_err());
        let (pointer, _) = promote_staged_record(&root, &mut r, "governor", &source, 5).unwrap();
        assert!(prove_release_loaded(&root, "governor", "wrong-candidate", "run", 6).is_err());
        assert_eq!(pointer.predecessor_manifest_digest, None);
        let mut no_predecessor = r.clone();
        assert!(rollback_record(&root, &mut no_predecessor, "governor", 7).is_err());
        fs::remove_dir_all(root).unwrap();
    }
}
