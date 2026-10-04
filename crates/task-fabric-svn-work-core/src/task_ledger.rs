//! Durable Task Fabric ledger: the Task Contract v1 state machine.
//!
//! This is the execution primitive that replaces `agent.message` into the
//! void. A Task is bounded work delegated into a Space, executed by an
//! interchangeable harness, with its result routed back to the originating
//! conversation. The workstation owns this ledger: it is the durable,
//! single-writer record of every task's life, independent of any model or
//! harness process (those attach in later steps).
//!
//! Storage follows the worldstate conventions: an append-only JSONL journal
//! (`task-events.jsonl`) for the full history and one atomic JSON snapshot
//! per task (`tasks/<task_id>.json`) for the current record. No raw voice
//! transcripts are ever stored here — tasks carry intent summaries, typed
//! parameters, and references, per the contract's privacy direction.

use crate::worldstate::{self, MAX_STRING};
use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::PathBuf;
use thiserror::Error;

/// Task Contract schema version. Bump on any breaking record change.
pub const TASK_SCHEMA_VERSION: u32 = 1;

/// Tight cap for identifiers (task ids, conversation ids, space ids, ...).
pub const MAX_ID: usize = 128;
/// Cap for human-scale text fields: intent summaries, questions, results.
pub const MAX_TEXT: usize = 4_000;

#[derive(Debug, Error)]
pub enum TaskError {
    #[error("task not found: {0}")]
    NotFound(String),
    #[error("invalid transition: {from} -> {to}")]
    InvalidTransition { from: TaskState, to: TaskState },
    #[error("task {task_id} is terminal ({state}); no further transitions")]
    Terminal { task_id: String, state: TaskState },
    #[error("invalid task field: {0}")]
    Validation(String),
    #[error("resume token mismatch")]
    ResumeTokenMismatch,
    #[error("io: {0}")]
    Io(#[from] io::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
}

/// Lifecycle states from Task Contract v1.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskState {
    Queued,
    Starting,
    Working,
    InputRequired,
    ApprovalRequired,
    Completed,
    Failed,
    Cancelled,
}

impl TaskState {
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            TaskState::Completed | TaskState::Failed | TaskState::Cancelled
        )
    }

    /// The full transition table. Anything not listed here is rejected.
    pub fn can_transition_to(self, next: TaskState) -> bool {
        use TaskState::*;
        match (self, next) {
            (Queued, Starting) => true,
            (Queued, Failed) => true,    // budget/deadline pre-checks
            (Queued, Cancelled) => true, // cancelled before a worker claimed it
            (Starting, Working) => true,
            (Starting, Failed) => true,
            (Starting, Cancelled) => true,
            (Working, InputRequired) => true,
            (Working, ApprovalRequired) => true,
            (Working, Completed) => true,
            (Working, Failed) => true,
            (Working, Cancelled) => true,
            (InputRequired, Working) => true, // task.respond
            (InputRequired, Failed) => true,
            (InputRequired, Cancelled) => true,
            (ApprovalRequired, Working) => true,   // task.approve(approved=true)
            (ApprovalRequired, Cancelled) => true,  // task.approve(approved=false)
            (ApprovalRequired, Failed) => true,
            _ => false,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            TaskState::Queued => "queued",
            TaskState::Starting => "starting",
            TaskState::Working => "working",
            TaskState::InputRequired => "input_required",
            TaskState::ApprovalRequired => "approval_required",
            TaskState::Completed => "completed",
            TaskState::Failed => "failed",
            TaskState::Cancelled => "cancelled",
        }
    }
}

impl std::fmt::Display for TaskState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Risk classes from the contract. Gates approvals and budgets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RiskClass {
    Read,
    WriteSandbox,
    WriteRepo,
    Exec,
    External,
}

/// One step in the task's state history.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TaskStateTransition {
    pub from: TaskState,
    pub to: TaskState,
    pub at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// The durable task record. Deliberately contains no raw transcripts.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TaskRecord {
    pub schema_version: u32,
    pub task_id: String,
    pub conversation_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_task_id: Option<String>,
    pub target_space_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_repo_identity: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_revision: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness_session_id: Option<String>,
    pub intent_summary: String,
    pub risk_class: RiskClass,
    pub status: TaskState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub correlation_id: Option<String>,
    #[serde(default)]
    pub parameters: serde_json::Value,
    /// Set while `input_required`: the worker's question.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_question: Option<String>,
    /// Server-issued identity for the current input prompt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_generation: Option<String>,
    /// Set while `input_required`: must match on `task.respond`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resume_token: Option<String>,
    /// Set while `approval_required`: the action the worker proposed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval_action: Option<String>,
    /// Server-issued identity for the current approval gate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval_generation: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result_summary: Option<String>,
    #[serde(default)]
    pub artifact_refs: Vec<String>,
    #[serde(default)]
    pub receipt_refs: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure_reason: Option<String>,
    pub created_at: u64,
    pub updated_at: u64,
    #[serde(default)]
    pub state_history: Vec<TaskStateTransition>,
}

/// What the submitter provides. `task.submit`.
#[derive(Debug, Clone)]
pub struct TaskSubmitRequest {
    pub conversation_id: String,
    pub parent_task_id: Option<String>,
    pub target_space_id: String,
    pub target_repo_identity: Option<String>,
    pub target_revision: Option<String>,
    /// Requested harness; the worker may confirm/assign at start.
    pub harness: Option<String>,
    pub intent_summary: String,
    pub risk_class: RiskClass,
    pub trace_id: Option<String>,
    pub correlation_id: Option<String>,
    /// Typed parameters for the worker. Never raw transcripts.
    pub parameters: serde_json::Value,
}

/// Journal event kinds. `task-events.jsonl` is the source of truth for history.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskEventKind {
    Submitted,
    Started,
    Progress,
    InputRequired,
    InputResponded,
    ApprovalRequired,
    ApprovalResolved,
    Completed,
    Failed,
    Cancelled,
    Note,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskEvent {
    pub schema_version: u32,
    pub seq: u64,
    pub task_id: String,
    pub kind: TaskEventKind,
    pub at: u64,
    #[serde(default)]
    pub payload: serde_json::Value,
}

/// Filter for listing tasks.
#[derive(Debug, Clone, Default)]
pub struct TaskListFilter {
    pub state: Option<TaskState>,
    pub conversation_id: Option<String>,
    pub limit: Option<usize>,
}

/// The durable ledger. Single writer per process; storage is file-based so a
/// restarted daemon replays from disk.
pub struct TaskLedger {
    dir: PathBuf,
}

/// Advisory process lock held for the full read/modify/journal/snapshot cycle.
/// The lock file is intentionally persistent; flock ownership follows the fd.
struct LedgerLock(File);

impl LedgerLock {
    fn acquire(dir: &PathBuf) -> Result<Self, TaskError> {
        fs::create_dir_all(dir)?;
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .open(dir.join("task-ledger.lock"))?;
        #[cfg(unix)]
        // SAFETY: flock only borrows the valid descriptor owned by `file`.
        if unsafe { libc::flock(std::os::fd::AsRawFd::as_raw_fd(&file), libc::LOCK_EX) } != 0 {
            return Err(TaskError::Io(io::Error::last_os_error()));
        }
        Ok(Self(file))
    }
}

impl Drop for LedgerLock {
    fn drop(&mut self) {
        #[cfg(unix)]
        // SAFETY: the descriptor remains owned by this guard until drop ends.
        unsafe {
            libc::flock(std::os::fd::AsRawFd::as_raw_fd(&self.0), libc::LOCK_UN);
        }
    }
}

impl TaskLedger {
    /// Ledger rooted at the standard workstation state dir.
    pub fn new() -> Self {
        Self {
            dir: worldstate::state_dir(),
        }
    }

    /// Ledger rooted at an explicit dir (tests, tooling).
    pub fn with_dir(dir: PathBuf) -> Self {
        Self { dir }
    }

    fn journal_path(&self) -> PathBuf {
        self.dir.join("task-events.jsonl")
    }

    fn snapshot_path(&self, task_id: &str) -> PathBuf {
        self.dir.join("tasks").join(format!("{task_id}.json"))
    }

    fn lock(&self) -> Result<LedgerLock, TaskError> {
        LedgerLock::acquire(&self.dir)
    }

    // ---- task.submit ----

    pub fn submit(&self, req: TaskSubmitRequest) -> Result<TaskRecord, TaskError> {
        validate_id("conversation_id", &req.conversation_id)?;
        if let Some(parent) = &req.parent_task_id {
            validate_id("parent_task_id", parent)?;
        }
        validate_id("target_space_id", &req.target_space_id)?;
        if let Some(repo) = &req.target_repo_identity {
            validate_id("target_repo_identity", repo)?;
        }
        if let Some(rev) = &req.target_revision {
            validate_id("target_revision", rev)?;
        }
        if let Some(harness) = &req.harness {
            validate_id("harness", harness)?;
        }
        validate_text("intent_summary", &req.intent_summary, true)?;
        if let Some(trace) = &req.trace_id {
            validate_id("trace_id", trace)?;
        }
        if let Some(corr) = &req.correlation_id {
            validate_id("correlation_id", corr)?;
        }

        let _lock = self.lock()?;
        // A conversation-scoped correlation id is the submit idempotency key.
        if let Some(correlation_id) = &req.correlation_id {
            if let Some(existing) = self
                .list(TaskListFilter {
                    conversation_id: Some(req.conversation_id.clone()),
                    ..Default::default()
                })?
                .into_iter()
                .find(|r| r.correlation_id.as_ref() == Some(correlation_id))
            {
                let same_request = existing.target_space_id == req.target_space_id
                    && existing.parent_task_id == req.parent_task_id
                    && existing.target_repo_identity == req.target_repo_identity
                    && existing.target_revision == req.target_revision
                    && existing.harness == req.harness
                    && existing.intent_summary == req.intent_summary
                    && existing.risk_class == req.risk_class
                    && existing.trace_id == req.trace_id
                    && existing.parameters == req.parameters;
                if same_request {
                    return Ok(existing);
                }
                return Err(TaskError::Validation(
                    "correlation_id already used for a different task submission".into(),
                ));
            }
        }

        let now = crate::now_epoch_seconds();
        let task_id = format!("task-{}", &uuid::Uuid::new_v4().simple().to_string()[..12]);
        let record = TaskRecord {
            schema_version: TASK_SCHEMA_VERSION,
            task_id: task_id.clone(),
            conversation_id: req.conversation_id,
            parent_task_id: req.parent_task_id,
            target_space_id: req.target_space_id,
            target_repo_identity: req.target_repo_identity,
            target_revision: req.target_revision,
            harness: req.harness,
            harness_session_id: None,
            intent_summary: req.intent_summary,
            risk_class: req.risk_class,
            status: TaskState::Queued,
            trace_id: req.trace_id,
            correlation_id: req.correlation_id,
            parameters: req.parameters,
            input_question: None,
            input_generation: None,
            resume_token: None,
            approval_action: None,
            approval_generation: None,
            result_summary: None,
            artifact_refs: Vec::new(),
            receipt_refs: Vec::new(),
            failure_reason: None,
            created_at: now,
            updated_at: now,
            state_history: vec![TaskStateTransition {
                from: TaskState::Queued,
                to: TaskState::Queued,
                at: now,
                reason: Some("submitted".into()),
            }],
        };
        self.append_event(
            &task_id,
            TaskEventKind::Submitted,
            serde_json::json!({
                "conversation_id": record.conversation_id,
                "target_space_id": record.target_space_id,
                "risk_class": record.risk_class,
                "intent_summary": record.intent_summary,
            }),
        )?;
        self.write_snapshot(&record)?;
        Ok(record)
    }

    // ---- task.inspect ----

    /// Read the current record for a task. `Ok(None)` when it does not exist.
    pub fn inspect(&self, task_id: &str) -> Result<Option<TaskRecord>, TaskError> {
        validate_id("task_id", task_id)?;
        match fs::read(self.snapshot_path(task_id)) {
            Ok(bytes) => {
                let record: TaskRecord = serde_json::from_slice(&bytes)?;
                if record.schema_version != TASK_SCHEMA_VERSION {
                    return Err(TaskError::Validation(format!(
                        "unsupported task schema version {}",
                        record.schema_version
                    )));
                }
                Ok(Some(record))
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(TaskError::Io(e)),
        }
    }

    fn require(&self, task_id: &str) -> Result<TaskRecord, TaskError> {
        self.inspect(task_id)?
            .ok_or_else(|| TaskError::NotFound(task_id.to_string()))
    }

    // ---- worker-side transitions ----

    /// Worker claims a queued task. Records the harness-side session id.
    pub fn start(&self, task_id: &str, harness_session_id: &str) -> Result<TaskRecord, TaskError> {
        let _lock = self.lock()?;
        validate_id("task_id", task_id)?;
        validate_id("harness_session_id", harness_session_id)?;
        let mut record = self.require(task_id)?;
        self.transition(&mut record, TaskState::Starting, None)?;
        record.harness_session_id = Some(harness_session_id.to_string());
        self.append_event(
            task_id,
            TaskEventKind::Started,
            serde_json::json!({ "harness_session_id": harness_session_id }),
        )?;
        self.write_snapshot(&record)?;
        Ok(record)
    }

    /// Worker begins execution proper: starting -> working.
    pub fn begin_work(&self, task_id: &str) -> Result<TaskRecord, TaskError> {
        self.simple_transition(
            task_id,
            TaskState::Working,
            TaskEventKind::Progress,
            None,
            None,
        )
    }

    /// Worker note while working (stays in working; journal only).
    pub fn note_progress(&self, task_id: &str, note: &str) -> Result<TaskRecord, TaskError> {
        let _lock = self.lock()?;
        validate_text("note", note, true)?;
        let mut record = self.require(task_id)?;
        if record.status != TaskState::Working {
            return Err(TaskError::InvalidTransition {
                from: record.status,
                to: TaskState::Working,
            });
        }
        record.updated_at = crate::now_epoch_seconds();
        self.append_event(
            task_id,
            TaskEventKind::Progress,
            serde_json::json!({ "note": note }),
        )?;
        self.write_snapshot(&record)?;
        Ok(record)
    }

    /// Worker asks a question: working -> input_required.
    pub fn request_input(
        &self,
        task_id: &str,
        question: &str,
        resume_token: &str,
    ) -> Result<TaskRecord, TaskError> {
        let _lock = self.lock()?;
        validate_text("question", question, true)?;
        validate_id("resume_token", resume_token)?;
        let mut record = self.require(task_id)?;
        self.transition(&mut record, TaskState::InputRequired, None)?;
        record.input_question = Some(question.to_string());
        record.resume_token = Some(resume_token.to_string());
        record.input_generation = Some(uuid::Uuid::new_v4().simple().to_string());
        self.append_event(
            task_id,
            TaskEventKind::InputRequired,
            serde_json::json!({ "question": question, "input_generation": record.input_generation }),
        )?;
        self.write_snapshot(&record)?;
        Ok(record)
    }

    // ---- task.respond ----

    /// Answer the worker's question: input_required -> working. The token must
    /// match the one issued with the question.
    pub fn respond(
        &self,
        task_id: &str,
        resume_token: &str,
        answer: &str,
    ) -> Result<TaskRecord, TaskError> {
        let _lock = self.lock()?;
        self.respond_locked(task_id, None, resume_token, answer)
    }

    /// Answer only the input prompt generation that was presented to the user.
    pub fn respond_generation(
        &self,
        task_id: &str,
        generation: &str,
        answer: &str,
    ) -> Result<TaskRecord, TaskError> {
        validate_id("input_generation", generation)?;
        let _lock = self.lock()?;
        self.respond_locked(task_id, Some(generation), "", answer)
    }

    fn respond_locked(
        &self,
        task_id: &str,
        generation: Option<&str>,
        resume_token: &str,
        answer: &str,
    ) -> Result<TaskRecord, TaskError> {
        validate_text("answer", answer, true)?;
        validate_id("task_id", task_id)?;
        let mut record = self.require(task_id)?;
        if record.status != TaskState::InputRequired {
            return Err(TaskError::InvalidTransition {
                from: record.status,
                to: TaskState::Working,
            });
        }
        if generation.is_some() && record.input_generation.as_deref() != generation {
            return Err(TaskError::Validation("stale input generation".into()));
        }
        if generation.is_none() && record.resume_token.as_deref() != Some(resume_token) {
            return Err(TaskError::ResumeTokenMismatch);
        }
        record.input_question = None;
        record.resume_token = None;
        record.input_generation = None;
        self.transition(&mut record, TaskState::Working, None)?;
        self.append_event(
            task_id,
            TaskEventKind::InputResponded,
            serde_json::json!({ "answer": answer }),
        )?;
        self.write_snapshot(&record)?;
        Ok(record)
    }

    /// Worker proposes a gated action: working -> approval_required.
    pub fn request_approval(&self, task_id: &str, action: &str) -> Result<TaskRecord, TaskError> {
        let _lock = self.lock()?;
        validate_text("action", action, true)?;
        let mut record = self.require(task_id)?;
        self.transition(&mut record, TaskState::ApprovalRequired, None)?;
        record.approval_action = Some(action.to_string());
        let generation = uuid::Uuid::new_v4().simple().to_string();
        record.approval_generation = Some(generation.clone());
        self.append_event(
            task_id,
            TaskEventKind::ApprovalRequired,
            serde_json::json!({
                "action": action,
                "risk_class": record.risk_class,
                "approval_generation": generation,
            }),
        )?;
        self.write_snapshot(&record)?;
        Ok(record)
    }

    // ---- task.approve ----

    /// Resolve the approval gate. `approved=true` resumes working;
    /// `approved=false` cancels the task.
    pub fn approve(
        &self,
        task_id: &str,
        approved: bool,
        reason: Option<&str>,
    ) -> Result<TaskRecord, TaskError> {
        let _lock = self.lock()?;
        self.approve_locked(task_id, approved, reason, None)
    }

    /// Resolve only the approval prompt whose generation was presented.
    pub fn approve_generation(
        &self,
        task_id: &str,
        generation: &str,
        approved: bool,
        reason: Option<&str>,
    ) -> Result<TaskRecord, TaskError> {
        validate_id("approval_generation", generation)?;
        let _lock = self.lock()?;
        self.approve_locked(task_id, approved, reason, Some(generation))
    }

    fn approve_locked(
        &self,
        task_id: &str,
        approved: bool,
        reason: Option<&str>,
        generation: Option<&str>,
    ) -> Result<TaskRecord, TaskError> {
        if let Some(reason) = reason {
            validate_text("reason", reason, false)?;
        }
        let mut record = self.require(task_id)?;
        if record.status != TaskState::ApprovalRequired {
            return Err(TaskError::InvalidTransition {
                from: record.status,
                to: if approved {
                    TaskState::Working
                } else {
                    TaskState::Cancelled
                },
            });
        }
        if generation.is_some() && record.approval_generation.as_deref() != generation {
            return Err(TaskError::Validation("stale approval generation".into()));
        }
        record.approval_action = None;
        record.approval_generation = None;
        let next = if approved {
            TaskState::Working
        } else {
            TaskState::Cancelled
        };
        self.transition(&mut record, next, reason.map(str::to_string))?;
        self.append_event(
            task_id,
            TaskEventKind::ApprovalResolved,
            serde_json::json!({ "approved": approved, "reason": reason }),
        )?;
        self.write_snapshot(&record)?;
        Ok(record)
    }

    /// Worker finished: working -> completed.
    pub fn complete(
        &self,
        task_id: &str,
        result_summary: &str,
        artifact_refs: &[String],
        receipt_refs: &[String],
    ) -> Result<TaskRecord, TaskError> {
        let _lock = self.lock()?;
        validate_text("result_summary", result_summary, true)?;
        for r in artifact_refs.iter().chain(receipt_refs.iter()) {
            validate_id("ref", r)?;
        }
        let mut record = self.require(task_id)?;
        self.transition(&mut record, TaskState::Completed, None)?;
        record.result_summary = Some(result_summary.to_string());
        record.artifact_refs = artifact_refs.to_vec();
        record.receipt_refs = receipt_refs.to_vec();
        self.append_event(
            task_id,
            TaskEventKind::Completed,
            serde_json::json!({
                "result_summary": result_summary,
                "artifact_refs": artifact_refs,
                "receipt_refs": receipt_refs,
            }),
        )?;
        self.write_snapshot(&record)?;
        Ok(record)
    }

    /// Terminal failure from any non-terminal state. Budget/deadline failures
    /// land here with reason `budget_exhausted` / `deadline_exceeded` and are
    /// never silently retried on another model.
    pub fn fail(&self, task_id: &str, reason: &str) -> Result<TaskRecord, TaskError> {
        let _lock = self.lock()?;
        validate_text("reason", reason, true)?;
        validate_id("task_id", task_id)?;
        let mut record = self.require(task_id)?;
        self.transition(&mut record, TaskState::Failed, Some(reason.to_string()))?;
        record.failure_reason = Some(reason.to_string());
        self.append_event(
            task_id,
            TaskEventKind::Failed,
            serde_json::json!({ "reason": reason }),
        )?;
        self.write_snapshot(&record)?;
        Ok(record)
    }

    // ---- task.cancel ----

    /// Cancel from any non-terminal state.
    pub fn cancel(&self, task_id: &str, reason: &str) -> Result<TaskRecord, TaskError> {
        validate_text("reason", reason, true)?;
        self.simple_transition(
            task_id,
            TaskState::Cancelled,
            TaskEventKind::Cancelled,
            Some(reason.to_string()),
            None,
        )
    }

    /// Cancel only if the generation presented to the user is still current.
    /// The comparison and transition share the ledger lock across processes.
    pub fn cancel_generation(
        &self,
        task_id: &str,
        generation: &str,
        reason: &str,
    ) -> Result<TaskRecord, TaskError> {
        validate_text("reason", reason, true)?;
        validate_id("task_id", task_id)?;
        validate_id("generation", generation)?;
        let _lock = self.lock()?;
        let mut record = self.require(task_id)?;
        let current_generation = record.input_generation.as_deref()
            .or(record.approval_generation.as_deref());
        if current_generation != Some(generation) {
            return Err(TaskError::Validation("stale task generation".into()));
        }
        self.transition(&mut record, TaskState::Cancelled, Some(reason.to_string()))?;
        self.append_event(task_id, TaskEventKind::Cancelled, serde_json::json!({"reason": reason}))?;
        self.write_snapshot(&record)?;
        Ok(record)
    }

    /// Journal history for a task in append order.
    pub fn history(&self, task_id: &str, limit: usize) -> Result<Vec<TaskEvent>, TaskError> {
        validate_id("task_id", task_id)?;
        let text = fs::read_to_string(self.journal_path()).unwrap_or_default();
        let mut events = text
            .lines()
            .filter_map(|line| serde_json::from_str::<TaskEvent>(line).ok())
            .filter(|event| event.task_id == task_id)
            .collect::<Vec<_>>();
        let start = events.len().saturating_sub(limit.min(MAX_STRING));
        Ok(events.drain(start..).collect())
    }

    /// List snapshots, newest first, with optional filters.
    pub fn list(&self, filter: TaskListFilter) -> Result<Vec<TaskRecord>, TaskError> {
        let mut records = Vec::new();
        let dir = self.dir.join("tasks");
        let entries = fs::read_dir(&dir)
            .map(|it| it.collect::<Vec<_>>())
            .unwrap_or_default();
        for entry in entries.into_iter().flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let bytes = match fs::read(&path) {
                Ok(bytes) => bytes,
                Err(_) => continue,
            };
            let record: TaskRecord = match serde_json::from_slice(&bytes) {
                Ok(record) => record,
                Err(_) => continue,
            };
            if record.schema_version != TASK_SCHEMA_VERSION {
                continue;
            }
            if let Some(state) = filter.state {
                if record.status != state {
                    continue;
                }
            }
            if let Some(conv) = &filter.conversation_id {
                if record.conversation_id != *conv {
                    continue;
                }
            }
            records.push(record);
        }
        records.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        if let Some(limit) = filter.limit {
            records.truncate(limit);
        }
        Ok(records)
    }

    // ---- internals ----

    fn transition(
        &self,
        record: &mut TaskRecord,
        next: TaskState,
        reason: Option<String>,
    ) -> Result<(), TaskError> {
        let from = record.status;
        if from.is_terminal() {
            return Err(TaskError::Terminal {
                task_id: record.task_id.clone(),
                state: from,
            });
        }
        if !from.can_transition_to(next) {
            return Err(TaskError::InvalidTransition { from, to: next });
        }
        let now = crate::now_epoch_seconds();
        record.status = next;
        record.updated_at = now;
        record.state_history.push(TaskStateTransition {
            from,
            to: next,
            at: now,
            reason,
        });
        Ok(())
    }

    fn simple_transition(
        &self,
        task_id: &str,
        next: TaskState,
        kind: TaskEventKind,
        reason: Option<String>,
        journal_reason: Option<String>,
    ) -> Result<TaskRecord, TaskError> {
        let _lock = self.lock()?;
        validate_id("task_id", task_id)?;
        let mut record = self.require(task_id)?;
        let from = record.status;
        self.transition(&mut record, next, reason)?;
        self.append_event(
            task_id,
            kind,
            serde_json::json!({ "from": from.as_str(), "to": next.as_str(), "reason": journal_reason }),
        )?;
        self.write_snapshot(&record)?;
        Ok(record)
    }

    fn append_event(
        &self,
        task_id: &str,
        kind: TaskEventKind,
        payload: serde_json::Value,
    ) -> Result<(), TaskError> {
        let path = self.journal_path();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let seq = fs::read_to_string(&path)
            .map(|text| text.lines().filter(|l| !l.trim().is_empty()).count() as u64)
            .unwrap_or(0);
        let mut file = OpenOptions::new().create(true).append(true).open(&path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(fs::Permissions::from_mode(0o600))?;
        }
        let event = TaskEvent {
            schema_version: TASK_SCHEMA_VERSION,
            seq,
            task_id: task_id.to_string(),
            kind,
            at: crate::now_epoch_seconds(),
            payload,
        };
        // JSONL records must remain one physical line.
        let bytes = serde_json::to_vec(&event)?;
        file.write_all(&bytes)?;
        file.write_all(b"\n")?;
        Ok(())
    }

    fn write_snapshot(&self, record: &TaskRecord) -> Result<(), TaskError> {
        let dir = self.dir.join("tasks");
        fs::create_dir_all(&dir)?;
        worldstate::atomic_write(&self.snapshot_path(&record.task_id), record)?;
        Ok(())
    }
}

fn validate_id(field: &str, value: &str) -> Result<(), TaskError> {
    if value.is_empty() || value.len() > MAX_ID {
        return Err(TaskError::Validation(format!(
            "{field} must be 1..={MAX_ID} chars"
        )));
    }
    if value.contains('/') || value.contains('\\') || value.contains("..") {
        return Err(TaskError::Validation(format!("{field} is not a safe id")));
    }
    if value.chars().any(|c| c.is_control()) {
        return Err(TaskError::Validation(format!(
            "{field} contains control chars"
        )));
    }
    Ok(())
}

fn validate_text(field: &str, value: &str, required: bool) -> Result<(), TaskError> {
    if value.trim().is_empty() {
        if required {
            return Err(TaskError::Validation(format!("{field} must not be empty")));
        }
        return Ok(());
    }
    if value.len() > MAX_TEXT {
        return Err(TaskError::Validation(format!(
            "{field} exceeds {MAX_TEXT} chars"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn test_ledger() -> TaskLedger {
        let n = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!(
            "task-fabric-svn-task-ledger-test-{}-{}",
            std::process::id(),
            n
        ));
        let _ = fs::remove_dir_all(&dir);
        TaskLedger::with_dir(dir)
    }

    fn submit_request(conversation: &str) -> TaskSubmitRequest {
        TaskSubmitRequest {
            conversation_id: conversation.into(),
            parent_task_id: None,
            target_space_id: "space-example-press".into(),
            target_repo_identity: Some("example-press".into()),
            target_revision: None,
            harness: Some("opencode".into()),
            intent_summary: "Summarize the latest draft.".into(),
            risk_class: RiskClass::Read,
            trace_id: Some("trace-1".into()),
            correlation_id: None,
            parameters: serde_json::json!({ "draft": "latest" }),
        }
    }

    fn run_to_working(ledger: &TaskLedger, task_id: &str) -> TaskRecord {
        ledger.start(task_id, "harness-session-1").expect("start");
        ledger.begin_work(task_id).expect("begin_work")
    }

    #[test]
    fn submit_creates_queued_task_with_journal_entry() {
        let ledger = test_ledger();
        let record = ledger.submit(submit_request("conv-1")).expect("submit");

        assert!(record.task_id.starts_with("task-"));
        assert_eq!(record.status, TaskState::Queued);
        assert_eq!(record.schema_version, TASK_SCHEMA_VERSION);
        assert_eq!(record.input_question, None);
        assert_eq!(record.result_summary, None);

        let history = ledger.history(&record.task_id, 10).expect("history");
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].kind, TaskEventKind::Submitted);
        assert_eq!(history[0].seq, 0);
    }

    #[test]
    fn happy_path_to_completed() {
        let ledger = test_ledger();
        let record = ledger.submit(submit_request("conv-1")).expect("submit");

        let record = ledger.start(&record.task_id, "sess-9").expect("start");
        assert_eq!(record.status, TaskState::Starting);
        assert_eq!(record.harness_session_id.as_deref(), Some("sess-9"));

        let record = ledger.begin_work(&record.task_id).expect("begin_work");
        assert_eq!(record.status, TaskState::Working);

        let refs = vec!["artifact-1".to_string()];
        let receipts = vec!["receipt-1".to_string()];
        let record = ledger
            .complete(&record.task_id, "Draft summarized.", &refs, &receipts)
            .expect("complete");
        assert_eq!(record.status, TaskState::Completed);
        assert_eq!(record.result_summary.as_deref(), Some("Draft summarized."));
        assert_eq!(record.artifact_refs, refs);
        assert_eq!(record.receipt_refs, receipts);

        // Terminal: nothing further may transition.
        let err = ledger.cancel(&record.task_id, "too late").unwrap_err();
        assert!(matches!(err, TaskError::Terminal { .. }));
        let err = ledger.begin_work(&record.task_id).unwrap_err();
        assert!(matches!(err, TaskError::Terminal { .. }));
    }

    #[test]
    fn input_required_roundtrip_requires_matching_token() {
        let ledger = test_ledger();
        let record = ledger.submit(submit_request("conv-1")).expect("submit");
        let record = run_to_working(&ledger, &record.task_id);

        let record = ledger
            .request_input(&record.task_id, "Which draft?", "tok-abc")
            .expect("request_input");
        assert_eq!(record.status, TaskState::InputRequired);
        assert_eq!(record.input_question.as_deref(), Some("Which draft?"));

        let err = ledger
            .respond(&record.task_id, "tok-wrong", "the latest")
            .unwrap_err();
        assert!(matches!(err, TaskError::ResumeTokenMismatch));

        let record = ledger
            .respond(&record.task_id, "tok-abc", "the latest")
            .expect("respond");
        assert_eq!(record.status, TaskState::Working);
        assert_eq!(record.input_question, None);
        assert_eq!(record.resume_token, None);

        // respond outside input_required is an invalid transition.
        let err = ledger
            .respond(&record.task_id, "tok-abc", "again")
            .unwrap_err();
        assert!(matches!(err, TaskError::InvalidTransition { .. }));
    }

    #[test]
    fn stale_input_generation_cannot_answer_a_later_prompt() {
        let ledger = test_ledger();
        let record = ledger.submit(submit_request("conv-1")).unwrap();
        let record = run_to_working(&ledger, &record.task_id);
        let first = ledger
            .request_input(&record.task_id, "first?", "reused-token")
            .unwrap();
        let old_generation = first.input_generation.clone().unwrap();
        ledger
            .respond_generation(&record.task_id, &old_generation, "answer")
            .unwrap();
        let second = ledger
            .request_input(&record.task_id, "second?", "reused-token")
            .unwrap();
        assert_ne!(
            second.input_generation.as_deref(),
            Some(old_generation.as_str())
        );
        assert!(ledger
            .respond_generation(&record.task_id, &old_generation, "stale")
            .is_err());
        assert_eq!(
            ledger.inspect(&record.task_id).unwrap().unwrap().status,
            TaskState::InputRequired
        );
    }

    #[test]
    fn approval_gate_yes_resumes_no_cancels() {
        let ledger = test_ledger();
        let record = ledger.submit(submit_request("conv-1")).expect("submit");
        let record = run_to_working(&ledger, &record.task_id);

        let record = ledger
            .request_approval(&record.task_id, "commit to main")
            .expect("request_approval");
        assert_eq!(record.status, TaskState::ApprovalRequired);
        assert_eq!(record.approval_action.as_deref(), Some("commit to main"));

        let record = ledger
            .approve(&record.task_id, true, Some("looks right"))
            .expect("approve yes");
        assert_eq!(record.status, TaskState::Working);
        assert_eq!(record.approval_action, None);

        let record = ledger
            .request_approval(&record.task_id, "push to origin")
            .expect("request_approval 2");
        let record = ledger
            .approve(&record.task_id, false, Some("not yet"))
            .expect("approve no");
        assert_eq!(record.status, TaskState::Cancelled);
    }

    #[test]
    fn stale_approval_generation_cannot_resolve_later_gate() {
        let ledger = test_ledger();
        let record = ledger.submit(submit_request("conv-1")).expect("submit");
        let record = run_to_working(&ledger, &record.task_id);
        let first = ledger
            .request_approval(&record.task_id, "first action")
            .unwrap();
        let old_generation = first.approval_generation.clone().unwrap();
        ledger
            .approve_generation(&record.task_id, &old_generation, true, None)
            .unwrap();
        let second = ledger
            .request_approval(&record.task_id, "second action")
            .unwrap();
        assert_ne!(
            second.approval_generation.as_deref(),
            Some(old_generation.as_str())
        );
        assert!(ledger
            .approve_generation(&record.task_id, &old_generation, true, None)
            .is_err());
        assert_eq!(
            ledger.inspect(&record.task_id).unwrap().unwrap().status,
            TaskState::ApprovalRequired
        );
    }

    #[test]
    fn stale_task_generation_cannot_cancel_a_later_interaction() {
        let ledger = test_ledger();
        let record = ledger.submit(submit_request("conv-1")).unwrap();
        run_to_working(&ledger, &record.task_id);
        let first = ledger.request_input(&record.task_id, "first?", "resume-first").unwrap();
        let first_generation = first.input_generation.clone().unwrap();
        ledger.respond_generation(&record.task_id, &first_generation, "answer").unwrap();
        let second = ledger.request_input(&record.task_id, "second?", "resume-second").unwrap();
        let second_generation = second.input_generation.clone().unwrap();

        assert!(ledger.cancel_generation(&record.task_id, &first_generation, "cancelled").is_err());
        assert_eq!(ledger.inspect(&record.task_id).unwrap().unwrap().status, TaskState::InputRequired);
        assert_eq!(ledger.cancel_generation(&record.task_id, &second_generation, "cancelled").unwrap().status, TaskState::Cancelled);
    }

    #[test]
    fn correlation_id_makes_submission_idempotent_and_rejects_reuse() {
        let ledger = test_ledger();
        let mut request = submit_request("conv-1");
        request.correlation_id = Some("corr-1".into());
        let first = ledger.submit(request.clone()).unwrap();
        let retry = ledger.submit(request.clone()).unwrap();
        assert_eq!(retry.task_id, first.task_id);
        assert_eq!(ledger.history(&first.task_id, 10).unwrap().len(), 1);

        request.intent_summary = "different request".into();
        assert!(matches!(
            ledger.submit(request),
            Err(TaskError::Validation(_))
        ));
    }

    #[test]
    fn independent_ledger_handles_serialize_submissions() {
        let base = test_ledger();
        let dir = base.dir.clone();
        let workers = (0..8)
            .map(|i| {
                let dir = dir.clone();
                std::thread::spawn(move || {
                    TaskLedger::with_dir(dir)
                        .submit(submit_request(&format!("conv-{i}")))
                        .unwrap()
                })
            })
            .collect::<Vec<_>>();
        let records = workers
            .into_iter()
            .map(|worker| worker.join().unwrap())
            .collect::<Vec<_>>();
        let history = fs::read_to_string(base.journal_path()).unwrap();
        let events = history
            .lines()
            .map(|line| serde_json::from_str::<TaskEvent>(line).unwrap())
            .collect::<Vec<_>>();
        let mut seqs = events.iter().map(|event| event.seq).collect::<Vec<_>>();
        seqs.sort_unstable();
        assert_eq!(seqs, (0..8).collect::<Vec<_>>());
        assert_eq!(records.len(), 8);
        assert_eq!(base.list(TaskListFilter::default()).unwrap().len(), 8);
    }

    #[test]
    fn invalid_transitions_rejected() {
        let ledger = test_ledger();
        let record = ledger.submit(submit_request("conv-1")).expect("submit");

        // Cannot complete a queued task, cannot begin work before start.
        assert!(matches!(
            ledger.complete(&record.task_id, "x", &[], &[]).unwrap_err(),
            TaskError::InvalidTransition { .. }
        ));
        assert!(matches!(
            ledger.begin_work(&record.task_id).unwrap_err(),
            TaskError::InvalidTransition { .. }
        ));
        // Cannot approve outside the approval gate.
        assert!(matches!(
            ledger.approve(&record.task_id, true, None).unwrap_err(),
            TaskError::InvalidTransition { .. }
        ));
    }

    #[test]
    fn fail_records_reason_from_non_terminal_states() {
        let ledger = test_ledger();
        let record = ledger.submit(submit_request("conv-1")).expect("submit");

        // Failing straight from queued (e.g. budget pre-check).
        let record = ledger
            .fail(&record.task_id, "budget_exhausted")
            .expect("fail");
        assert_eq!(record.status, TaskState::Failed);
        assert_eq!(record.failure_reason.as_deref(), Some("budget_exhausted"));

        let record = ledger.submit(submit_request("conv-1")).expect("submit");
        let record = run_to_working(&ledger, &record.task_id);
        let record = ledger
            .fail(&record.task_id, "deadline_exceeded")
            .expect("fail");
        assert_eq!(record.status, TaskState::Failed);
        assert_eq!(record.failure_reason.as_deref(), Some("deadline_exceeded"));

        // Terminal failure is immutable.
        assert!(matches!(
            ledger.fail(&record.task_id, "again").unwrap_err(),
            TaskError::Terminal { .. }
        ));
    }

    #[test]
    fn cancel_from_queued_and_working() {
        let ledger = test_ledger();
        let record = ledger.submit(submit_request("conv-1")).expect("submit");
        let record = ledger
            .cancel(&record.task_id, "changed my mind")
            .expect("cancel");
        assert_eq!(record.status, TaskState::Cancelled);

        let record = ledger.submit(submit_request("conv-1")).expect("submit");
        let record = run_to_working(&ledger, &record.task_id);
        let record = ledger
            .cancel(&record.task_id, "no longer needed")
            .expect("cancel");
        assert_eq!(record.status, TaskState::Cancelled);
    }

    #[test]
    fn snapshots_survive_ledger_reload() {
        let dir =
            std::env::temp_dir().join(format!("task-fabric-svn-task-ledger-reload-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let task_id = {
            let ledger = TaskLedger::with_dir(dir.clone());
            let record = ledger.submit(submit_request("conv-9")).expect("submit");
            run_to_working(&ledger, &record.task_id);
            record.task_id.clone()
        };
        // New ledger instance on the same dir: the task is still there.
        let ledger = TaskLedger::with_dir(dir);
        let record = ledger.inspect(&task_id).expect("inspect").expect("exists");
        assert_eq!(record.status, TaskState::Working);
        assert_eq!(record.conversation_id, "conv-9");
        let history = ledger.history(&task_id, 10).expect("history");
        assert!(history.len() >= 3);
    }

    #[test]
    fn list_filters_by_state_and_conversation() {
        let ledger = test_ledger();
        let a = ledger.submit(submit_request("conv-a")).expect("submit a");
        let b = ledger.submit(submit_request("conv-b")).expect("submit b");
        run_to_working(&ledger, &a.task_id);
        ledger
            .complete(&a.task_id, "done", &[], &[])
            .expect("complete a");

        let completed = ledger
            .list(TaskListFilter {
                state: Some(TaskState::Completed),
                ..Default::default()
            })
            .expect("list");
        assert_eq!(completed.len(), 1);
        assert_eq!(completed[0].task_id, a.task_id);

        let conv_b = ledger
            .list(TaskListFilter {
                conversation_id: Some("conv-b".into()),
                ..Default::default()
            })
            .expect("list");
        assert_eq!(conv_b.len(), 1);
        assert_eq!(conv_b[0].task_id, b.task_id);

        let all = ledger.list(TaskListFilter::default()).expect("list");
        assert_eq!(all.len(), 2);
    }

    #[test]
    fn submit_validates_fields() {
        let ledger = test_ledger();
        let mut req = submit_request("conv-1");
        req.intent_summary = String::new();
        assert!(matches!(
            ledger.submit(req).unwrap_err(),
            TaskError::Validation(_)
        ));

        let mut req = submit_request("conv-1");
        req.conversation_id = "has/slash".into();
        assert!(matches!(
            ledger.submit(req).unwrap_err(),
            TaskError::Validation(_)
        ));
    }

    #[test]
    fn inspect_unknown_task_returns_none() {
        let ledger = test_ledger();
        assert!(ledger
            .inspect("task-doesnotexist")
            .expect("inspect")
            .is_none());
        assert!(matches!(
            ledger.start("task-doesnotexist", "sess").unwrap_err(),
            TaskError::NotFound(_)
        ));
    }

    #[test]
    fn state_machine_table_matches_contract() {
        use TaskState::*;
        // Contract: queued → starting → working → completed
        assert!(Queued.can_transition_to(Starting));
        assert!(Starting.can_transition_to(Working));
        assert!(Working.can_transition_to(Completed));
        // Contract: working → input_required → working
        assert!(Working.can_transition_to(InputRequired));
        assert!(InputRequired.can_transition_to(Working));
        // Contract: working → approval_required → working | cancelled
        assert!(Working.can_transition_to(ApprovalRequired));
        assert!(ApprovalRequired.can_transition_to(Working));
        assert!(ApprovalRequired.can_transition_to(Cancelled));
        // Terminal states exit nowhere.
        for terminal in [Completed, Failed, Cancelled] {
            for next in [
                Queued,
                Starting,
                Working,
                InputRequired,
                ApprovalRequired,
                Completed,
                Failed,
                Cancelled,
            ] {
                assert!(!terminal.can_transition_to(next), "{terminal} -> {next}");
            }
            assert!(terminal.is_terminal());
        }
        assert!(!Queued.is_terminal());
    }
}
