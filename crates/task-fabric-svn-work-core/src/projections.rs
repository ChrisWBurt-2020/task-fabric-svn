//! Durable task projections: typed, reconnect-safe answers to "the task".
//!
//! Built from the snapshot only — no transcript replay, no journal scan.
//! A restarted client resolves the same projections straight from disk.

use crate::task_ledger::{
    validate_id, ApprovalChannel, TaskError, TaskLedger, TaskListFilter, TaskRecord, TaskState,
};
use serde::{Deserialize, Serialize};

/// Pending worker question, as presented to the user.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PendingInputSummary {
    pub generation: String,
    pub question: String,
}

/// Pending approval gate, as presented to the user.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PendingApprovalSummary {
    pub envelope_id: String,
    pub generation: String,
    pub action: String,
    pub requested_at: u64,
    pub channel: ApprovalChannel,
}

/// Reconnect-safe view of a task. Everything here comes from the durable
/// snapshot; nothing requires the journal or any transcript.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TaskProjection {
    pub task_id: String,
    pub conversation_id: String,
    pub state: TaskState,
    pub intent_summary: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_repo_identity: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_revision: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_input: Option<PendingInputSummary>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_approval: Option<PendingApprovalSummary>,
    #[serde(default)]
    pub receipt_refs: Vec<String>,
    pub updated_at: u64,
}

impl TaskProjection {
    pub fn from_record(record: &TaskRecord) -> Self {
        let pending_input = if record.status == TaskState::InputRequired {
            match (&record.input_generation, &record.input_question) {
                (Some(generation), Some(question)) => Some(PendingInputSummary {
                    generation: generation.clone(),
                    question: question.clone(),
                }),
                _ => None,
            }
        } else {
            None
        };
        let pending_approval = if record.status == TaskState::ApprovalRequired {
            record.pending_envelope.as_ref().map(|env| PendingApprovalSummary {
                envelope_id: env.envelope_id.clone(),
                generation: env.generation.clone(),
                action: env.action.clone(),
                requested_at: env.requested_at,
                channel: env.channel,
            })
        } else {
            None
        };
        TaskProjection {
            task_id: record.task_id.clone(),
            conversation_id: record.conversation_id.clone(),
            state: record.status,
            intent_summary: record.intent_summary.clone(),
            target_repo_identity: record.target_repo_identity.clone(),
            target_revision: record.target_revision.clone(),
            harness: record.harness.clone(),
            pending_input,
            pending_approval,
            receipt_refs: record.receipt_refs.clone(),
            updated_at: record.updated_at,
        }
    }
}

impl TaskLedger {
    /// Resolve "the task" after reconnect: snapshot read only.
    pub fn project(&self, task_id: &str) -> Result<Option<TaskProjection>, TaskError> {
        Ok(self
            .inspect(task_id)?
            .map(|record| TaskProjection::from_record(&record)))
    }

    /// Every task in a conversation, newest first: snapshot reads only.
    pub fn project_by_conversation(
        &self,
        conversation_id: &str,
    ) -> Result<Vec<TaskProjection>, TaskError> {
        validate_id("conversation_id", conversation_id)?;
        Ok(self
            .list(TaskListFilter {
                conversation_id: Some(conversation_id.to_string()),
                ..Default::default()
            })?
            .iter()
            .map(TaskProjection::from_record)
            .collect())
    }
}
