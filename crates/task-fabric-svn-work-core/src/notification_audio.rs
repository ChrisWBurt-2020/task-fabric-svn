//! Local notification-audio policy.  This module is deliberately free of
//! process and audio dependencies so the policy can be tested in isolation.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

pub const AUDIO_SCHEMA_VERSION: u32 = 1;

/// The semantic need is intentionally independent from the presentation
/// category.  A warning may require awareness, intervention, or a decision;
/// the renderer should not have to infer that from a sound name.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum Need {
    #[default]
    None,
    Awareness,
    Review,
    Decision,
    Approval,
    Authentication,
    Intervention,
    Retry,
    Reconnect,
    Investigate,
    UrgentAction,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, Default)]
#[serde(rename_all = "snake_case")]
pub enum AttentionLevel {
    #[default]
    Ambient,
    Aware,
    Attention,
    Interrupt,
}

// ------------------------------------------------------------ phrase book
//
// Optional user overrides that replace the built-in spoken phrases. Each
// category can carry any number of phrases; the winning one is chosen
// deterministically from the notification id so the same notification always
// says the same thing. `[by_app]` entries rank above the category defaults so
// an app can have a voice of its own. Templates `{app}` and `{summary}` are
// substituted before the phrase is bounded to the word cap.

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "snake_case")]
pub struct PhraseBook {
    pub approval: Vec<String>,
    pub blocked: Vec<String>,
    pub warning: Vec<String>,
    pub critical: Vec<String>,
    pub success: Vec<String>,
    pub routine: Vec<String>,
    pub message: Vec<String>,
    pub by_app: HashMap<String, AppPhrases>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "snake_case")]
pub struct AppPhrases {
    pub label: Option<String>,
    pub approval: Vec<String>,
    pub blocked: Vec<String>,
    pub warning: Vec<String>,
    pub critical: Vec<String>,
    pub success: Vec<String>,
    pub routine: Vec<String>,
    pub message: Vec<String>,
}

impl PhraseBook {
    fn app_phrases(&self, app: &str) -> &AppPhrases {
        self.by_app
            .get(&app.to_ascii_lowercase())
            .unwrap_or(&EMPTY_APP_PHRASES)
    }

    fn category_phrases<'a>(app: &'a AppPhrases, category: &str) -> &'a [String] {
        match category {
            "approval" => &app.approval,
            "blocked" => &app.blocked,
            "warning" => &app.warning,
            "critical" => &app.critical,
            "success" => &app.success,
            "routine" => &app.routine,
            "message" => &app.message,
            _ => &app.routine,
        }
    }

    fn global_phrases(&self, category: &str) -> &[String] {
        match category {
            "approval" => &self.approval,
            "blocked" => &self.blocked,
            "warning" => &self.warning,
            "critical" => &self.critical,
            "success" => &self.success,
            "routine" => &self.routine,
            "message" => &self.message,
            _ => &self.routine,
        }
    }

    /// The most specific configured phrase for (category, app), or "" when no
    /// override is set. App-scoped entries win over category-scoped entries.
    pub fn override_for(&self, category: &str, app: &str, id: &str, summary: &str) -> String {
        let app_ps = self.app_phrases(app);
        let from_app = Self::category_phrases(app_ps, category);
        if !from_app.is_empty() {
            return pick_phrase(from_app, id, app, summary);
        }
        let global = Self::global_phrases(self, category);
        if !global.is_empty() {
            return pick_phrase(global, id, app, summary);
        }
        String::new()
    }
}

static EMPTY_APP_PHRASES: AppPhrases = AppPhrases {
    label: None,
    approval: Vec::new(),
    blocked: Vec::new(),
    warning: Vec::new(),
    critical: Vec::new(),
    success: Vec::new(),
    routine: Vec::new(),
    message: Vec::new(),
};

fn pick_phrase(phrases: &[String], id: &str, app: &str, summary: &str) -> String {
    if phrases.is_empty() {
        return String::new();
    }
    let index = (fnv1a(id.as_bytes()) as usize) % phrases.len();
    substitute(&phrases[index], app, summary)
}

#[derive(Debug, Clone, Default)]
pub struct ContextVars<'a> {
    pub repo: Option<&'a str>,
    pub tool: Option<&'a str>,
    pub duration: Option<&'a str>,
    pub time_of_day: Option<&'a str>,
}

fn substitute(template: &str, app: &str, summary: &str) -> String {
    substitute_context(template, app, summary, &ContextVars::default())
}

pub fn substitute_context(
    template: &str,
    app: &str,
    summary: &str,
    vars: &ContextVars,
) -> String {
    let mut out = String::with_capacity(template.len() + 32);
    let mut rest = template;
    while let Some(start) = rest.find('{') {
        out.push_str(&rest[..start]);
        let Some(end) = rest[start..].find('}') else {
            out.push_str(&rest[start..]);
            rest = "";
            break;
        };
        let key = &rest[start + 1..start + end];
        match key {
            "app" => out.push_str(&sanitize(app)),
            "summary" => out.push_str(&sanitize(summary)),
            "repo" | "workspace" => {
                if let Some(r) = vars.repo {
                    out.push_str(&sanitize(r));
                }
            }
            "tool" => {
                if let Some(t) = vars.tool {
                    out.push_str(&sanitize(t));
                }
            }
            "duration" => {
                if let Some(d) = vars.duration {
                    out.push_str(d);
                }
            }
            "time_of_day" => {
                if let Some(tod) = vars.time_of_day {
                    out.push_str(tod);
                }
            }
            _ => out.push_str(&rest[start..=start + end]),
        }
        rest = &rest[start + end + 1..];
    }
    out.push_str(rest);
    // Collapse whitespace left behind by empty substitutions: a missing
    // {tool}/{repo} degrades to nothing instead of the literal words
    // "task"/"the repo".
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

pub fn persona_leadin(app: &str, vars: &ContextVars) -> String {
    let app_lower = app.to_ascii_lowercase();
    let time_phrase = match vars.time_of_day {
        Some("late_night") => "Late tonight",
        Some("morning") => "This morning",
        Some("afternoon") => "This afternoon",
        Some("evening") => "This evening",
        _ => "",
    };

    let repo_phrase = match vars.repo {
        Some(r) if !r.trim().is_empty() => format!("in {}", sanitize(r)),
        _ => String::new(),
    };

    let dur_phrase = match vars.duration {
        Some(d) if !d.trim().is_empty() => format!("in {}", sanitize(d)),
        _ => String::new(),
    };

    let app_name = sanitize(app);

    // Build context prefix combining time and duration
    let prefix = match (!time_phrase.is_empty(), !dur_phrase.is_empty()) {
        (true, true) => format!("{time_phrase} {dur_phrase},"),
        (true, false) => format!("{time_phrase},"),
        (false, true) => format!("{dur_phrase},"),
        (false, false) => String::new(),
    };

    if app_lower.contains("claude") {
        if !prefix.is_empty() && !repo_phrase.is_empty() {
            format!("{prefix} Claude wrapped up {repo_phrase}:")
        } else if !prefix.is_empty() {
            format!("{prefix} Claude wrapped up:")
        } else if !repo_phrase.is_empty() {
            format!("Claude finished {repo_phrase}:")
        } else {
            "Claude finished:".into()
        }
    } else if app_lower.contains("codex") {
        if !prefix.is_empty() && !repo_phrase.is_empty() {
            format!("{prefix} Codex finished {repo_phrase}:")
        } else if !prefix.is_empty() {
            format!("{prefix} Codex finished:")
        } else if !repo_phrase.is_empty() {
            format!("Codex finished {repo_phrase}:")
        } else {
            "Codex finished:".into()
        }
    } else if app_lower.contains("opencode") {
        if !prefix.is_empty() && !repo_phrase.is_empty() {
            format!("{prefix} Opencode done {repo_phrase}:")
        } else if !prefix.is_empty() {
            format!("{prefix} Opencode done:")
        } else if !repo_phrase.is_empty() {
            format!("Opencode done {repo_phrase}:")
        } else {
            "Opencode done:".into()
        }
    } else if app_lower.contains("gemini") {
        if !prefix.is_empty() && !repo_phrase.is_empty() {
            format!("{prefix} Gemini completed work {repo_phrase}:")
        } else if !prefix.is_empty() {
            format!("{prefix} Gemini completed work:")
        } else if !repo_phrase.is_empty() {
            format!("Gemini finished {repo_phrase}:")
        } else {
            "Gemini finished:".into()
        }
    } else if !prefix.is_empty() && !repo_phrase.is_empty() {
        format!("{prefix} {app_name} finished {repo_phrase}:")
    } else if !prefix.is_empty() {
        format!("{prefix} {app_name} finished:")
    } else if !repo_phrase.is_empty() {
        format!("{app_name} finished {repo_phrase}:")
    } else {
        format!("{app_name} finished:")
    }
}


// ------------------------------------------------------------ humor spec
//
// Every humor kind has its own phrase list, split between wins (success) and
// losses (blocked / warning). Haiku mode composes a 5-7-5 line from fixed
// templates rather than free-generation, so it stays deterministic, offline
// and cheap. Approval and critical are never joked about: the apply_humor
// gate in task-fabric-svn-soundd enforces the attention-level whitelist, and these
// lists are never consulted for those categories.

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum HumorKind {
    #[default]
    Off,
    Dry,
    Playful,
    Blunt,
    Noir,
    Haiku,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "snake_case")]
pub struct HumorSpec {
    pub dry_success: Vec<String>,
    pub playful_success: Vec<String>,
    pub blunt_success: Vec<String>,
    pub noir_success: Vec<String>,
    pub dry_failure: Vec<String>,
    pub playful_failure: Vec<String>,
    pub blunt_failure: Vec<String>,
    pub noir_failure: Vec<String>,
    pub haiku: Vec<[String; 3]>,

    // Per-agent personas
    pub codex_success: Vec<String>,
    pub codex_failure: Vec<String>,
    pub claude_success: Vec<String>,
    pub claude_failure: Vec<String>,
    pub opencode_success: Vec<String>,
    pub opencode_failure: Vec<String>,
    pub gemini_success: Vec<String>,
    pub gemini_failure: Vec<String>,

    // Situational triggers
    pub late_night: Vec<String>,
    pub fast_fix: Vec<String>,
    pub marathon: Vec<String>,
    pub recovery: Vec<String>,
}

impl Default for HumorSpec {
    fn default() -> Self {
        Self {
            dry_success: vec![
                "Done. The bits survived.".into(),
                "Finished. Nothing seems haunted.".into(),
                "Complete. Future you says thanks.".into(),
            ],
            playful_success: vec![
                "Task finished. No dragons detected.".into(),
                "Build complete. The bits survived.".into(),
                "Done. Reality appears stable.".into(),
                "Shipped it. Go touch some grass.".into(),
            ],
            blunt_success: vec![
                "Done. The bits survived.".into(),
                "Finished.".into(),
                "Complete.".into(),
            ],
            noir_success: vec![
                "The build went quiet. Too quiet. But it passed.".into(),
                "Another one done. The terminal sleeps tonight.".into(),
                "It is done. Somewhere, a linter cries tears of joy.".into(),
            ],
            dry_failure: vec![
                "Well. That escalated.".into(),
                "It failed. The bits are restless.".into(),
                "The ghosts got their due.".into(),
            ],
            playful_failure: vec![
                "Oops. The build hit a wall.".into(),
                "Something exploded. The bits are shaken.".into(),
                "Well that backfired.".into(),
            ],
            blunt_failure: vec![
                "It failed.".into(),
                "This broke.".into(),
                "Nope.".into(),
            ],
            noir_failure: vec![
                "The build went dark. Something is wrong.".into(),
                "Failure. The terminal weeps.".into(),
            ],
            haiku: vec![
                [
                    "work is done at last".into(),
                    "your approval still awaits".into(),
                    "come check on me soon".into(),
                ],
                [
                    "the build is complete".into(),
                    "the terminal now goes quiet".into(),
                    "your input is due".into(),
                ],
                [
                    "a task reached its end".into(),
                    "the machine is satisfied".into(),
                    "we speak when you call".into(),
                ],
            ],
            codex_success: Vec::new(),
            codex_failure: Vec::new(),
            claude_success: Vec::new(),
            claude_failure: Vec::new(),
            opencode_success: Vec::new(),
            opencode_failure: Vec::new(),
            gemini_success: Vec::new(),
            gemini_failure: Vec::new(),
            late_night: Vec::new(),
            fast_fix: Vec::new(),
            marathon: Vec::new(),
            recovery: Vec::new(),
        }
    }
}

impl HumorSpec {
    fn list_for(&self, kind: HumorKind, is_failure: bool) -> &[String] {
        match (kind, is_failure) {
            (HumorKind::Dry, false) => &self.dry_success,
            (HumorKind::Dry, true) => &self.dry_failure,
            (HumorKind::Blunt, false) => &self.blunt_success,
            (HumorKind::Blunt, true) => &self.blunt_failure,
            (HumorKind::Noir, false) => &self.noir_success,
            (HumorKind::Noir, true) => &self.noir_failure,
            (HumorKind::Playful, false) => &self.playful_success,
            (HumorKind::Playful, true) => &self.playful_failure,
            (HumorKind::Off | HumorKind::Haiku, _) => &self.playful_success,
        }
    }
}

/// The composer returns the full spoken line for a humor event, supporting
/// rich situational variables ({repo}, {tool}, {duration}, {time_of_day}) and
/// per-agent persona overrides.
pub fn compose_humor_rich(
    kind: HumorKind,
    spec: &HumorSpec,
    is_failure: bool,
    id: &str,
    app: &str,
    summary: &str,
    vars: &ContextVars,
    is_recovery: bool,
) -> String {
    let hash = fnv1a(id.as_bytes()) as usize;

    // 1. Recovery celebration if coming back from failure
    if !is_failure && is_recovery && !spec.recovery.is_empty() && (hash % 2 == 0) {
        let idx = hash % spec.recovery.len();
        return substitute_context(&spec.recovery[idx], app, summary, vars);
    }

    // 2. Situational: late night quip
    if vars.time_of_day == Some("late_night") && !spec.late_night.is_empty() && (hash % 3 == 0) {
        let idx = hash % spec.late_night.len();
        return substitute_context(&spec.late_night[idx], app, summary, vars);
    }

    // 3. Situational: fast fix vs marathon
    if let Some(dur_str) = vars.duration {
        if let Ok(secs) = dur_str.trim_end_matches('s').parse::<f64>() {
            if secs < 10.0 && !spec.fast_fix.is_empty() && (hash % 2 == 0) {
                let idx = hash % spec.fast_fix.len();
                return substitute_context(&spec.fast_fix[idx], app, summary, vars);
            } else if secs > 180.0 && !spec.marathon.is_empty() && (hash % 2 == 0) {
                let idx = hash % spec.marathon.len();
                return substitute_context(&spec.marathon[idx], app, summary, vars);
            }
        }
    }

    // 4. Per-agent persona check
    let app_lower = app.to_ascii_lowercase();
    let persona_list = match (app_lower.as_str(), is_failure) {
        ("codex", false) if !spec.codex_success.is_empty() => &spec.codex_success,
        ("codex", true) if !spec.codex_failure.is_empty() => &spec.codex_failure,
        ("claude", false) if !spec.claude_success.is_empty() => &spec.claude_success,
        ("claude", true) if !spec.claude_failure.is_empty() => &spec.claude_failure,
        ("opencode", false) if !spec.opencode_success.is_empty() => &spec.opencode_success,
        ("opencode", true) if !spec.opencode_failure.is_empty() => &spec.opencode_failure,
        ("gemini", false) if !spec.gemini_success.is_empty() => &spec.gemini_success,
        ("gemini", true) if !spec.gemini_failure.is_empty() => &spec.gemini_failure,
        _ => &[][..],
    };

    if !persona_list.is_empty() {
        let idx = hash % persona_list.len();
        return substitute_context(&persona_list[idx], app, summary, vars);
    }

    // 5. Fall back to standard compose_humor
    if kind == HumorKind::Haiku {
        let templates = if spec.haiku.is_empty() {
            &HumorSpec::default().haiku
        } else {
            &spec.haiku
        };
        let lines = &templates[hash % templates.len()];
        let joined = lines
            .iter()
            .map(|line| substitute_context(line, app, summary, vars))
            .collect::<Vec<_>>()
            .join(". ");
        format!("{joined}.")
    } else {
        let phrases = spec.list_for(kind, is_failure);
        if phrases.is_empty() {
            String::new()
        } else {
            let idx = hash % phrases.len();
            substitute_context(&phrases[idx], app, summary, vars)
        }
    }
}

pub fn compose_humor(
    kind: HumorKind,
    spec: &HumorSpec,
    is_failure: bool,
    id: &str,
    app: &str,
    summary: &str,
) -> String {
    compose_humor_rich(
        kind,
        spec,
        is_failure,
        id,
        app,
        summary,
        &ContextVars::default(),
        false,
    )
}


// ---------------------------------------------------- speech enrichment
//
// The classifier emits a terse base phrase from the summary/app. This final
// pass layers the user-configurable phrase book over it, then injects a short
// "what was being requested" token from either the task-fabric-svn-audio-extra hint or
// a sanitized excerpt of the notification body, and bounds the result to the
// word cap.

#[derive(Debug, Clone)]
pub struct SpeechContext<'a> {
    pub id: &'a str,
    pub app: &'a str,
    pub summary: &'a str,
    pub body: &'a str,
    pub extra: &'a str,
    pub workspace: &'a str,
    pub details: &'a str,
    pub book: Option<&'a PhraseBook>,
    pub word_cap: usize,
}

/// A short "what is being requested" phrase, or "" when nothing safe remains.
pub fn body_excerpt(body: &str) -> String {
    let words = sanitize(body);
    words
        .split_whitespace()
        .take(4)
        .collect::<Vec<_>>()
        .join(" ")
}

/// Build the final spoken line for a decision. Empty overrides fall through
/// to the base speech; a non-empty extra hint or body excerpt gives approval
/// and blocked lines concrete context.
pub fn finalize_speech(decision: &AudioDecision, ctx: &SpeechContext) -> String {
    let cap = ctx.word_cap.clamp(1, 32);
    let subject = sanitize(ctx.app);
    let base = decision.speech.trim().to_string();

    if let Some(book) = ctx.book {
        let override_phrase =
            book.override_for(&decision.category, ctx.app, ctx.id, ctx.summary);
        if !override_phrase.is_empty() {
            return bound_phrase(&override_phrase, cap);
        }
    }

    // Approval: deterministic who/what/where composition. Approvals never
    // speak completion language; they name who needs the decision, what is
    // being asked, and where/why.
    if decision.category == "approval" {
        return bound_phrase(&approval_speech(&subject, ctx, &base), cap);
    }

    let extra = sanitize(ctx.extra).trim_end_matches('.').trim().to_string();
    if !extra.is_empty() {
        let is_caution = extra.to_lowercase().starts_with("caution:")
            || extra.to_lowercase().starts_with("caution ");
        let clean_extra = if is_caution {
            let stripped = extra[8..].trim();
            stripped.to_string()
        } else {
            extra.clone()
        };

        let composed = if is_caution {
            if clean_extra.to_lowercase().starts_with(&subject.to_lowercase()) {
                format!("Caution: {clean_extra}.")
            } else if subject.is_empty() {
                format!("Caution: {clean_extra}.")
            } else {
                format!("Caution: {subject} {clean_extra}.")
            }
        } else if clean_extra.to_lowercase().starts_with(&subject.to_lowercase()) {
            format!("{clean_extra}.")
        } else if clean_extra.to_lowercase().starts_with("in ") {
            if subject.is_empty() {
                format!("{clean_extra}.")
            } else if let Some(colon_idx) = clean_extra.find(':') {
                let (prefix, action) = clean_extra.split_at(colon_idx + 1);
                format!("{prefix} {subject} {}.", action.trim().trim_end_matches('.'))
            } else {
                format!("{subject} {clean_extra}.")
            }
        } else if decision.category == "success"
            && !clean_extra.to_lowercase().starts_with("done")
            && !clean_extra.to_lowercase().starts_with("finished")
            && !clean_extra.to_lowercase().starts_with("completed")
        {
            if subject.is_empty() {
                format!("{clean_extra}.")
            } else {
                format!("{subject} finished: {clean_extra}.")
            }
        } else if subject.is_empty() {
            format!("{clean_extra}.")
        } else {
            format!("{subject} {clean_extra}.")
        };
        return bound_phrase(&composed, cap);
    }

    if !base.is_empty() && subject.is_empty() {
        return bound_phrase(&base, cap);
    }

    let excerpt = body_excerpt(ctx.body);
    if !excerpt.is_empty() {
        let composed = match decision.category.as_str() {
            "approval" => format!("{subject} needs your approval for {excerpt}."),
            "blocked" => format!("{subject} is blocked on {excerpt}."),
            "critical" => format!("{subject} needs attention for {excerpt}."),
            _ => base.clone(),
        };
        if !composed.is_empty() {
            return bound_phrase(&composed, cap);
        }
    }

    bound_phrase(&if base.is_empty() { subject } else { base }, cap)
}

/// FNV-1a 64-bit, shared with the sound daemon and the phrase composters.
pub fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf29ce484222325;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

pub fn bound_phrase(value: &str, cap: usize) -> String {
    value
        .split_whitespace()
        .take(cap)
        .collect::<Vec<_>>()
        .join(" ")
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PresentationPlan {
    pub attention_level: AttentionLevel,
    pub need: Need,
    pub earcon: String,
    pub speech: String,
    pub speak: bool,
    pub confidence: f32,
    pub time_sensitivity: f32,
    pub repeat_after_seconds: Option<u64>,
}

impl Default for PresentationPlan {
    fn default() -> Self {
        Self {
            attention_level: AttentionLevel::Ambient,
            need: Need::None,
            earcon: "arrival".into(),
            speech: String::new(),
            speak: false,
            confidence: 0.0,
            time_sensitivity: 0.0,
            repeat_after_seconds: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct NotificationEnvelope {
    pub schema_version: u32,
    pub id: String,
    pub replaces_id: Option<String>,
    pub timestamp_epoch_ms: u64,
    pub app: String,
    pub urgency: String,
    pub summary: String,
    pub body: String,
    pub dnd: bool,
    /// Optional "what is being requested" hint, threaded into the speech line
    /// by finalize_speech. Set via the task-fabric-svn-audio-extra hint.
    pub extra: Option<String>,
    pub workspace: Option<String>,
    pub duration: Option<String>,
    pub details: Option<String>,
    pub time_of_day: Option<String>,
    pub risk: Option<String>,
    pub session: Option<String>,
    pub task_fabric_svn: Option<TaskFabricSvnPresentation>,
    /// Optional profile identity and resolved voice supplied by HomeNode.
    /// Older callers omit these fields and retain the normal cascade.
    pub profile_id: Option<String>,
    pub profile_priority: u32,
    pub resolved_voice: Option<String>,
    pub resolved_voice_source: Option<String>,
    pub speech_queue_policy: Option<String>,
    /// Manual preview from the humor settings page: run the full
    /// classification/humor pipeline and return the decision, but never
    /// speak, consume budget, or write to the decision ledger.
    pub dry_run: bool,
}

impl Default for NotificationEnvelope {
    fn default() -> Self {
        Self {
            schema_version: AUDIO_SCHEMA_VERSION,
            id: String::new(),
            replaces_id: None,
            timestamp_epoch_ms: 0,
            app: String::new(),
            urgency: "normal".into(),
            summary: String::new(),
            body: String::new(),
            dnd: false,
            extra: None,
            workspace: None,
            duration: None,
            details: None,
            time_of_day: None,
            risk: None,
            session: None,
            task_fabric_svn: None,
            profile_id: None,
            profile_priority: 50,
            resolved_voice: None,
            resolved_voice_source: None,
            speech_queue_policy: None,
            dry_run: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct TaskFabricSvnPresentation {
    pub category: Option<String>,
    pub need: Option<String>,
    pub context: Option<String>,
    pub outcome: Option<String>,
}

impl Default for TaskFabricSvnPresentation {
    fn default() -> Self {
        Self {
            category: None,
            need: None,
            context: None,
            outcome: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AudioDecision {
    pub schema_version: u32,
    pub category: String,
    pub inferred_need: String,
    pub safe_context: String,
    pub earcon: String,
    pub speak: bool,
    pub speech: String,
    pub priority: String,
    pub classification_source: String,
    pub suppressed: bool,
    pub need: Need,
    pub attention_level: AttentionLevel,
    pub confidence: f32,
    pub time_sensitivity: f32,
    pub presentation: PresentationPlan,
    pub suppression_reason: Option<String>,
}

impl AudioDecision {
    fn new(category: &str, need: &str, context: &str, earcon: &str, priority: &str) -> Self {
        Self {
            schema_version: AUDIO_SCHEMA_VERSION,
            category: category.into(),
            inferred_need: need.into(),
            safe_context: context.into(),
            earcon: earcon.into(),
            speak: false,
            speech: String::new(),
            priority: priority.into(),
            classification_source: "rule".into(),
            suppressed: false,
            need: need_for_category(category),
            attention_level: level_for_category(category),
            confidence: 1.0,
            time_sensitivity: if priority == "critical" {
                1.0
            } else if priority == "high" {
                0.7
            } else {
                0.2
            },
            presentation: PresentationPlan::default(),
            suppression_reason: None,
        }
    }

    pub fn presentation_plan(&self) -> PresentationPlan {
        PresentationPlan {
            attention_level: self.attention_level,
            need: self.need.clone(),
            earcon: self.earcon.clone(),
            speech: self.speech.clone(),
            speak: self.speak,
            confidence: self.confidence,
            time_sensitivity: self.time_sensitivity,
            repeat_after_seconds: if self.attention_level >= AttentionLevel::Attention {
                Some(900)
            } else {
                None
            },
        }
    }
}

pub fn classify(notification: &NotificationEnvelope) -> AudioDecision {
    let app = normalize(&notification.app);
    let display_app = sanitize(&notification.app);
    let summary = sanitize(&notification.summary);
    let body = sanitize(&notification.body);
    let text = format!("{} {}", summary.to_lowercase(), body.to_lowercase());
    let local_critical = is_local_critical_app(&app);
    let is_caution = notification.risk.as_deref() == Some("high")
        || notification.summary.to_lowercase().contains("caution")
        || notification.extra.as_deref().unwrap_or("").to_lowercase().contains("caution");
    let mut decision = if let Some(task_fabric_svn) = &notification.task_fabric_svn {
        classify_task_fabric_svn(task_fabric_svn, &display_app, &summary, &text, is_caution)
    } else if is_authentication_prompt(&app, &text) {
        let mut d = AudioDecision::new(
            "approval",
            "authentication",
            &safe_context(&display_app, &summary),
            "approval",
            "high",
        );
        d.speak = true;
        d.speech = if contains_any(&app, &["polkit", "pkexec", "policykit"]) {
            "System authentication required.".into()
        } else {
            phrase("authentication", &display_app, &summary)
        };
        d
    } else if is_ai_chat_app(&app) && is_ai_completion(&text) {
        let mut d = AudioDecision::new(
            "approval",
            "your approval",
            &safe_context(&display_app, &summary),
            "approval",
            "high",
        );
        d.speak = true;
        d.speech = ai_completion_phrase(&app);
        d
    } else if contains_any(
        &text,
        &[
            "approval",
            "approve",
            "authentication",
            "authorize",
            "authorization",
            "permission",
            "password",
            "waiting for you",
        ],
    ) && is_ai_chat_app(&app)
    {
        let mut d = AudioDecision::new(
            "approval",
            "your approval",
            &safe_context(&display_app, &summary),
            "approval",
            "high",
        );
        d.speak = true;
        d.speech = phrase("needs your approval", &display_app, &summary);
        d
    } else if contains_any(&text, &["battery", "low power", "power level"]) {
        let mut d = AudioDecision::new(
            "warning",
            "attention to power",
            &safe_context(&display_app, &summary),
            "warning",
            "high",
        );
        if notification.urgency == "critical" && local_critical {
            d.category = "critical".into();
            d.earcon = "critical".into();
            d.priority = "critical".into();
            d.speak = true;
            d.speech = phrase("needs immediate attention", &display_app, &summary);
        }
        d
    } else if contains_completion_word(&text)
        || contains_any(&text, &["success", "saved", "ready"])
    {
        AudioDecision::new(
            "success",
            "nothing; work completed",
            &safe_context(&display_app, &summary),
            "complete",
            "low",
        )
    } else if contains_any(
        &text,
        &[
            "failed",
            "failure",
            "error",
            "unavailable",
            "timed out",
            "timeout",
            "degraded",
            "offline",
        ],
    ) {
        let mut d = AudioDecision::new(
            "blocked",
            "attention to a problem",
            &safe_context(&display_app, &summary),
            "blocked",
            "high",
        );
        if notification.urgency == "critical" && local_critical {
            d.category = "critical".into();
            d.earcon = "critical".into();
            d.priority = "critical".into();
            d.speak = true;
            d.speech = phrase("needs attention", &display_app, &summary);
        }
        d
    } else if is_message_app(&app) {
        AudioDecision::new(
            "message",
            "a message arrived",
            &safe_context(&display_app, &summary),
            "message",
            "normal",
        )
    } else {
        let mut d = AudioDecision::new(
            "routine",
            "notification arrived",
            &safe_context(&display_app, &summary),
            "arrival",
            "low",
        );
        d.classification_source = "model".into();
        d.confidence = 0.35;
        d
    };

    if decision.category == "approval" && is_caution {
        decision.earcon = "critical".into();
        decision.inferred_need = "caution approval".into();
    }

    if notification.dnd && !(local_critical && decision.priority == "critical") {
        decision.suppressed = true;
        decision.speak = false;
        decision.suppression_reason = Some("dnd".into());
    }
    refresh_presentation(&mut decision);
    decision
}

/// Recompute the semantic and presentation fields after a classifier (for
/// example the bounded local model) changes the category or inferred need.
pub fn refresh_presentation(decision: &mut AudioDecision) {
    decision.need = if decision.category == "success" {
        Need::None
    } else {
        need_for_category(&decision.category)
    };
    let need_text = decision.inferred_need.to_lowercase();
    if need_text.contains("auth") {
        decision.need = Need::Authentication;
    } else if need_text.contains("reconnect") || need_text.contains("connection") {
        decision.need = Need::Reconnect;
    } else if need_text.contains("retry") {
        decision.need = Need::Retry;
    }
    decision.attention_level = level_for_category(&decision.category);
    decision.presentation = decision.presentation_plan();
}

fn need_for_category(category: &str) -> Need {
    match category {
        "approval" => Need::Approval,
        "blocked" => Need::Intervention,
        "warning" => Need::Awareness,
        "critical" => Need::UrgentAction,
        "message" => Need::Awareness,
        "success" | "routine" => Need::None,
        _ => Need::Awareness,
    }
}

fn level_for_category(category: &str) -> AttentionLevel {
    match category {
        "critical" => AttentionLevel::Interrupt,
        "approval" | "blocked" | "warning" => AttentionLevel::Attention,
        "message" | "success" => AttentionLevel::Aware,
        _ => AttentionLevel::Ambient,
    }
}

fn classify_task_fabric_svn(
    task_fabric_svn: &TaskFabricSvnPresentation,
    app: &str,
    summary: &str,
    text: &str,
    is_caution: bool,
) -> AudioDecision {
    let category = task_fabric_svn.category.as_deref().unwrap_or("").to_lowercase();
    let outcome = task_fabric_svn.outcome.as_deref().unwrap_or("").to_lowercase();
    let context = task_fabric_svn
        .context
        .as_deref()
        .map(sanitize)
        .unwrap_or_else(|| safe_context(app, summary));
    if category == "critical" {
        let mut d = AudioDecision::new(
            "critical",
            task_fabric_svn.need.as_deref().unwrap_or("urgent attention"),
            &context,
            "critical",
            "critical",
        );
        d.speak = true;
        d.speech = phrase(
            task_fabric_svn.need.as_deref().unwrap_or("needs immediate attention"),
            app,
            &context,
        );
        return d;
    }
    if category == "approval"
        || contains_any(text, &["approval", "authorize"])
    {
        let earcon = if is_caution { "critical" } else { "approval" };
        let mut d = AudioDecision::new(
            "approval",
            task_fabric_svn.need.as_deref().unwrap_or(if is_caution { "caution approval" } else { "your approval" }),
            &context,
            earcon,
            "high",
        );
        d.speak = true;
        d.speech = phrase(
            task_fabric_svn.need.as_deref().unwrap_or("needs your approval"),
            app,
            &context,
        );
        return d;
    }
    if outcome == "completed" || category == "success" {
        return AudioDecision::new(
            "success",
            task_fabric_svn.need.as_deref().unwrap_or("nothing; work completed"),
            &context,
            "complete",
            "low",
        );
    }
    if outcome == "failed"
        || outcome == "degraded"
        || category == "blocked"
        || category == "warning"
    {
        let mut d = AudioDecision::new(
            "blocked",
            task_fabric_svn
                .need
                .as_deref()
                .unwrap_or("attention to a Task Fabric SVN problem"),
            &context,
            "blocked",
            "high",
        );
        d.speak = true;
        d.speech = phrase(
            task_fabric_svn.need.as_deref().unwrap_or("needs attention"),
            app,
            &context,
        );
        return d;
    }
    AudioDecision::new(
        "routine",
        task_fabric_svn.need.as_deref().unwrap_or("a Task Fabric SVN update arrived"),
        &context,
        "arrival",
        "low",
    )
}

/// Render a filesystem path as speakable words: home-relative, last two
/// components, dotfiles spoken as words ("/home/user/.config/task_fabric_svn" ->
/// "dot-config task fabric svn"). Mirrors the Omarchy QML audioSanitize behavior so a
/// bridge-provided path survives the last hop instead of becoming "[path]".
fn speakable_path(raw: &str) -> String {
    let trimmed = raw
        .trim()
        .trim_matches(|c: char| matches!(c, '"' | '\'' | '(' | ')' | '[' | ']' | ',' | '.' | ';' | ':'))
        .trim_end_matches('/');
    if trimmed.is_empty() {
        return "the path".into();
    }
    // "/home/<user>/..." -> "~/..."
    let p: String = match trimmed.strip_prefix("/home/") {
        Some(rest) => match rest.find('/') {
            Some(idx) => format!("~/{}", &rest[idx + 1..]),
            None => "~".to_string(),
        },
        None => trimmed.to_string(),
    };
    let comps: Vec<&str> = p
        .split('/')
        .filter(|c| !c.is_empty() && *c != "~")
        .collect();
    let tail_start = comps.len().saturating_sub(2);
    let words: Vec<String> = comps[tail_start..]
        .iter()
        .map(|c| {
            let c = match c.strip_prefix('.') {
                Some(stripped) => format!("dot-{stripped}"),
                None => c.to_string(),
            };
            c.replace('_', " ")
        })
        .collect();
    let spoken: String = words.join(" ").split_whitespace().collect::<Vec<_>>().join(" ");
    if spoken.is_empty() {
        "the path".into()
    } else {
        spoken
    }
}

/// Replace path-shaped tokens with their speakable rendering so approval
/// speech keeps the location instead of dropping it (sanitize strips paths).
fn render_paths_speakable(text: &str) -> String {
    text.split_whitespace()
        .map(|tok| {
            let lead_trimmed =
                tok.trim_start_matches(|c: char| matches!(c, '"' | '\'' | '(' | '['));
            if lead_trimmed.starts_with('/') || lead_trimmed.starts_with("~/") {
                let end = lead_trimmed
                    .find(|c: char| matches!(c, '"' | '\'' | ')' | ']' | ',' | ';' | ':'))
                    .unwrap_or(lead_trimmed.len());
                let (path_part, rest_part) = lead_trimmed.split_at(end);
                let lead_len = tok.len() - lead_trimmed.len();
                format!(
                    "{}{}{}",
                    &tok[..lead_len],
                    speakable_path(path_part),
                    rest_part
                )
            } else {
                tok.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Approval-text sanitizer: keeps speakable paths, drops secret-like
/// assignments and URLs. The shared sanitize() strips path tokens, which
/// would delete the very location an approval is about.
fn sanitize_approval_text(text: &str) -> String {
    render_paths_speakable(text)
        .split_whitespace()
        .filter_map(|raw| {
            let mut token = raw
                .trim_matches(|c: char| c.is_ascii_punctuation() && c != '-' && c != '_')
                .to_string();
            // Safety net: any path token the renderer missed still becomes
            // speakable instead of being spoken raw.
            if token.starts_with('/') || token.starts_with("~/") {
                token = speakable_path(&token);
            }
            if token.is_empty() || token.contains("://") || token.len() > 96 {
                return None;
            }
            let lower = token.to_lowercase();
            // Strip leading dashes for the key check so "--token=x" is caught.
            let key_check = lower.trim_start_matches(|c| c == '-' || c == '_');
            if [
                "token",
                "password",
                "passwd",
                "secret",
                "apikey",
                "api_key",
                "authorization",
            ]
            .iter()
            .any(|key| key_check.starts_with(&format!("{key}=")))
            {
                return None;
            }
            Some(token)
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Strip a leading "Caution:"/"Caution " marker. Risk travels via the earcon;
/// the spoken approval line stays in the deterministic "needs your approval"
/// shape.
fn strip_caution_prefix(text: &str) -> &str {
    let lower = text.to_lowercase();
    if lower.starts_with("caution:") {
        text["caution:".len()..].trim_start()
    } else if lower.starts_with("caution ") {
        text["caution ".len()..].trim_start()
    } else {
        text
    }
}

/// Build the "what" clause from the extra hint: "{subject} wants to X" or
/// "wants to X" becomes "to X". Returns None when nothing safe remains.
fn approval_intent_clause(extra: &str, subject: &str) -> Option<String> {
    let text = strip_caution_prefix(extra.trim()).trim();
    let text = text.trim_end_matches('.').trim();
    if text.is_empty() {
        return None;
    }
    let lower = text.to_lowercase();
    let subj_prefix = format!("{} wants to ", subject.to_lowercase());
    let rest = if !subject.is_empty() && lower.starts_with(&subj_prefix) {
        text[subj_prefix.len()..].trim()
    } else if lower.starts_with("wants to ") {
        text["wants to ".len()..].trim()
    } else {
        text
    };
    if rest.is_empty() {
        return None;
    }
    Some(format!("to {rest}"))
}

/// Build the "what" clause from a body fallback: "Please approve X" becomes
/// "for X".
fn approval_body_clause(body: &str) -> Option<String> {
    let text = strip_caution_prefix(body.trim()).trim();
    let text = text.trim_end_matches('.').trim();
    if text.is_empty() {
        return None;
    }
    let lower = text.to_lowercase();
    let rest = if lower.starts_with("please approve ") {
        text["please approve ".len()..].trim()
    } else if lower.starts_with("approve ") {
        text["approve ".len()..].trim()
    } else {
        text
    };
    if rest.is_empty() {
        return None;
    }
    Some(format!("for {rest}"))
}

/// Who/what/where approval speech. Deterministic; never completion language
/// ("done", "finished"): it names who needs the decision, what is being
/// asked, and where/why. `base` is the classifier's speech, kept for genuine
/// completion-awaiting-approval events that carry no intent of their own.
fn approval_speech(subject: &str, ctx: &SpeechContext, base: &str) -> String {
    let who = if subject.is_empty() {
        "Approval needed".to_string()
    } else {
        format!("{} needs your approval", capitalize(subject))
    };

    let extra = sanitize_approval_text(ctx.extra);
    let mut what = approval_intent_clause(&extra, subject);
    if what.is_none() {
        let body_text = sanitize_approval_text(ctx.body);
        let excerpt = body_text
            .split_whitespace()
            .take(12)
            .collect::<Vec<_>>()
            .join(" ");
        what = approval_body_clause(&excerpt);
    }
    let Some(mut what) = what else {
        if !base.is_empty() {
            return base.to_string();
        }
        return who;
    };

    // Where + why: fold the workspace into the intent when it already names
    // the location; otherwise append it. Never repeat the workspace.
    let ws = sanitize(ctx.workspace);
    let ws = ws.trim();
    let mut tail: Vec<String> = Vec::new();
    if !ws.is_empty() && !what.to_lowercase().contains(&ws.to_lowercase()) {
        if what.to_lowercase().contains("outside the workspace") {
            what = what.replace(
                " outside the workspace",
                &format!(", outside the {ws} workspace"),
            );
        } else {
            tail.push(format!("in the {ws} workspace"));
        }
    }

    // Details: the why, when they add non-machine, non-duplicate information.
    // JSON-shaped details are machine data; the intent already carries their
    // human meaning.
    let raw_details = ctx.details.trim();
    if !raw_details.contains('{') && !raw_details.contains('}') {
        let details = sanitize_approval_text(ctx.details);
        let det = details.trim().trim_end_matches('.').trim();
        if !det.is_empty() && det.split_whitespace().count() <= 24 {
            let det_low = det.to_lowercase();
            let what_low = what.to_lowercase();
            if !what_low.contains(&det_low) && !det_low.contains(&what_low) {
                tail.push(det.to_string());
            }
        }
    }

    let mut speech = format!("{who} {what}");
    if !tail.is_empty() {
        speech.push_str(&format!(", {}", tail.join(", ")));
    }
    speech.push('.');
    speech
}

pub fn sanitize(value: &str) -> String {
    sanitize_n(value, 12)
}

/// Same as [`sanitize`] but keeps up to `max_tokens` for richer, longer
/// passages (e.g. reading an agent's final message aloud) while still
/// stripping paths, URLs, and secret-like assignments.
pub fn sanitize_n(value: &str, max_tokens: usize) -> String {
    value
        .split_whitespace()
        .filter_map(|raw| {
            let raw = raw.trim();
            let protected_path = raw.starts_with('/') || raw.starts_with("~/");
            let token =
                raw.trim_matches(|c: char| c.is_ascii_punctuation() && c != '-' && c != '_');
            if token.is_empty()
                || token.contains("://")
                || token.starts_with('/')
                || token.starts_with("~/")
                || token.len() > 96
            {
                return None;
            }
            if protected_path {
                return None;
            }
            let lower = token.to_lowercase();
            if [
                "token",
                "password",
                "passwd",
                "secret",
                "apikey",
                "api_key",
                "authorization",
            ]
            .iter()
            .any(|key| lower.starts_with(&format!("{key}=")))
            {
                return None;
            }
            Some(token.to_string())
        })
        .take(max_tokens)
        .collect::<Vec<_>>()
        .join(" ")
}

fn normalize(value: &str) -> String {
    value.trim().to_lowercase()
}
fn safe_context(app: &str, summary: &str) -> String {
    let app = sanitize(app);
    let summary = sanitize(summary);
    if app.is_empty() {
        summary
    } else if summary.is_empty() {
        app
    } else {
        format!("{app}: {summary}")
    }
}
pub fn compile_phrase(need: &str, app: &str, context: &str) -> String {
    let n = sanitize(need);
    let c = sanitize(context);
    let subject = sanitize(app);
    let lower = n.to_lowercase();
    let phrase = if lower.contains("approval") || lower.contains("approve") {
        format!("{subject} needs approval.")
    } else if lower.contains("authentication") || lower.contains("authenticate") {
        format!("{subject} needs authentication.")
    } else if lower.contains("reconnect") || lower.contains("connection") {
        format!("{subject} needs reconnection.")
    } else if lower.contains("retry") {
        format!("{subject} needs a retry.")
    } else if lower.contains("attention") || lower.contains("intervention") {
        if lower.contains("needs") {
            format!("{}.", capitalize(&n.trim_start_matches("the ")))
        } else {
            format!("{subject} needs attention.")
        }
    } else if c.is_empty() {
        format!("{}.", capitalize(&n))
    } else {
        format!("{subject}: {}.", capitalize(&n))
    };
    bound_phrase(&phrase, 12)
}

fn phrase(need: &str, app: &str, context: &str) -> String {
    compile_phrase(need, app, context)
}
fn capitalize(value: &str) -> String {
    let mut chars = value.chars();
    chars
        .next()
        .map(|c| c.to_uppercase().collect::<String>() + chars.as_str())
        .unwrap_or_default()
}
fn contains_any(text: &str, needles: &[&str]) -> bool {
    needles.iter().any(|needle| text.contains(needle))
}
fn contains_completion_word(text: &str) -> bool {
    text.split(|character: char| !character.is_ascii_alphanumeric())
        .any(|word| matches!(word, "complete" | "completed" | "completion" | "finished"))
}
fn is_ai_chat_app(app: &str) -> bool {
    contains_any(
        app,
        &[
            "codex",
            "chatgpt",
            "herdr",
            "claude",
            "cursor",
            "gemini",
            "copilot",
            "aider",
            "cline",
            "continue",
            "windsurf",
            "goose",
            "opencode",
            "claude-code",
            "gemini-cli",
            "amazon-q",
            "qwen",
            "openai",
            "coding agent",
            "terminal agent",
        ],
    )
}
fn is_ai_completion(text: &str) -> bool {
    contains_completion_word(text)
        || contains_any(
            text,
            &[
                "task done",
                "work done",
                "session stopped",
                "run stopped",
                "awaiting your approval",
                "awaiting approval",
                "waiting for your approval",
                "waiting for approval",
            ],
        )
}
fn ai_completion_phrase(app: &str) -> String {
    let normalized = app.to_ascii_lowercase();
    let subject = if normalized.contains("codex") {
        "Codex"
    } else if normalized.contains("herdr") {
        "Herdr"
    } else if normalized.contains("chatgpt") {
        "ChatGPT"
    } else if normalized.contains("claude") {
        "Claude"
    } else if normalized.contains("cursor") {
        "Cursor"
    } else if normalized.contains("gemini") {
        "Gemini"
    } else if normalized.contains("copilot") {
        "Copilot"
    } else if normalized.contains("aider") {
        "Aider"
    } else if normalized.contains("cline") {
        "Cline"
    } else if normalized.contains("continue") {
        "Continue"
    } else if normalized.contains("windsurf") {
        "Windsurf"
    } else if normalized.contains("goose") {
        "Goose"
    } else {
        return format!("{} has completed and is awaiting your approval.", capitalize(&sanitize(app)));
    };
    format!("{subject} has completed and is awaiting your approval.")
}
fn is_authentication_prompt(app: &str, text: &str) -> bool {
    contains_any(
        app,
        &[
            "polkit",
            "pkexec",
            "policykit",
            "authentication",
            "auth agent",
        ],
    ) || contains_any(
        text,
        &[
            "authentication is required",
            "authentication required",
            "enter your password",
            "administrator password",
            "password required",
            "authorize this action",
        ],
    )
}
fn is_message_app(app: &str) -> bool {
    contains_any(
        app,
        &[
            "discord",
            "slack",
            "signal",
            "telegram",
            "matrix",
            "vesktop",
            "mail",
            "thunderbird",
        ],
    )
}
fn is_local_critical_app(app: &str) -> bool {
    contains_any(
        app,
        &[
            "notify-send",
            "omarchy",
            "task fabric svn",
            "systemd",
            "battery",
            "upower",
        ],
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    fn n(app: &str, summary: &str, body: &str) -> NotificationEnvelope {
        NotificationEnvelope {
            app: app.into(),
            summary: summary.into(),
            body: body.into(),
            ..Default::default()
        }
    }

    #[test]
    fn codex_approval_speaks_need_not_body() {
        let d = classify(&n(
            "Codex",
            "Approval required",
            "Please approve /home/user/private token=abc",
        ));
        assert_eq!(d.category, "approval");
        assert_eq!(d.need, Need::Approval);
        assert_eq!(d.attention_level, AttentionLevel::Attention);
        assert!(d.speak);
        assert!(!d.speech.contains("private"));
        assert!(!d.speech.contains("abc"));
        assert!(d.speech.split_whitespace().count() <= 12);
        assert_eq!(d.presentation.speech, d.speech);
    }

    #[test]
    fn ai_chat_completion_speaks_completion_and_approval() {
        for app in ["Codex", "Codex Desktop", "Herdr", "Claude"] {
            let d = classify(&n(app, "Task completed", ""));
            assert_eq!(d.category, "approval");
            assert_eq!(d.need, Need::Approval);
            assert!(d.speak);
            assert_eq!(
                d.speech,
                format!("{} has completed and is awaiting your approval.", if app.contains("Codex") { "Codex" } else if app == "Herdr" { "Herdr" } else { "Claude" })
            );
        }
    }

    #[test]
    fn stopped_ai_session_is_actionable_when_waiting_for_approval() {
        let d = classify(&n(
            "Codex Desktop",
            "Session stopped",
            "Waiting for your approval",
        ));
        assert_eq!(d.category, "approval");
        assert!(d.speak);
        assert_eq!(d.speech, "Codex has completed and is awaiting your approval.");
    }

    #[test]
    fn opencode_awaiting_input_classifies_as_approval() {
        let d = classify(&n("opencode", "Awaiting your approval", ""));
        assert_eq!(d.category, "approval");
        assert_eq!(d.need, Need::Approval);
        assert!(d.speak);
    }

    #[test]
    fn opencode_question_is_awaiting_input_not_silent() {
        let d = classify(&n("opencode", "Permission needed", ""));
        assert_eq!(d.category, "approval");
        assert!(d.speak);
    }

    #[test]
    fn new_terminal_agents_are_ai_chat_apps() {
        for app in [
            "opencode",
            "OpenCode",
            "gemini",
            "Gemini",
            "gemini-cli",
            "Claude Code",
            "claude-code",
            "amazon-q",
            "qwen",
        ] {
            assert!(
                is_ai_chat_app(&app.to_ascii_lowercase()),
                "expected {app} to be recognized as an AI coding app"
            );
        }
    }

    #[test]
    fn incomplete_ai_output_does_not_sound_like_completion() {
        let d = classify(&n("Codex Desktop", "Incomplete output", ""));
        assert_ne!(d.category, "approval");
        assert!(!d.speak);
    }

    #[test]
    fn task_fabric_svn_degraded_uses_typed_context() {
        let mut x = n("Task Fabric SVN Pulse", "update", "");
        x.task_fabric_svn = Some(TaskFabricSvnPresentation {
            category: Some("warning".into()),
            need: Some("the API service needs attention".into()),
            context: Some("Example Project".into()),
            outcome: Some("degraded".into()),
        });
        let d = classify(&x);
        assert_eq!(d.earcon, "blocked");
        assert!(d.speech.contains("API service"));
    }
    #[test]
    fn dnd_suppresses_noncritical() {
        let mut x = n("Discord", "message", "hello");
        x.dnd = true;
        assert!(classify(&x).suppressed);
    }
    #[test]
    fn dnd_allows_local_critical() {
        let mut x = n("notify-send", "Battery failure", "critical");
        x.dnd = true;
        x.urgency = "critical".into();
        let d = classify(&x);
        assert!(!d.suppressed);
        assert_eq!(d.category, "critical");
    }
    #[test]
    fn sanitizer_removes_paths_urls_and_secrets() {
        let value = sanitize("open /home/user/x https://example.com token=secret hello");
        assert_eq!(value, "open hello");
    }
    #[test]
    fn unknown_is_model_candidate() {
        let d = classify(&n("Odd App", "A curious event", "something happened"));
        assert_eq!(d.classification_source, "model");
        assert_eq!(d.attention_level, AttentionLevel::Ambient);
    }

    #[test]
    fn phrase_compiler_uses_short_need_language() {
        let mut notification = n("GitHub", "Authentication required", "sign in");
        notification.task_fabric_svn = Some(TaskFabricSvnPresentation {
            category: Some("approval".into()),
            need: Some("authentication".into()),
            context: None,
            outcome: None,
        });
        let d = classify(&notification);
        assert_eq!(d.speech, "GitHub needs authentication.");
        assert!(d.speech.split_whitespace().count() <= 8);
    }

    #[test]
    fn pkexec_authentication_speaks_system_need() {
        let d = classify(&n(
            "pkexec",
            "Authentication is required",
            "Enter your password",
        ));
        assert_eq!(d.category, "approval");
        assert_eq!(d.need, Need::Authentication);
        assert!(d.speak);
        assert_eq!(d.speech, "System authentication required.");
    }

    #[test]
    fn generic_polkit_app_name_is_enough_to_speak() {
        let d = classify(&n("org.freedesktop.PolicyKit1", "Authorize action", ""));
        assert!(d.speak);
        assert_eq!(d.speech, "System authentication required.");
    }

    #[test]
    fn phrase_book_app_scoped_beats_global() {
        let mut book = PhraseBook {
            approval: vec!["global {app}: {summary}".into()],
            ..Default::default()
        };
        book.by_app.insert(
            "opencode".into(),
            AppPhrases {
                approval: vec!["opencode special {app}".into()],
                label: Some("OpenCode".into()),
                ..Default::default()
            },
        );
        assert_eq!(book.override_for("approval", "opencode", "id-1", "hi"), "opencode special opencode");
        assert_eq!(book.override_for("approval", "codex", "id-1", "hi"), "global codex: hi");
        assert_eq!(book.override_for("blocked", "codex", "id-1", "hi"), "");
    }

    #[test]
    fn phrase_book_picks_deterministically_by_id() {
        let book = PhraseBook {
            success: vec!["first".into(), "second".into()],
            ..Default::default()
        };
        let a = book.override_for("success", "x", "same-notification", "");
        let b = book.override_for("success", "x", "same-notification", "");
        assert_eq!(a, b);
        assert!(matches!(a.as_str(), "first" | "second"));
    }

    #[test]
    fn finalize_speech_injects_extra_hint() {
        let d = AudioDecision::new("approval", "your approval", "opencode", "approval", "high");
        let speech = finalize_speech(
            &d,
            &SpeechContext {
                id: "x",
                app: "opencode",
                summary: "Approval required",
                body: "",
                extra: "wants to run bash",
                workspace: "",
                details: "",
                book: None,
                word_cap: 20,
            },
        );
        assert_eq!(speech, "Opencode needs your approval to run bash.");
    }

    #[test]
    fn finalize_speech_injects_body_excerpt_for_approval() {
        let d = AudioDecision::new("approval", "your approval", "codex", "approval", "high");
        let speech = finalize_speech(
            &d,
            &SpeechContext {
                id: "x",
                app: "codex",
                summary: "Approval required",
                body: "Please approve git push to origin/main",
                extra: "",
                workspace: "",
                details: "",
                book: None,
                word_cap: 20,
            },
        );
        assert_eq!(speech, "Codex needs your approval for git push to origin/main.");
    }

    #[test]
    fn finalize_speech_enforces_word_cap() {
        let d = AudioDecision::new("approval", "your approval", "codex", "approval", "high");
        let speech = finalize_speech(
            &d,
            &SpeechContext {
                id: "x",
                app: "codex",
                summary: "Approval required",
                body: "one two three four five six seven eight nine ten eleven twelve thirteen",
                extra: "",
                workspace: "",
                details: "",
                book: None,
                word_cap: 8,
            },
        );
        assert!(speech.split_whitespace().count() <= 8);
    }

    #[test]
    fn humor_composer_always_in_bounds() {
        let spec = HumorSpec::default();
        for id in 0..32u64 {
            let id = format!("n{id}");
            let line = compose_humor(HumorKind::Playful, &spec, false, &id, "opencode", "done");
            assert!(!line.is_empty());
            let haiku = compose_humor(HumorKind::Haiku, &spec, false, &id, "opencode", "done");
            assert!(haiku.split_whitespace().count() <= 25);
            assert!(haiku.ends_with('.'));
        }
    }

    #[test]
    fn humor_failure_lists_are_cooled_down_not_approval() {
        let spec = HumorSpec::default();
        let playful = compose_humor(HumorKind::Playful, &spec, true, "fail-1", "codex", "");
        assert!(!playful.is_empty());
        assert!(!playful.contains("approval"));
    }

    #[test]
    fn body_excerpt_strips_paths_and_secrets() {
        assert_eq!(body_excerpt("run /home/user/x token=abc git push"), "run git push");
        assert!(body_excerpt("token=supersecret").is_empty());
    }

    #[test]
    fn contextual_substitution_replaces_repo_tool_duration() {
        let vars = ContextVars {
            repo: Some("task-fabric-svn"),
            tool: Some("cargo test"),
            duration: Some("4.2s"),
            time_of_day: Some("late_night"),
        };
        let res = substitute_context(
            "{app} compiled {repo} via {tool} in {duration}.",
            "codex",
            "done",
            &vars,
        );
        assert_eq!(res, "codex compiled task-fabric-svn via cargo test in 4.2s.");
    }

    #[test]
    fn compose_humor_rich_routes_persona_and_situational() {
        let mut spec = HumorSpec::default();
        spec.codex_success = vec!["Codex deadpan: {repo} is fixed.".into()];
        spec.claude_success = vec!["Claude poetic: {repo} achieved harmony.".into()];
        spec.recovery = vec!["Praise the machine: {repo} recovered!".into()];

        let vars = ContextVars {
            repo: Some("test-box"),
            ..Default::default()
        };

        // Recovery trigger
        let line = compose_humor_rich(
            HumorKind::Playful,
            &spec,
            false,
            "id-recovery-0", // hash % 2 == 0
            "codex",
            "done",
            &vars,
            true,
        );
        assert!(line.contains("test-box"));

        // Persona trigger for claude
        let claude_line = compose_humor_rich(
            HumorKind::Playful,
            &spec,
            false,
            "id-claude",
            "claude",
            "done",
            &vars,
            false,
        );
        assert!(claude_line.contains("Claude poetic: test-box achieved harmony."));

        // Persona trigger for codex
        let codex_line = compose_humor_rich(
            HumorKind::Playful,
            &spec,
            false,
            "id-codex",
            "codex",
            "done",
            &vars,
            false,
        );
        assert!(codex_line.contains("Codex deadpan: test-box is fixed."));
    }

    #[test]
    fn test_tiered_caution_approval_triggers_critical_earcon() {
        let envelope = NotificationEnvelope {
            app: "Claude Code".into(),
            summary: "Caution: Claude needs approval".into(),
            risk: Some("high".into()),
            extra: Some("Caution: wants to delete files".into()),
            task_fabric_svn: Some(TaskFabricSvnPresentation {
                category: Some("approval".into()),
                need: Some("tool_permission".into()),
                context: Some("Claude Code".into()),
                outcome: None,
            }),
            ..Default::default()
        };
        let decision = classify(&envelope);
        assert_eq!(decision.category, "approval");
        assert_eq!(decision.earcon, "critical");
        assert_eq!(decision.inferred_need, "caution approval");

        let speech = finalize_speech(
            &decision,
            &SpeechContext {
                id: "1",
                app: &envelope.app,
                summary: &envelope.summary,
                body: "",
                extra: envelope.extra.as_deref().unwrap_or(""),
                workspace: "",
                details: "",
                book: None,
                word_cap: 20,
            },
        );
        assert_eq!(speech, "Claude Code needs your approval to delete files.");
    }


    #[test]
    fn approval_speech_composes_who_what_where() {
        let d = AudioDecision::new("approval", "your approval", "opencode", "approval", "high");
        let speech = finalize_speech(
            &d,
            &SpeechContext {
                id: "x",
                app: "opencode",
                summary: "opencode needs your approval",
                body: "Caution: opencode wants to access files in /home/user/.config/task_fabric_svn outside the workspace",
                extra: "Caution: wants to access files in /home/user/.config/task_fabric_svn outside the workspace",
                workspace: "ExampleField",
                details: "{\"permission\": \"external_directory\"}",
                book: None,
                word_cap: 30,
            },
        );
        assert_eq!(
            speech,
            "Opencode needs your approval to access files in dot-config task fabric svn, outside the ExampleField workspace."
        );
    }

    #[test]
    fn approval_speech_appends_workspace_when_intent_names_no_location() {
        let d = AudioDecision::new("approval", "your approval", "codex", "approval", "high");
        let speech = finalize_speech(
            &d,
            &SpeechContext {
                id: "x",
                app: "codex",
                summary: "Codex needs your approval",
                body: "",
                extra: "wants to run the test suite",
                workspace: "ExampleField",
                details: "",
                book: None,
                word_cap: 30,
            },
        );
        assert_eq!(
            speech,
            "Codex needs your approval to run the test suite, in the ExampleField workspace."
        );
    }

    #[test]
    fn approval_speech_never_says_done() {
        let d = AudioDecision::new("approval", "your approval", "opencode", "approval", "high");
        let speech = finalize_speech(
            &d,
            &SpeechContext {
                id: "x",
                app: "opencode",
                summary: "opencode needs your approval",
                body: "",
                extra: "wants to run npm test",
                workspace: "user",
                details: "",
                book: None,
                word_cap: 30,
            },
        );
        let lower = speech.to_lowercase();
        assert!(!lower.contains("done"));
        assert!(!lower.contains("finished"));
        assert!(!lower.contains("completed"));
        assert!(speech.contains("needs your approval"));
    }

    #[test]
    fn approval_speech_scrubs_secrets() {
        let d = AudioDecision::new("approval", "your approval", "opencode", "approval", "high");
        let speech = finalize_speech(
            &d,
            &SpeechContext {
                id: "x",
                app: "opencode",
                summary: "opencode needs your approval",
                body: "",
                extra: "wants to run deploy --token=abc123",
                workspace: "",
                details: "",
                book: None,
                word_cap: 30,
            },
        );
        assert!(!speech.contains("abc123"));
        assert!(speech.contains("needs your approval"));
    }

    #[test]
    fn approval_speech_keeps_classifier_for_bare_completion() {
        // A genuine completion awaiting approval with no intent of its own
        // keeps the classifier's speech instead of inventing one.
        let mut d = AudioDecision::new("approval", "your approval", "codex", "approval", "high");
        d.speech = "Codex has completed and is awaiting your approval.".into();
        let speech = finalize_speech(
            &d,
            &SpeechContext {
                id: "x",
                app: "codex",
                summary: "Session stopped",
                body: "",
                extra: "",
                workspace: "",
                details: "",
                book: None,
                word_cap: 30,
            },
        );
        assert_eq!(speech, "Codex has completed and is awaiting your approval.");
    }

    #[test]
    fn speakable_path_renders_dotfiles_home_relative() {
        assert_eq!(speakable_path("/home/user/.config/task_fabric_svn"), "dot-config task fabric svn");
        assert_eq!(speakable_path("~/notes/todo.md"), "notes todo.md");
        assert_eq!(speakable_path("/etc/hosts"), "etc hosts");
        assert_eq!(speakable_path("/"), "the path");
    }

    #[test]
    fn missing_humor_vars_degrade_to_empty_not_words() {
        let vars = ContextVars::default();
        let res = substitute_context(
            "{app} compiled {repo} via {tool} in {duration}.",
            "codex",
            "done",
            &vars,
        );
        assert!(!res.contains("task"));
        assert!(!res.contains("the repo"));
        assert_eq!(res, "codex compiled via in .");
    }

    #[test]
    fn test_persona_leadin_with_duration() {
        let vars = ContextVars {
            repo: Some("task-fabric-svn"),
            duration: Some("14s"),
            time_of_day: Some("afternoon"),
            tool: None,
        };
        let leadin = persona_leadin("claude", &vars);
        assert_eq!(leadin, "This afternoon in 14s, Claude wrapped up in task-fabric-svn:");
    }
}
