//! Evidence-backed improvement proposals compiled from canonical workstation
//! observations.  This layer diagnoses and specifies work; it never mutates a
//! target repository and never grants itself release authority.

use crate::{experiment, now_epoch_seconds, worldstate, ProjectConfig};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

pub const IMPROVEMENT_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ProposalStatus {
    Incomplete,
    Ready,
    Queued,
    Running,
    Evaluated,
    Blocked,
    Rejected,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ProposalConfidence {
    EvidenceSupported,
    Incomplete,
    Speculative,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProposalTarget {
    pub world_id: Option<String>,
    pub repository: String,
    pub component: String,
    pub component_class: String,
    pub experiment_target: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RecurrenceHistory {
    pub first_observed_epoch_seconds: u64,
    pub last_observed_epoch_seconds: u64,
    pub occurrence_count: u64,
    pub occurrence_ids: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TrialComparison {
    pub parent_condition: String,
    pub candidate_condition: String,
    pub primary_measure: String,
    pub matched_conditions: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProposalLineage {
    pub source_observation_ids: Vec<String>,
    pub experiment_id: Option<String>,
    pub evaluation_digest: Option<String>,
    pub owner_decision_ref: Option<String>,
    pub canary_ref: Option<String>,
    pub promotion_ref: Option<String>,
    pub rollback_ref: Option<String>,
    pub successor_proposal_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ImprovementProposal {
    pub schema_version: u32,
    pub proposal_id: String,
    pub fingerprint: String,
    pub created_at_epoch_seconds: u64,
    pub updated_at_epoch_seconds: u64,
    pub target: ProposalTarget,
    pub observed_friction: String,
    pub source_evidence: Vec<experiment::EvidenceRef>,
    pub recurrence: RecurrenceHistory,
    pub falsifiable_hypothesis: String,
    pub predicted_measurable_effect: String,
    pub protected_invariants: Vec<String>,
    pub negative_controls: Vec<String>,
    #[serde(default)]
    pub alternatives_considered: Vec<String>,
    pub trial: TrialComparison,
    pub estimated_blast_radius: String,
    pub reversibility: String,
    pub required_evaluator: String,
    pub confidence: ProposalConfidence,
    pub status: ProposalStatus,
    pub missing_evidence: Vec<String>,
    pub lineage: ProposalLineage,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ImprovementCompileReport {
    pub schema_version: u32,
    pub generated_at_epoch_seconds: u64,
    pub source_event_count: usize,
    pub proposals: Vec<ImprovementProposal>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProposalReviewProjection {
    pub proposal: ImprovementProposal,
    pub linked_experiment: Option<experiment::ExperimentRecord>,
}

pub fn proposal_dir(state: &Path) -> PathBuf {
    state.join("improvements")
}

fn digest_bytes(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn digest_json<T: Serialize>(value: &T) -> String {
    digest_bytes(&serde_json::to_vec(value).expect("proposal digest serialization cannot fail"))
}

fn safe_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 160
        && id.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

fn slug(value: &str) -> String {
    value
        .chars()
        .map(|character| if character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.') { character } else { '-' })
        .collect()
}

pub fn proposal_path(state: &Path, proposal_id: &str) -> io::Result<PathBuf> {
    if !safe_id(proposal_id) {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "invalid proposal id"));
    }
    Ok(proposal_dir(state).join(format!("{proposal_id}.json")))
}

pub fn save_proposal(state: &Path, proposal: &ImprovementProposal) -> io::Result<()> {
    crate::worldstate::atomic_write(&proposal_path(state, &proposal.proposal_id)?, proposal)
}

pub fn load_proposal(state: &Path, proposal_id: &str) -> io::Result<Option<ImprovementProposal>> {
    match fs::read(proposal_path(state, proposal_id)?) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

pub fn load_proposals(state: &Path) -> io::Result<Vec<ImprovementProposal>> {
    let dir = proposal_dir(state);
    let mut proposals = Vec::new();
    let Ok(entries) = fs::read_dir(dir) else { return Ok(proposals); };
    for entry in entries {
        let path = entry?.path();
        if path.extension().and_then(|x| x.to_str()) != Some("json") { continue; }
        let bytes = fs::read(&path)?;
        if let Ok(proposal) = serde_json::from_slice(&bytes) { proposals.push(proposal); }
    }
    proposals.sort_by(|a, b| a.proposal_id.cmp(&b.proposal_id));
    Ok(proposals)
}

fn evidence(id: impl Into<String>, digest: String, state: experiment::EvidenceState) -> experiment::EvidenceRef {
    experiment::EvidenceRef { id: id.into(), digest, acquired_at_epoch_seconds: now_epoch_seconds(), state }
}

fn event_evidence(event: &worldstate::WorldEvent) -> experiment::EvidenceRef {
    evidence(format!("world-event:{}", event.id), digest_json(event), experiment::EvidenceState::Verified)
}

fn occurrence_id(fingerprint: &str, source: &str) -> String {
    digest_bytes(format!("{fingerprint}\0{source}").as_bytes())
}

fn existing_for_fingerprint<'a>(existing: &'a [ImprovementProposal], fingerprint: &str) -> Option<&'a ImprovementProposal> {
    existing.iter().find(|proposal| proposal.fingerprint == fingerprint)
}

fn preserve_lifecycle(mut proposal: ImprovementProposal, existing: Option<&ImprovementProposal>) -> ImprovementProposal {
    if let Some(previous) = existing {
        if matches!(previous.status, ProposalStatus::Queued | ProposalStatus::Running | ProposalStatus::Evaluated | ProposalStatus::Rejected) {
            proposal.status = previous.status.clone();
        }
    }
    proposal
}

fn merge_recurrence(
    existing: Option<&ImprovementProposal>,
    fingerprint: &str,
    observed_at: u64,
    source_ids: &[String],
) -> RecurrenceHistory {
    let mut ids: BTreeSet<String> = existing
        .map(|proposal| proposal.recurrence.occurrence_ids.iter().cloned().collect())
        .unwrap_or_default();
    for source in source_ids { ids.insert(occurrence_id(fingerprint, source)); }
    let first = existing.map(|proposal| proposal.recurrence.first_observed_epoch_seconds).unwrap_or(observed_at);
    RecurrenceHistory {
        first_observed_epoch_seconds: first,
        last_observed_epoch_seconds: observed_at,
        occurrence_count: ids.len() as u64,
        occurrence_ids: ids.into_iter().collect(),
    }
}

fn pattern_evidence(
    pattern: &worldstate::FrictionPattern,
    events: &[worldstate::WorldEvent],
    config: &ProjectConfig,
) -> (Vec<experiment::EvidenceRef>, Vec<String>) {
    let mut refs = Vec::new();
    for event in events {
        let same_world = event.world_id.as_deref() == Some(pattern.world_id.as_str());
        let same_effect = event.effect_ref.as_deref() == Some(pattern.target_ref.as_str());
        let degraded = matches!(event.outcome, Some(worldstate::EffectOutcome::Degraded | worldstate::EffectOutcome::Failed));
        if same_world && (same_effect || degraded) { refs.push(event_evidence(event)); }
    }
    if refs.is_empty() {
        if let Some(project) = config.projects.iter().find(|p| p.id == pattern.world_id) {
            refs.push(evidence(
                format!("worldstate-project:{}", project.id),
                digest_json(project),
                experiment::EvidenceState::Verified,
            ));
        }
    }
    let source_ids = refs.iter().map(|item| item.id.clone()).collect();
    (refs, source_ids)
}

fn static_proposal(
    pattern: &worldstate::FrictionPattern,
    events: &[worldstate::WorldEvent],
    config: &ProjectConfig,
    existing: &[ImprovementProposal],
) -> ImprovementProposal {
    let fingerprint = format!("workstation:{}:{}", pattern.kind_string(), pattern.world_id);
    let (source_evidence, source_ids) = pattern_evidence(pattern, events, config);
    let now = now_epoch_seconds();
    let old = existing_for_fingerprint(existing, &fingerprint);
    let occurrence_count = events.iter().filter(|event| event.world_id.as_deref() == Some(pattern.world_id.as_str())).count();
    let mut missing = vec![
        "a bounded parent/candidate implementation is not identified".to_string(),
        "an independent evaluator and matched comparison are not identified".to_string(),
    ];
    let confidence = if source_evidence.is_empty() { ProposalConfidence::Speculative } else { ProposalConfidence::Incomplete };
    if occurrence_count == 0 { missing.push("no matching WorldState event recurrence is retained".into()); }
    let status = ProposalStatus::Incomplete;
    preserve_lifecycle(ImprovementProposal {
        schema_version: IMPROVEMENT_SCHEMA_VERSION,
        proposal_id: format!("improvement-{}", slug(&fingerprint)),
        fingerprint: fingerprint.clone(),
        created_at_epoch_seconds: old.map(|p| p.created_at_epoch_seconds).unwrap_or(now),
        updated_at_epoch_seconds: now,
        target: ProposalTarget {
            world_id: Some(pattern.world_id.clone()),
            repository: config.projects.iter().find(|p| p.id == pattern.world_id).map(|p| p.repo.display().to_string()).unwrap_or_default(),
            component: pattern.target_ref.clone(),
            component_class: "workstation-worldstate".into(),
            experiment_target: "task-fabric-svn".into(),
        },
        observed_friction: format!("{}: {}", pattern.title, pattern.rationale),
        source_evidence,
        recurrence: merge_recurrence(old, &fingerprint, now, &source_ids),
        falsifiable_hypothesis: format!("A bounded change addressing {} will reduce the observed friction without changing protected workstation invariants.", pattern.title),
        predicted_measurable_effect: "Reduce matched friction occurrences per comparable transition while preserving required world activation and owner approval behavior.".into(),
        protected_invariants: vec!["world transitions remain owner-authorized".into(), "failed preconditions remain fail-closed".into(), "no live target is modified by diagnosis".into()],
        negative_controls: vec!["unrelated world transition with the same approval and safety gates".into()],
        alternatives_considered: vec!["manual TOML patch without a bounded trial".into(), "silent live remediation".into()],
        trial: TrialComparison {
            parent_condition: "current canonical workstation configuration".into(),
            candidate_condition: "isolated candidate configuration only".into(),
            primary_measure: "matched friction occurrences per transition".into(),
            matched_conditions: "same world, task, and declared preconditions".into(),
        },
        estimated_blast_radius: "single declared workstation component; no live mutation".into(),
        reversibility: "isolated artifact rollback before any owner-authorized release".into(),
        required_evaluator: "independent workstation evaluator".into(),
        confidence,
        status,
        missing_evidence: missing,
        lineage: ProposalLineage { source_observation_ids: source_ids, experiment_id: None, evaluation_digest: None, owner_decision_ref: None, canary_ref: None, promotion_ref: None, rollback_ref: None, successor_proposal_id: None },
    }, old)
}

fn governor_proposal(record: &experiment::ExperimentRecord, existing: &[ImprovementProposal]) -> ImprovementProposal {
    let fingerprint = "governor:attempt-budget-environment-probe".to_string();
    let now = now_epoch_seconds();
    let old = existing_for_fingerprint(existing, &fingerprint);
    let mut source_evidence = record.evidence.clone();
    source_evidence.extend(record.trial_artifacts.clone());
    source_evidence.sort_by(|a, b| a.id.cmp(&b.id));
    source_evidence.dedup_by(|a, b| a.id == b.id && a.digest == b.digest);
    let source_ids: Vec<String> = source_evidence.iter().map(|item| item.id.clone()).collect();
    let mut missing = Vec::new();
    let eligible = record.evaluation.as_ref().is_some_and(experiment::EvaluationSummary::is_eligible_for_approval);
    if !eligible { missing.push("complete verified evaluation and cost coverage are required before queueing or approval".into()); }
    if record.parent_manifest_digest.is_empty() || record.candidate_manifest_digest.is_empty() { missing.push("parent and candidate manifests are required".into()); }
    let status = if eligible { ProposalStatus::Ready } else { ProposalStatus::Blocked };
    let confidence = if source_evidence.iter().any(|item| item.state == experiment::EvidenceState::Verified) { ProposalConfidence::EvidenceSupported } else { ProposalConfidence::Incomplete };
    let evaluation_digest = record.evaluation.as_ref().map(digest_json);
    preserve_lifecycle(ImprovementProposal {
        schema_version: IMPROVEMENT_SCHEMA_VERSION,
        proposal_id: "improvement-governor-attempt-budget-environment-probe".into(),
        fingerprint: fingerprint.clone(),
        created_at_epoch_seconds: old.map(|p| p.created_at_epoch_seconds).unwrap_or(now),
        updated_at_epoch_seconds: now,
        target: ProposalTarget { world_id: Some("governor".into()), repository: "externally-versioned Governor runtime".into(), component: "attempt-budget classification".into(), component_class: "governor".into(), experiment_target: "governor".into() },
        observed_friction: "Diagnostic setup counted toward substantive Governor attempts, causing avoidable attempt-budget escalations.".into(),
        source_evidence,
        recurrence: merge_recurrence(old, &fingerprint, now, &source_ids),
        falsifiable_hypothesis: record.hypothesis.clone(),
        predicted_measurable_effect: record.predicted_observable_effect.clone(),
        protected_invariants: vec!["two substantive failures must still escalate".into(), "negative-control substantive failures remain counted".into(), "owner approval remains external".into()],
        negative_controls: vec!["substantive failure sequence that must still trigger escalation".into(), "unrelated Governor criterion with no environment probe".into()],
        alternatives_considered: vec!["count every setup probe as substantive".into(), "change the live Governor without a matched trial".into()],
        trial: TrialComparison { parent_condition: format!("parent manifest {}", record.parent_manifest_digest), candidate_condition: format!("candidate manifest {}", record.candidate_manifest_digest), primary_measure: record.evaluation.as_ref().map(|e| e.primary_friction_measure.clone()).unwrap_or_else(|| "avoidable_attempt_budget_escalations_per_matched_opportunity".into()), matched_conditions: "same task revision, session, harness, and negative control".into() },
        estimated_blast_radius: "isolated Governor runtime and one bounded task".into(),
        reversibility: "candidate artifact rollback to parent manifest".into(),
        required_evaluator: "independent Governor evaluator with retained results digest".into(),
        confidence,
        status,
        missing_evidence: missing,
        lineage: ProposalLineage { source_observation_ids: source_ids, experiment_id: Some(record.identity.experiment_id.clone()), evaluation_digest, owner_decision_ref: record.release_authorization.as_ref().map(|a| a.review_reference.clone()), canary_ref: record.canary.as_ref().map(|_| format!("canary:{}", record.identity.experiment_id)), promotion_ref: matches!(record.state, experiment::ExperimentState::Active).then(|| format!("promotion:{}", record.identity.experiment_id)), rollback_ref: matches!(record.state, experiment::ExperimentState::RolledBack).then(|| format!("rollback:{}", record.identity.experiment_id)), successor_proposal_id: None },
    }, old)
}

fn load_governor_records(state: &Path) -> io::Result<Vec<experiment::ExperimentRecord>> {
    let mut records = Vec::new();
    let dir = state.join("experiments");
    let Ok(entries) = fs::read_dir(dir) else { return Ok(records); };
    for entry in entries {
        let path = entry?.path();
        if path.extension().and_then(|x| x.to_str()) != Some("json") || path.file_name().and_then(|x| x.to_str()).is_some_and(|x| x.ends_with(".baseline.json")) { continue; }
        if let Ok(bytes) = fs::read(&path) {
            if let Ok(record) = serde_json::from_slice::<experiment::ExperimentRecord>(&bytes) {
                if record.identity.case_id.contains("attempt-budget") || record.evaluation.as_ref().is_some_and(|e| e.primary_friction_measure.contains("attempt_budget")) { records.push(record); }
            }
        }
    }
    Ok(records)
}

pub fn compile(
    config: &ProjectConfig,
    events: &[worldstate::WorldEvent],
    state: &Path,
    filter_world: Option<&str>,
) -> io::Result<ImprovementCompileReport> {
    let existing = load_proposals(state)?;
    let report = worldstate::analyze_workstation_friction(config, events, filter_world);
    let mut by_fingerprint: BTreeMap<String, ImprovementProposal> = BTreeMap::new();
    for pattern in &report.patterns {
        let proposal = static_proposal(pattern, events, config, &existing);
        by_fingerprint.entry(proposal.fingerprint.clone()).or_insert(proposal);
    }
    for record in load_governor_records(state)? {
        let proposal = governor_proposal(&record, &existing);
        if filter_world.is_none_or(|world| world.eq_ignore_ascii_case("governor")) { by_fingerprint.insert(proposal.fingerprint.clone(), proposal); }
    }
    // A repeated attention interruption is a real observation, but it is not
    // automatically an eligible experiment: matched user-impact evidence is
    // required before proposing a candidate change.
    let interruptions: Vec<_> = events.iter().filter(|event| event.kind == "attention.opened" && event.summary.contains("waiting_approval")).collect();
    if interruptions.len() >= 2 && filter_world.is_none_or(|world| world.eq_ignore_ascii_case("workstation")) {
        let fingerprint = "workstation:approval-interruption-recurrence".to_string();
        let old = existing_for_fingerprint(&existing, &fingerprint);
        let source_evidence: Vec<_> = interruptions.iter().map(|event| event_evidence(event)).collect();
        let source_ids = source_evidence.iter().map(|item| item.id.clone()).collect::<Vec<_>>();
        let now = now_epoch_seconds();
        by_fingerprint.insert(fingerprint.clone(), ImprovementProposal {
            schema_version: IMPROVEMENT_SCHEMA_VERSION,
            proposal_id: "improvement-workstation-approval-interruption-recurrence".into(),
            fingerprint: fingerprint.clone(),
            created_at_epoch_seconds: old.map(|p| p.created_at_epoch_seconds).unwrap_or(now),
            updated_at_epoch_seconds: now,
            target: ProposalTarget { world_id: None, repository: "task-fabric-svn".into(), component: "task-fabric-svn-workd attention relay".into(), component_class: "workstation-attention".into(), experiment_target: "task-fabric-svn".into() },
            observed_friction: "Approval-waiting attention interruptions recur across workstation events.".into(),
            source_evidence,
            recurrence: merge_recurrence(old, &fingerprint, now, &source_ids),
            falsifiable_hypothesis: "Grouping or deduplicating repeated approval-waiting notifications will reduce owner interruptions without hiding a new approval request.".into(),
            predicted_measurable_effect: "fewer repeated approval interruptions per active session, with every distinct approval still observable".into(),
            protected_invariants: vec!["every new approval remains visible".into(), "approval state is never auto-accepted".into(), "owner conversation history is unchanged".into()],
            negative_controls: vec!["distinct approval requests in one session must remain separately attributable".into()],
            alternatives_considered: vec!["leave repeated interruptions unchanged".into(), "suppress all approval notifications".into()],
            trial: TrialComparison { parent_condition: "current attention relay".into(), candidate_condition: "isolated relay candidate".into(), primary_measure: "deduplicated approval interruptions per session".into(), matched_conditions: "same event stream and session mix".into() },
            estimated_blast_radius: "attention presentation only; no authority or target mutation".into(),
            reversibility: "disable isolated presentation candidate".into(),
            required_evaluator: "independent owner-attention evaluator".into(),
            confidence: ProposalConfidence::Incomplete,
            status: ProposalStatus::Blocked,
            missing_evidence: vec!["matched parent/candidate trial is not retained".into(), "owner-impact measurement and independent evaluator are missing".into()],
            lineage: ProposalLineage { source_observation_ids: source_ids, experiment_id: None, evaluation_digest: None, owner_decision_ref: None, canary_ref: None, promotion_ref: None, rollback_ref: None, successor_proposal_id: None },
        });
    }
    let proposals: Vec<_> = by_fingerprint.into_values().collect();
    Ok(ImprovementCompileReport { schema_version: IMPROVEMENT_SCHEMA_VERSION, generated_at_epoch_seconds: now_epoch_seconds(), source_event_count: events.len(), proposals })
}

pub fn persist_report(state: &Path, report: &ImprovementCompileReport) -> io::Result<()> {
    for proposal in &report.proposals { save_proposal(state, proposal)?; }
    Ok(())
}

pub fn review(state: &Path, proposal_id: &str) -> io::Result<Option<ProposalReviewProjection>> {
    let Some(proposal) = load_proposal(state, proposal_id)? else { return Ok(None); };
    let linked_experiment = proposal.lineage.experiment_id.as_deref().and_then(|id| experiment::load_record(state, id).ok().flatten());
    Ok(Some(ProposalReviewProjection { proposal, linked_experiment }))
}

/// Queueing is deliberately a state-only handoff to the existing experiment
/// substrate. It never executes a candidate and never approves or promotes it.
pub fn queue_bounded_experiment(state: &Path, proposal_id: &str) -> io::Result<String> {
    let mut proposal = load_proposal(state, proposal_id)?.ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "proposal not found"))?;
    if proposal.status != ProposalStatus::Ready { return Err(io::Error::new(io::ErrorKind::PermissionDenied, format!("proposal is {:?}; missing evidence: {}", proposal.status, proposal.missing_evidence.join("; ")))); }
    if let Some(id) = proposal.lineage.experiment_id.clone() {
        let mut record = experiment::load_record(state, &id)?
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "linked experiment record is missing"))?;
        if matches!(record.state, experiment::ExperimentState::Proposed | experiment::ExperimentState::Inconclusive | experiment::ExperimentState::Rejected) {
            record.transition(experiment::ExperimentState::Evaluating)
                .map_err(|error| io::Error::new(io::ErrorKind::PermissionDenied, error))?;
            record.decision = Some(format!("queued from improvement proposal {}", proposal.proposal_id));
            experiment::save_record(state, &record)?;
        }
        proposal.status = ProposalStatus::Queued;
        proposal.updated_at_epoch_seconds = now_epoch_seconds();
        save_proposal(state, &proposal)?;
        return Ok(id);
    }
    Err(io::Error::new(io::ErrorKind::InvalidData, "ready proposal has no bounded experiment specification"))
}

impl worldstate::FrictionPattern {
    fn kind_string(&self) -> &'static str {
        match self.kind {
            worldstate::FrictionKind::PortConflict => "port-conflict",
            worldstate::FrictionKind::ServiceDegraded => "service-degraded",
            worldstate::FrictionKind::MissingScratchpad => "missing-scratchpad",
            worldstate::FrictionKind::DirtyWorkingTree => "dirty-working-tree",
            worldstate::FrictionKind::MissingAction => "missing-action",
            worldstate::FrictionKind::MissingAlias => "missing-alias",
            worldstate::FrictionKind::RepositoryMissing => "repository-missing",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Project, ProjectConfig};

    fn temp_state(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!("task-fabric-svn-improvement-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn proposal_contract_is_typed_and_source_linked() {
        let state = temp_state("contract");
        let config = ProjectConfig { version: 1, projects: vec![Project::default()] };
        let report = compile(&config, &[], &state, None).unwrap();
        assert!(!report.proposals.is_empty());
        assert!(report.proposals.iter().all(|proposal| !proposal.proposal_id.is_empty() && !proposal.source_evidence.is_empty()));
    }

    #[test]
    fn incomplete_proposal_fails_closed_when_queued() {
        let state = temp_state("blocked");
        let config = ProjectConfig { version: 1, projects: vec![Project::default()] };
        let report = compile(&config, &[], &state, None).unwrap();
        persist_report(&state, &report).unwrap();
        let proposal = &report.proposals[0];
        assert!(queue_bounded_experiment(&state, &proposal.proposal_id).is_err());
    }

    #[test]
    fn recurrence_merges_without_duplicate_proposal_ids() {
        let state = temp_state("recurrence");
        let config = ProjectConfig { version: 1, projects: vec![Project::default()] };
        let first = compile(&config, &[], &state, None).unwrap();
        persist_report(&state, &first).unwrap();
        let second = compile(&config, &[], &state, None).unwrap();
        let ids: BTreeSet<_> = second.proposals.iter().map(|p| p.proposal_id.clone()).collect();
        assert_eq!(ids.len(), second.proposals.len());
    }

    #[test]
    fn eligible_governor_proposal_hands_off_to_existing_experiment() {
        let state = temp_state("queue");
        let identity = experiment::ExperimentIdentity {
            experiment_id: "governor-eligible-test".into(),
            case_id: "criterion-attempt-budget-environment-probe".into(),
            candidate_id: "candidate-1".into(),
            run_id: "run-1".into(),
            session_id: "session-1".into(),
            task_revision: "task-1".into(),
            harness_manifest_digest: "a".repeat(64),
            trace_id: None,
        };
        let mut record = experiment::ExperimentRecord::new(
            identity,
            "b".repeat(64),
            "c".repeat(64),
            "classify probes separately".into(),
            "zero avoidable escalations".into(),
        );
        let source = evidence("retained-test-evidence", "d".repeat(64), experiment::EvidenceState::Verified);
        record.evidence.push(source.clone());
        record.evaluation = Some(experiment::EvaluationSummary {
            evaluator_digest: "e".repeat(64),
            results_digest: "f".repeat(64),
            primary_friction_measure: "attempt_budget".into(),
            baseline_value: Some(1),
            candidate_value: Some(0),
            quality_regression: false,
            essential_escalation_regression: false,
            matched_conditions: true,
            evidence_state: experiment::EvidenceState::Verified,
            cost: experiment::CostAccounting {
                acquisition_seconds: Some(1), generation_seconds: Some(1), evaluation_seconds: Some(1),
                review_seconds: Some(1), maintenance_seconds: Some(1), usage_tokens: Some(1),
                monetary_micros: Some(1), evidence: vec![source], unknown_components: Vec::new(),
            },
        });
        experiment::save_record(&state, &record).unwrap();
        let report = compile(&ProjectConfig { version: 1, projects: Vec::new() }, &[], &state, None).unwrap();
        let proposal = report.proposals.iter().find(|proposal| proposal.target.component_class == "governor").unwrap();
        assert_eq!(proposal.status, ProposalStatus::Ready);
        persist_report(&state, &report).unwrap();
        assert_eq!(queue_bounded_experiment(&state, &proposal.proposal_id).unwrap(), "governor-eligible-test");
        assert_eq!(load_proposal(&state, &proposal.proposal_id).unwrap().unwrap().status, ProposalStatus::Queued);
        let refreshed = compile(&ProjectConfig { version: 1, projects: Vec::new() }, &[], &state, None).unwrap();
        let refreshed_proposal = refreshed.proposals.iter().find(|item| item.proposal_id == proposal.proposal_id).unwrap();
        assert_eq!(refreshed_proposal.status, ProposalStatus::Queued);
    }
}
