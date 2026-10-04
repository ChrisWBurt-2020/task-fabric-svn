use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CompanionEventCategory {
    Approval,
    Authentication,
    Blocked,
    Completion,
    Connection,
    Critical,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CompanionEventNeed {
    None,
    Awareness,
    Approval,
    Authentication,
    Intervention,
    Reconnect,
    UrgentAction,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CompanionAttentionLevel {
    Aware,
    Attention,
    Interrupt,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CompanionActionKind {
    None,
    ReviewConsent,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CompanionEventV1 {
    pub schema_version: u32,
    pub event_id: String,
    pub created_at: u64,
    pub category: CompanionEventCategory,
    pub need: CompanionEventNeed,
    pub attention_level: CompanionAttentionLevel,
    pub title: String,
    pub safe_context: String,
    pub speech_phrase: String,
    pub expires_at: Option<u64>,
    pub action_kind: CompanionActionKind,
    pub action_ref: Option<String>,
}

impl CompanionEventV1 {
    pub const SCHEMA_VERSION: u32 = 1;

    pub fn validate(&self) -> Result<(), &'static str> {
        if self.schema_version != Self::SCHEMA_VERSION {
            return Err("unsupported schema");
        }
        for (value, max, required) in [
            (&self.event_id, 128, true),
            (&self.title, 96, true),
            (&self.safe_context, 256, false),
            (&self.speech_phrase, 160, false),
        ] {
            if required && value.is_empty()
                || value.len() > max
                || value.bytes().any(|b| b.is_ascii_control())
            {
                return Err("bounded text violation");
            }
        }
        if let Some(reference) = &self.action_ref {
            if reference.is_empty()
                || reference.len() > 128
                || reference.bytes().any(|b| b.is_ascii_control())
            {
                return Err("action reference violation");
            }
        }
        if self
            .expires_at
            .is_some_and(|expiry| expiry < self.created_at)
        {
            return Err("expiry precedes creation");
        }
        match (&self.category, &self.action_kind, &self.action_ref) {
            (CompanionEventCategory::Approval, CompanionActionKind::ReviewConsent, Some(_)) => {
                Ok(())
            }
            (CompanionEventCategory::Approval, _, _) => Err("approval action invariant"),
            (_, CompanionActionKind::None, None) => Ok(()),
            _ => Err("non-approval action invariant"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event() -> CompanionEventV1 {
        CompanionEventV1 {
            schema_version: 1,
            event_id: "event-1".into(),
            created_at: 10,
            category: CompanionEventCategory::Completion,
            need: CompanionEventNeed::None,
            attention_level: CompanionAttentionLevel::Aware,
            title: "Done".into(),
            safe_context: "A safe update".into(),
            speech_phrase: "Task Fabric SVN finished.".into(),
            expires_at: None,
            action_kind: CompanionActionKind::None,
            action_ref: None,
        }
    }

    #[test]
    fn event_fixture_is_stable_and_bounded() {
        let value = serde_json::to_value(event()).expect("serialize event");
        assert_eq!(value["schema_version"], 1);
        assert!(event().validate().is_ok());
        let mut invalid = event();
        invalid.title = "x".repeat(97);
        assert!(invalid.validate().is_err());
    }

    #[test]
    fn approval_requires_review_reference() {
        let mut invalid = event();
        invalid.category = CompanionEventCategory::Approval;
        assert!(invalid.validate().is_err());
    }
}
