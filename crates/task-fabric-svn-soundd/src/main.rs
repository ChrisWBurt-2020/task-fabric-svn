use task_fabric_svn_work_core::notification_audio::{
    classify, compile_phrase, compose_humor_rich, finalize_speech, fnv1a,
    refresh_presentation, sanitize, sanitize_n, AudioDecision, ContextVars, HumorKind, HumorSpec,
    NotificationEnvelope, PhraseBook, SpeechContext,
};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
struct AudioConfig {
    mode: String,
    speech: bool,
    master_volume: u16,
    speech_volume: u16,
    boost_approved: bool,
    speech_profile: String,
    sound_dir: String,
    template_dir: String,
    voice: String,
    model: String,
    quip_server_url: String,
    kokoro_model: String,
    kokoro_voices: String,
    kokoro_voice: String,
    kokoro_voices_by_app: HashMap<String, String>,
    kokoro_voices_by_category: HashMap<String, String>,
    kokoro_voice_override: String,
    espeak_fallback: bool,
    max_speech_per_minute: u8,
    max_speech_per_10_minutes: u8,
    media_ducking: bool,
    media_duck_percent: u8,
    humor: String,
    humor_kind: HumorKind,
    humor_failure: bool,
    humor_failure_rate: u8,
    humor_max_per_10_minutes: u8,
    humor_persona: String,
    phrase_word_cap: u16,
    speech_read_min_words: u16,
    speech_read_cap: u16,
    phrases_path: Option<String>,
}

impl Default for AudioConfig {
    fn default() -> Self {
        Self {
            mode: "full".into(),
            speech: true,
            master_volume: 70,
            speech_volume: 80,
            boost_approved: false,
            speech_profile: "kokoro".into(),
            sound_dir: "%h/.local/share/task-fabric-svn/sounds".into(),
            template_dir: "%h/.local/share/task-fabric-svn/templates".into(),
            voice: "%h/.local/share/task-fabric-svn/voices/en_US-lessac-medium.onnx".into(),
            model: "%h/.local/share/task-fabric-svn/models/qwen2.5-1.5b-instruct-q4_k_m.gguf"
                .into(),
            quip_server_url: "http://127.0.0.1:12121".into(),
            kokoro_model: "%h/.local/share/task-fabric-svn/kokoro/kokoro-v1.0.onnx".into(),
            kokoro_voices: "%h/.local/share/task-fabric-svn/kokoro/voices-v1.0.bin".into(),
            kokoro_voice: "af_heart".into(),
            kokoro_voices_by_app: HashMap::new(),
            kokoro_voices_by_category: HashMap::new(),
            kokoro_voice_override: String::new(),
            espeak_fallback: false,
            max_speech_per_minute: 3,
            max_speech_per_10_minutes: 8,
            media_ducking: true,
            media_duck_percent: 35,
            humor: "dry".into(),
            humor_kind: HumorKind::Dry,
            humor_failure: false,
            humor_failure_rate: 15,
            humor_max_per_10_minutes: 0,
            humor_persona: String::new(),
            phrase_word_cap: 20,
            speech_read_min_words: 0,
            speech_read_cap: 40,
            phrases_path: None,
        }
    }
}

#[derive(Clone, Default)]
struct Runtime {
    recent_keys: VecDeque<(String, u64)>,
    last_audio_at: u64,
    last_attention: task_fabric_svn_work_core::notification_audio::AttentionLevel,
    last_humor_at: u64,
    speech_events: VecDeque<u64>,
    humor_events: VecDeque<u64>,
    recent_failures: HashMap<String, u64>,
    phrases: PhraseBook,
    humor_spec: HumorSpec,
    play_lock: Arc<Mutex<()>>,
    pending: VecDeque<PendingPlayback>,
    worker_running: bool,
    cached_config: Option<AudioConfig>,
    cached_config_mtime: Option<SystemTime>,
}

#[derive(Clone)]
struct PendingPlayback {
    notification: NotificationEnvelope,
    decision: AudioDecision,
    config: AudioConfig,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
struct ProfileDocument {
    profiles: HashMap<String, ProfileEntry>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
struct ProfileEntry {
    id: String,
    enabled: bool,
    priority: i32,
    humor: ProfileHumor,
    voice: ProfileVoice,
    triggers: ProfileTriggers,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
struct ProfileHumor {
    kind: String,
    persona_prompt: String,
    on_failure: bool,
    failure_rate: u8,
    max_per_10_minutes: u16,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
struct ProfileVoice {
    mode: String,
    fixed: String,
    rotation: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
struct ProfileTriggers {
    apps: Vec<String>,
    categories: Vec<String>,
    urgencies: Vec<String>,
}

impl Runtime {
    /// Reload the phrase book and humor spec from the configured files. Called
    /// at startup and on config reload; missing files leave the defaults.
    fn reload_assets(&mut self, config: &AudioConfig) {
        self.humor_spec = load_humor_spec(config);
        self.phrases = load_phrases(config);
    }

    /// Return the live config, re-reading the file when its mtime advanced.
    /// Validate-then-swap: a missing, unparsable, or invalid file keeps the
    /// last-known-good config and logs a warning instead of breaking audio.
    fn current_config(&mut self) -> AudioConfig {
        self.maybe_reload_config(&config_path());
        self.cached_config.clone().unwrap_or_default()
    }

    fn maybe_reload_config(&mut self, path: &Path) {
        let mtime = fs::metadata(path).and_then(|m| m.modified()).ok();
        let changed = match (mtime, self.cached_config_mtime) {
            (Some(new), Some(old)) => new > old,
            (Some(_), None) => true,
            (None, _) => false,
        };
        if !changed {
            if self.cached_config.is_none() {
                // First call and no file: behave like load_config().
                self.cached_config = Some(load_config());
                self.cached_config_mtime = mtime;
            }
            return;
        }
        match fs::read_to_string(path)
            .ok()
            .and_then(|text| parse_config_text(&text))
        {
            Some(config) if validate_config(&config) => {
                self.reload_assets(&config);
                self.cached_config = Some(config);
                self.cached_config_mtime = mtime;
                eprintln!("task-fabric-svn-soundd: reloaded config from {}", path.display());
            }
            _ => eprintln!(
                "task-fabric-svn-soundd: ignoring config at {} (unreadable, unparsable, or invalid); keeping last-known-good",
                path.display()
            ),
        }
    }
}

fn main() -> std::io::Result<()> {
    if std::env::args().any(|arg| arg == "--version") {
        println!("task-fabric-svn-soundd {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| "/home/user".into());
    let runtime = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| format!("{home}/.cache"));
    let socket = PathBuf::from(
        std::env::var("TASK_FABRIC_SVN_AUDIO_SOCKET")
            .unwrap_or_else(|_| format!("{runtime}/task-fabric-svn/soundd.sock")),
    );
    if let Some(parent) = socket.parent() {
        fs::create_dir_all(parent)?;
    }
    if socket.exists() {
        fs::remove_file(&socket)?;
    }
    let listener = UnixListener::bind(&socket)?;
    let _ = fs::set_permissions(&socket, permissions(0o600));
    eprintln!(
        "task-fabric-svn-soundd {} listening on {}",
        env!("CARGO_PKG_VERSION"),
        socket.display()
    );
    let state = Arc::new(Mutex::new(Runtime::default()));
    if let Ok(mut runtime) = state.lock() {
        // Prime the config cache (and phrase/humor assets) at startup.
        let config = runtime.current_config();
        runtime.reload_assets(&config);
    }
    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let state = Arc::clone(&state);
                thread::spawn(move || handle(stream, state));
            }
            Err(error) => eprintln!("task-fabric-svn-soundd: connection failed: {error}"),
        }
    }
    Ok(())
}

#[cfg(unix)]
fn permissions(mode: u32) -> std::fs::Permissions {
    use std::os::unix::fs::PermissionsExt;
    std::fs::Permissions::from_mode(mode)
}
#[cfg(not(unix))]
fn permissions(_mode: u32) -> std::fs::Permissions {
    std::fs::metadata(".").unwrap().permissions()
}

/// Dry-run previews from the humor settings page run the full pipeline
/// and return the decision JSON, but must never reach the speakers.
fn should_enqueue(
    notification: &NotificationEnvelope,
    decision: &AudioDecision,
    config: &AudioConfig,
) -> bool {
    !notification.dry_run && !decision.suppressed && config.mode != "muted"
}

fn handle(mut stream: UnixStream, state: Arc<Mutex<Runtime>>) {
    const MAX_ENVELOPE_BYTES: u64 = 32 * 1024;
    let mut bytes = Vec::new();
    if BufReader::new(&mut stream)
        .take(MAX_ENVELOPE_BYTES + 1)
        .read_until(b'\n', &mut bytes)
        .is_err()
    {
        return;
    }
    if bytes.len() as u64 > MAX_ENVELOPE_BYTES {
        let _ = stream.write_all(b"{\"error\":\"notification envelope exceeds 32 KiB\"}\n");
        return;
    }
    let line = match std::str::from_utf8(&bytes) {
        Ok(value) => value,
        Err(_) => {
            let _ = stream.write_all(b"{\"error\":\"notification envelope is not UTF-8\"}\n");
            return;
        }
    };
    // Direct speech command for scheduled rituals (the Memento Mori timer).
    // Dispatched before envelope parsing so the command field never
    // reaches the classifier.
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(line.trim()) {
        if value.get("command").and_then(|v| v.as_str()) == Some("speak") {
            let response = handle_speak_command(&value, &state);
            let mut bytes = serde_json::to_vec(&response)
                .unwrap_or_else(|_| b"{\"error\":\"encoding failed\"}".to_vec());
            bytes.push(b'\n');
            let _ = stream.write_all(&bytes);
            return;
        }
    }
    let response = match serde_json::from_str::<NotificationEnvelope>(line.trim()) {
        Ok(mut notification) => {
            let mut config = state
                .lock()
                .map(|mut runtime| runtime.current_config())
                .unwrap_or_else(|_| load_config());
            let mut decision = classify(&notification);
            apply_profile_overrides(&mut notification, &mut config, &decision.category);
            if decision.classification_source == "model" {
                decision = model_or_fallback(&notification, decision, &config);
                refresh_presentation(&mut decision);
            }
            let profile_id = notification.profile_id.clone();
            let profile_priority = notification.profile_priority;
            let resolved_voice = notification.resolved_voice.clone();
            let resolved_voice_source = notification.resolved_voice_source.clone();
            let personality = apply_humor(&notification, &decision, &config, &state);
            finalize_decision_speech(&notification, &mut decision, &config, &state, &personality);
            // Always-on decision ledger: one JSON line per envelope, rotated
            // in place under ~/.local/state. A silent-audio gap (poisoned
            // play lock, below-threshold read, suppression) is then
            // diagnosable after the fact instead of needing a debug-env
            // restart. TASK_FABRIC_SVN_SOUNDD_DEBUG additionally mirrors to stderr.
            let decision_log = decision_log_line(&notification, &decision, &personality);
            if !notification.dry_run {
                log_decision(&decision_log);
            }
            if std::env::var("TASK_FABRIC_SVN_SOUNDD_DEBUG").is_ok() {
                eprintln!("[task-fabric-svn-soundd] {decision_log}");
            }
            if should_enqueue(&notification, &decision, &config) {
                decision = apply_attention_budget(&notification, decision, &config, &state);
                refresh_presentation(&mut decision);
                enqueue(notification, decision.clone(), config, state);
            }
            let mut response = serde_json::to_value(decision)
                .unwrap_or_else(|_| serde_json::json!({"error":"encoding failed"}));
            if let Some(object) = response.as_object_mut() {
                object.insert("profile_id".into(), serde_json::json!(profile_id));
                object.insert("profile_priority".into(), serde_json::json!(profile_priority));
                object.insert("resolved_voice".into(), serde_json::json!(resolved_voice));
                object.insert("resolved_voice_source".into(), serde_json::json!(resolved_voice_source));
            }
            response
        }
        Err(error) => {
            serde_json::json!({"error": format!("invalid notification envelope: {error}")})
        }
    };
    let mut bytes = serde_json::to_vec(&response)
        .unwrap_or_else(|_| b"{\"error\":\"encoding failed\"}".to_vec());
    bytes.push(b'\n');
    let _ = stream.write_all(&bytes);
}

fn apply_attention_budget(
    notification: &NotificationEnvelope,
    mut decision: AudioDecision,
    config: &AudioConfig,
    state: &Arc<Mutex<Runtime>>,
) -> AudioDecision {
    // Manual previews are an explicit user action and must never consume the
    // production speech budget.
    if notification.id.contains("preview-") {
        return decision;
    }
    if !decision.speak
        || decision.attention_level
            == task_fabric_svn_work_core::notification_audio::AttentionLevel::Interrupt
        || decision.need == task_fabric_svn_work_core::notification_audio::Need::Authentication
    {
        return decision;
    }
    let now = now_ms();
    let mut guard = match state.lock() {
        Ok(value) => value,
        Err(_) => return decision,
    };
    while guard
        .speech_events
        .front()
        .is_some_and(|timestamp| now.saturating_sub(*timestamp) > 600_000)
    {
        guard.speech_events.pop_front();
    }
    let minute_count = guard
        .speech_events
        .iter()
        .filter(|timestamp| now.saturating_sub(**timestamp) <= 60_000)
        .count();
    let ten_minute_limit = usize::from(config.max_speech_per_10_minutes);
    let minute_limit = usize::from(config.max_speech_per_minute);
    if (minute_limit > 0 && minute_count >= minute_limit)
        || (ten_minute_limit > 0 && guard.speech_events.len() >= ten_minute_limit)
    {
        decision.speak = false;
        decision.speech.clear();
        decision.suppression_reason = Some("attention_budget".into());
        return decision;
    }
    let _ = notification;
    decision
}

fn record_spoken_event(state: &Arc<Mutex<Runtime>>) {
    if let Ok(mut runtime) = state.lock() {
        runtime.speech_events.push_back(now_ms());
    }
}

/// Decide whether a notification earns a personality clause, and compose it.
///
/// Phase B bookends: this returns only the detachable personality clause —
/// a generated milestone quip or a humor-pool line — and never touches the
/// deterministic frame. The caller appends the clause after the frame is
/// composed. Approvals and criticals always return `None`.
fn apply_humor(
    notification: &NotificationEnvelope,
    decision: &AudioDecision,
    config: &AudioConfig,
    state: &Arc<Mutex<Runtime>>,
) -> Option<String> {
    // Humor is reserved for positively classified completions unless the
    // user opted into failure humor. Approval and critical stay pristine.
    let is_failure = decision.category == "blocked" || decision.category == "warning";
    if config.humor_kind == HumorKind::Off || decision.category == "approval" || decision.category == "critical" {
        return None;
    }
    // A custom persona overrides the style dropdown: when set, every
    // eligible notification gets a persona-voiced generated quip instead of
    // a pool line. Off above remains the master kill switch.
    let persona_active = !config.humor_persona.trim().is_empty();
    if is_failure && !config.humor_failure {
        return None;
    }
    let eligible = decision.category == "success" || is_failure;
    if !eligible {
        return None;
    }
    // A substantive body is a real message worth reading; humor defers so the
    // agent's actual words are spoken instead of replaced by a joke.
    let read_min = usize::from(config.speech_read_min_words);
    if read_min > 0 {
        let body_words = notification.body.split_whitespace().count();
        if body_words >= read_min {
            return None;
        }
    }
    let forced = notification.id.starts_with("humor-preview-");
    let now = now_ms();
    // Selection: the explicit kinds (playful/blunt/noir/haiku) always apply
    // to an eligible category; only the "dry" persona stays deliberately
    // occasional, gated by humor_failure_rate. `hash >= threshold` skips.
    let threshold = if persona_active {
        100u64
    } else {
        match config.humor_kind {
            HumorKind::Playful | HumorKind::Blunt | HumorKind::Noir | HumorKind::Haiku => 100u64,
            HumorKind::Off => 0,
            HumorKind::Dry => u64::from(config.humor_failure_rate.min(100)),
        }
    };
    if !forced && (fnv1a(notification.id.as_bytes()) % 100) >= threshold {
        return None;
    }
    let Ok(mut runtime) = state.lock() else {
        return None;
    };
    // If a humor-max-per-10-minutes is set, throttle by count; otherwise
    // only the "dry" persona keeps the once-per-10-minutes gate.
    let humor_rate_gate = config.humor_max_per_10_minutes > 0;
    let is_occasional = !persona_active
        && config.humor_kind == HumorKind::Dry
        && config.humor_max_per_10_minutes == 0;
    if humor_rate_gate {
        runtime
            .humor_events
            .retain(|ts| now.saturating_sub(*ts) <= 600_000);
        if !forced
            && runtime.humor_events.len()
                >= usize::from(config.humor_max_per_10_minutes)
        {
            return None;
        }
    } else if !forced
        && is_occasional
        && now.saturating_sub(runtime.last_humor_at) < 600_000
    {
        return None;
    }
    if !forced {
        runtime.last_humor_at = now;
        runtime.humor_events.push_back(now);
    }
    let (app, summary) = (&notification.app, &notification.summary);
    let ws = notification.workspace.as_deref();
    let failure_key = format!("{}-{}", app, ws.unwrap_or("default"));
    let is_recovery = if forced {
        // Manual previews never mutate recovery state.
        false
    } else if !is_failure {
        runtime
            .recent_failures
            .remove(&failure_key)
            .map(|ts| now.saturating_sub(ts) < 600_000)
            .unwrap_or(false)
    } else {
        runtime.recent_failures.insert(failure_key, now);
        false
    };

    let vars = ContextVars {
        repo: ws,
        tool: notification.extra.as_deref(),
        duration: notification.duration.as_deref(),
        time_of_day: notification.time_of_day.as_deref(),
    };

    let is_milestone = !is_failure
        && (is_recovery
            || notification
                .duration
                .as_deref()
                .and_then(|d| d.trim_end_matches('s').parse::<f64>().ok())
                .is_some_and(|s| s >= 120.0));

    // Milestone quips come only from the local generator. If it is down or
    // slow, the notification degrades to frame-only speech: no personality
    // clause is attached, and there is never a pool fallback.
    let speech = if is_milestone || persona_active {
        generate_milestone_quip(notification, config, is_milestone)?
    } else {
        compose_humor_rich(
            config.humor_kind,
            &runtime.humor_spec,
            is_failure,
            &notification.id,
            app,
            summary,
            &vars,
            is_recovery,
        )
    };
    // The personality clause is returned, not spoken directly: the caller
    // bookends it after the deterministic frame. An empty clause degrades
    // to no personality, so the frame stands alone.
    let clause = speech.trim();
    if clause.is_empty() {
        None
    } else {
        Some(clause.to_string())
    }
}

/// Ask the persistent local llama-server for a one-sentence milestone quip.
///
/// The server keeps the GGUF resident, so a warm request completes in a few
/// hundred milliseconds. If the server is down or slow, this returns `None`
/// and the caller emits frame-only speech — the notification is never
/// blocked on generation, and there is never a pool fallback.
/// Build the local-generator prompt for a personality clause. A custom
/// persona replaces the fixed "witty, humorous" voice; the milestone framing
/// is kept only for actual milestones so quick reactions don't claim one.
fn quip_prompt(app: &str, ws: &str, summary: &str, persona: &str, is_milestone: bool) -> String {
    if persona.is_empty() {
        format!("You are an AI coding assistant ({app}). Write a single witty, humorous one-sentence quip (under 12 words, no emojis) celebrating finishing a major milestone in {ws}: {summary}. Output ONLY the quip text without quotes.")
    } else if is_milestone {
        format!("You are {persona}. Write a single one-sentence quip (under 12 words, no emojis) in that voice, celebrating finishing a major milestone in {ws}: {summary}. Output ONLY the quip text without quotes.")
    } else {
        format!("You are {persona}. Write a single one-sentence quip (under 12 words, no emojis) in that voice, reacting briefly to this update in {ws}: {summary}. Output ONLY the quip text without quotes.")
    }
}

/// Persona text for the quip prompt: the full validated value (up to 500
/// chars), not the 12-token spoken-text sanitize. The persona is never
/// spoken directly -- it only steers the local generator -- so truncating
/// it mangles the voice without any safety benefit.
fn persona_for_prompt(config: &AudioConfig) -> String {
    config.humor_persona.trim().chars().take(500).collect()
}

fn generate_milestone_quip(
    notification: &NotificationEnvelope,
    config: &AudioConfig,
    is_milestone: bool,
) -> Option<String> {
    let app = sanitize(&notification.app);
    let summary = sanitize(&notification.summary);
    let ws = notification.workspace.as_deref().unwrap_or("repo");
    let persona = persona_for_prompt(config);
    let prompt = quip_prompt(&app, ws, &summary, &persona, is_milestone);
    let body = serde_json::json!({
        "prompt": prompt,
        "n_predict": 30,
        "temperature": 0.7,
        "cache_prompt": true,
    })
    .to_string();
    let url = format!(
        "{}/completion",
        config.quip_server_url.trim_end_matches('/')
    );
    let output = Command::new("curl")
        .args([
            "-s",
            "-m",
            "3",
            "-X",
            "POST",
            "-H",
            "Content-Type: application/json",
            "-d",
            &body,
            &url,
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).ok()?;
    let text = value.get("content")?.as_str()?;
    clean_generated_quip(text)
}

/// Clean a locally generated quip for speech: sanitize, strip emojis, and
/// enforce the word cap. Returns `None` when nothing speakable remains.
fn clean_generated_quip(text: &str) -> Option<String> {
    let text = text.trim().trim_matches('"').trim();
    if text.is_empty() {
        return None;
    }
    // Prefer the first complete sentence: the local model often rambles past
    // the word cap, and the first sentence is usually the quip. Sentence
    // detection runs on the raw output because sanitize() strips edge
    // punctuation (including sentence-final periods).
    let candidate = match text.find(&['.', '!', '?'][..]) {
        Some(i) => &text[..=i],
        // No terminal punctuation and longer than the cap: a cutoff fragment
        // or an unstructured ramble -> reject, degrading to frame-only speech.
        None if text.split_whitespace().count() > 18 => return None,
        None => text,
    };
    let stripped = strip_emoji(&sanitize_n(candidate, 18));
    let cleaned = stripped
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let words = cleaned.split_whitespace().count();
    if (3..=18).contains(&words) {
        Some(cleaned)
    } else {
        None
    }
}

/// Remove emoji characters from generated text. The local model ignores the
/// "no emojis" instruction often enough that the prompt alone cannot be
/// trusted, and the TTS phonemizer speaks emoji names aloud ("trophy",
/// "flexed") instead of skipping them.
fn strip_emoji(text: &str) -> String {
    text.chars().filter(|c| !is_emoji(*c)).collect()
}

fn is_emoji(c: char) -> bool {
    matches!(c as u32,
        0x1F600..=0x1F64F  // emoticons
        | 0x1F300..=0x1F5FF // misc symbols and pictographs
        | 0x1F680..=0x1F6FF // transport and map symbols
        | 0x1F900..=0x1F9FF // supplemental symbols and pictographs
        | 0x1FA70..=0x1FAFF // symbols and pictographs extended-A
        | 0x2600..=0x26FF   // misc symbols
        | 0x2700..=0x27BF   // dingbats
        | 0x1F1E6..=0x1F1FF // regional indicators (flags)
        | 0xFE00..=0xFE0F   // variation selectors
        | 0x200D            // zero-width joiner
        | 0x20E3            // combining enclosing keycap
    )
}


fn model_or_fallback(
    notification: &NotificationEnvelope,
    mut fallback: AudioDecision,
    config: &AudioConfig,
) -> AudioDecision {
    let model = expand_home(&config.model);
    if !Path::new(&model).exists() || Command::new("llama-cli").arg("--version").output().is_err() {
        fallback.classification_source = "fallback".into();
        return fallback;
    }
    let prompt = format!("Classify this desktop notification. Return JSON only with category one of routine,message,success,approval,blocked,warning,critical; need a short action phrase; context a short safe label; speak boolean. Never quote the body. App: {} Summary: {} Body: {}", sanitize(&notification.app), sanitize(&notification.summary), sanitize(&notification.body));
    let mut child = match Command::new("llama-cli")
        .args(["-m", &model, "-p", &prompt, "-n", "80", "--temp", "0"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(child) => child,
        Err(_) => {
            fallback.classification_source = "fallback".into();
            return fallback;
        }
    };
    let start = Instant::now();
    loop {
        if let Ok(Some(_)) = child.try_wait() {
            break;
        }
        if start.elapsed() > Duration::from_millis(800) {
            let _ = child.kill();
            fallback.classification_source = "fallback".into();
            return fallback;
        }
        thread::sleep(Duration::from_millis(20));
    }
    let output = child
        .wait_with_output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default();
    let Some(start) = output.find('{') else {
        fallback.classification_source = "fallback".into();
        return fallback;
    };
    let Some(end) = output[start..].find('}') else {
        fallback.classification_source = "fallback".into();
        return fallback;
    };
    let value: serde_json::Value = match serde_json::from_str(&output[start..start + end + 1]) {
        Ok(value) => value,
        Err(_) => {
            fallback.classification_source = "fallback".into();
            return fallback;
        }
    };
    let category = value.get("category").and_then(|v| v.as_str()).unwrap_or("");
    if ![
        "routine", "message", "success", "approval", "blocked", "warning", "critical",
    ]
    .contains(&category)
    {
        fallback.classification_source = "fallback".into();
        return fallback;
    }
    fallback.category = category.into();
    if category == "critical" {
        fallback.priority = "critical".into();
    }
    fallback.inferred_need = sanitize(
        value
            .get("need")
            .and_then(|v| v.as_str())
            .unwrap_or("notification arrived"),
    );
    fallback.safe_context = sanitize(value.get("context").and_then(|v| v.as_str()).unwrap_or(""));
    fallback.speak = value
        .get("speak")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
        && category != "routine"
        && category != "message";
    fallback.speech = if fallback.speak {
        compile_phrase(
            &fallback.inferred_need,
            &notification.app,
            &fallback.safe_context,
        )
    } else {
        String::new()
    };
    fallback.earcon = category.into();
    fallback.classification_source = "model".into();
    fallback
}

/// The kokoro-v1.0 voice set, read from the live engine's voices file.
/// Used to validate the speak command's `voice` field.
const KOKORO_VOICES: &[&str] = &[
    "af_alloy",
    "af_aoede",
    "af_bella",
    "af_heart",
    "af_jessica",
    "af_kore",
    "af_nicole",
    "af_nova",
    "af_river",
    "af_sarah",
    "af_sky",
    "am_adam",
    "am_echo",
    "am_eric",
    "am_fenrir",
    "am_liam",
    "am_michael",
    "am_onyx",
    "am_puck",
    "am_santa",
    "bf_alice",
    "bf_emma",
    "bf_isabella",
    "bf_lily",
    "bm_daniel",
    "bm_fable",
    "bm_george",
    "bm_lewis",
    "ef_dora",
    "em_alex",
    "em_santa",
    "ff_siwis",
    "hf_alpha",
    "hf_beta",
    "hm_omega",
    "hm_psi",
    "if_sara",
    "im_nicola",
    "jf_alpha",
    "jf_gongitsune",
    "jf_nezumi",
    "jf_tebukuro",
    "jm_kumo",
    "pf_dora",
    "pm_alex",
    "pm_santa",
    "zf_xiaobei",
    "zf_xiaoni",
    "zf_xiaoxiao",
    "zf_xiaoyi",
    "zm_yunjian",
    "zm_yunxi",
    "zm_yunxia",
    "zm_yunyang",
];

/// Pure validation for the speak command; unit-tested below. Returns
/// (text, voice) with voice possibly empty (the caller resolves the default).
fn validate_speak_request(value: &serde_json::Value) -> Result<(String, String), String> {
    let text = value
        .get("text")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    if text.is_empty() {
        return Err("speak command needs non-empty text".into());
    }
    if text.chars().count() > 2000 {
        return Err("speak text exceeds 2000 characters".into());
    }
    let voice = value
        .get("voice")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    if !voice.is_empty() && !KOKORO_VOICES.contains(&voice.as_str()) {
        return Err(format!("unknown kokoro voice: {voice}"));
    }
    Ok((text, voice))
}

/// Direct speech: {"command":"speak","text":"...","voice":"..."}. Bypasses
/// classification and the production speech budget like a manual test-play,
/// but honors mute. Falls back through kokoro -> piper -> espeak.
fn handle_speak_command(
    value: &serde_json::Value,
    state: &Arc<Mutex<Runtime>>,
) -> serde_json::Value {
    let (text, voice) = match validate_speak_request(value) {
        Ok(pair) => pair,
        Err(error) => return serde_json::json!({"ok": false, "error": error}),
    };
    let config = state
        .lock()
        .map(|mut runtime| runtime.current_config())
        .unwrap_or_else(|_| load_config());
    if config.mode == "muted" || !config.speech {
        return serde_json::json!({"ok": false, "error": "audio is muted"});
    }
    let play_lock = state
        .lock()
        .map(|runtime| Arc::clone(&runtime.play_lock))
        .unwrap_or_else(|poisoned| Arc::clone(&poisoned.into_inner().play_lock));
    let _play_guard = acquire_play_lock(&play_lock);
    let requested_resolved = value
        .get("resolved_voice")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim();
    let resolved = if !requested_resolved.is_empty() {
        requested_resolved.to_string()
    } else if voice.is_empty() {
        resolve_kokoro_voice(&config, "memento-mori", "reflection").to_string()
    } else {
        voice
    };
    let ducked = duck_media(&config);
    let kokoro_model = expand_home(&config.kokoro_model);
    let kokoro_voices = expand_home(&config.kokoro_voices);
    let kokoro_ready = matches!(config.speech_profile.as_str(), "auto" | "kokoro")
        && Path::new(&kokoro_model).exists()
        && Path::new(&kokoro_voices).exists();
    let mut spoken = false;
    if kokoro_ready && KOKORO_VOICES.contains(&resolved.as_str()) && play_kokoro(&text, &kokoro_model, &kokoro_voices, &resolved, &config) {
        spoken = true;
    } else {
        let fallback_voice = expand_home(&config.voice);
        if matches!(config.speech_profile.as_str(), "auto" | "kokoro" | "piper")
            && Path::new(&fallback_voice).exists()
            && play_piper(&text, &fallback_voice, &config)
        {
            spoken = true;
        } else if config.espeak_fallback
            && play_espeak(&text, config.speech_volume, config.boost_approved)
        {
            spoken = true;
        }
    }
    restore_media(ducked);
    if spoken {
        record_spoken_event(state);
        serde_json::json!({
            "ok": true,
            "spoken": true,
            "profile_id": value.get("profile_id").and_then(|v| v.as_str()),
            "profile_priority": value.get("profile_priority").and_then(|v| v.as_i64()).unwrap_or(50),
            "voice": resolved,
        })
    } else {
        serde_json::json!({"ok": false, "error": "playback failed", "profile_id": value.get("profile_id").and_then(|v| v.as_str())})
    }
}

fn resolve_kokoro_voice<'a>(config: &'a AudioConfig, app: &'a str, category: &'a str) -> &'a str {
    if !config.kokoro_voice_override.is_empty() {
        return &config.kokoro_voice_override;
    }
    if let Some(voice) = config.kokoro_voices_by_app.get(&app.to_ascii_lowercase()) {
        return voice.as_str();
    }
    if let Some(voice) = config.kokoro_voices_by_category.get(category) {
        return voice.as_str();
    }
    &config.kokoro_voice
}

fn enqueue(
    notification: NotificationEnvelope,
    decision: AudioDecision,
    config: AudioConfig,
    state: Arc<Mutex<Runtime>>,
) {
    let key = notification
        .replaces_id
        .clone()
        .unwrap_or_else(|| notification.id.clone());
    let now = now_ms();
    // Recover a poisoned runtime lock the same way play() does; bailing would
    // silently drop the notification and its audio.
    let mut guard = match state.lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    };
    while guard
        .recent_keys
        .front()
        .is_some_and(|(_, at)| now.saturating_sub(*at) >= 2000)
    {
        guard.recent_keys.pop_front();
    }
    if guard.recent_keys.iter().any(|(recent, _)| recent == &key) {
        return;
    }
    if notification.speech_queue_policy.as_deref() != Some("priority")
        && now.saturating_sub(guard.last_audio_at) < 2000
        && decision.attention_level <= guard.last_attention
    {
        guard.recent_keys.push_back((key, now));
        return;
    }
    guard.recent_keys.push_back((key, now));
    guard.last_audio_at = now;
    guard.last_attention = decision.attention_level;
    guard.pending.push_back(PendingPlayback { notification, decision, config });
    if guard.worker_running { return; }
    guard.worker_running = true;
    drop(guard);
    thread::spawn(move || drain_queue(state));
}

fn drain_queue(state: Arc<Mutex<Runtime>>) {
    loop {
        let next = {
            let mut guard = match state.lock() { Ok(g) => g, Err(p) => p.into_inner() };
            let index = guard.pending.iter().enumerate()
                .min_by_key(|(_, item)| item.notification.profile_priority)
                .map(|(i, _)| i);
            match index {
                Some(i) => Some(guard.pending.remove(i).expect("queued playback exists")),
                None => { guard.worker_running = false; None }
            }
        };
        let Some(item) = next else { return; };
        let play_lock = match state.lock() {
            Ok(g) => Arc::clone(&g.play_lock),
            Err(p) => Arc::clone(&p.into_inner().play_lock),
        };
        play(item.notification, item.decision, item.config, play_lock, Arc::clone(&state));
    }
}

/// Serialize all notification playback behind one lock, recovering from a
/// poisoned lock instead of silently giving up. A play worker that panics
/// while holding the lock (a subprocess hiccup, a worker killed mid-run)
/// poisons the std Mutex; bailing out on `Err` would mute every subsequent
/// earcon AND read-aloud until soundd restarts — exactly the silent-audio
/// regression seen in the field. `into_inner()` recovers the guard so the
/// next notification plays normally.
fn acquire_play_lock(play_lock: &Mutex<()>) -> std::sync::MutexGuard<'_, ()> {
    match play_lock.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

fn play(
    notification: NotificationEnvelope,
    decision: AudioDecision,
    config: AudioConfig,
    play_lock: Arc<Mutex<()>>,
    state: Arc<Mutex<Runtime>>,
) {
    let _play_guard = acquire_play_lock(&play_lock);
    let ducked = duck_media(&config);
    let sound_dir = expand_home(&config.sound_dir);
    let app_sound = Path::new(&sound_dir).join(format!("{}_{}.oga", decision.earcon, notification.app.to_ascii_lowercase()));
    let sound = if app_sound.exists() {
        app_sound
    } else {
        Path::new(&sound_dir).join(format!("{}.oga", decision.earcon))
    };
    if sound.exists() {
        let volume = effective_volume(config.master_volume, config.boost_approved);
        let _ = Command::new("pw-play")
            .args([
                "--media-role",
                "Notification",
                "--properties",
                "application.name=Task Fabric SVN Notification Audio",
                "--volume",
                &volume.to_string(),
            ])
            .arg(&sound)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    if sound.exists() && decision.speak && !decision.speech.is_empty() {
        thread::sleep(Duration::from_millis(150));
    }
    if config.mode == "sounds-only"
        || !config.speech
        || !decision.speak
        || decision.speech.is_empty()
    {
        restore_media(ducked);
        return;
    }
    if matches!(config.speech_profile.as_str(), "auto" | "templates")
        && play_template(&decision, &config)
    {
        record_spoken_event(&state);
        restore_media(ducked);
        return;
    }
    let voice = expand_home(&config.voice);
    let kokoro_model = expand_home(&config.kokoro_model);
    let kokoro_voices = expand_home(&config.kokoro_voices);
    let kokoro_ready = matches!(config.speech_profile.as_str(), "auto" | "kokoro")
        && Path::new(&kokoro_model).exists()
        && Path::new(&kokoro_voices).exists();
    let app_voice = notification
        .resolved_voice
        .as_deref()
        .filter(|voice| !voice.is_empty())
        .unwrap_or_else(|| resolve_kokoro_voice(&config, &notification.app, &decision.category));
    if kokoro_ready
        && play_kokoro(
            &decision.speech,
            &kokoro_model,
            &kokoro_voices,
            app_voice,
            &config,
        )
    {
        record_spoken_event(&state);
        restore_media(ducked);
        return;
    }
    if matches!(config.speech_profile.as_str(), "auto" | "kokoro" | "piper")
        && Path::new(&voice).exists()
        && play_piper(&decision.speech, &voice, &config)
    {
        record_spoken_event(&state);
        restore_media(ducked);
        return;
    }
    if config.espeak_fallback
        && play_espeak(
            &decision.speech,
            config.speech_volume,
            config.boost_approved,
        )
    {
        record_spoken_event(&state);
    }
    restore_media(ducked);
}

fn play_kokoro(text: &str, model: &str, voices: &str, voice: &str, config: &AudioConfig) -> bool {
    let cache = std::env::var("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            PathBuf::from(format!(
                "{}/.cache",
                std::env::var("HOME").unwrap_or_default()
            ))
        })
        .join("task-fabric-svn/speech-kokoro");
    let _ = fs::create_dir_all(&cache);
    let key = format!("{:x}", fnv1a(format!("{voice}\n{text}").as_bytes()));
    let wav = cache.join(format!("{key}.wav"));
    if !wav.exists() {
        let mut child = match Command::new("kokoro-tts")
            .args([
                "--model", model, "--voices", voices, "--voice", voice, "--output",
            ])
            .arg(&wav)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(value) => value,
            Err(_) => return false,
        };
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(text.as_bytes());
        }
        let start = Instant::now();
        let mut succeeded = false;
        loop {
            match child.try_wait() {
                Ok(Some(status)) => {
                    succeeded = status.success();
                    break;
                }
                Ok(None) => {}
                Err(_) => break,
            }
            // Kokoro loads its ONNX model on the first request. Keep this
            // generous enough for a cold start, while still bounding a bad
            // runtime so the notification queue cannot hang forever.
            if start.elapsed() > Duration::from_secs(20) {
                let _ = child.kill();
                let _ = child.wait();
                let _ = fs::remove_file(&wav);
                return false;
            }
            thread::sleep(Duration::from_millis(25));
        }
        if !succeeded {
            let _ = fs::remove_file(&wav);
            return false;
        }
    }
    if wav.exists() {
        let volume = effective_volume(config.speech_volume, config.boost_approved);
        return Command::new("pw-play")
            .args([
                "--media-role",
                "Notification",
                "--properties",
                "application.name=Task Fabric SVN Notification Kokoro",
                "--volume",
                &volume.to_string(),
            ])
            .arg(wav)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|status| status.success())
            .unwrap_or(false);
    }
    false
}

fn duck_media(config: &AudioConfig) -> Vec<(u32, f32, f32)> {
    if !config.media_ducking || config.media_duck_percent == 0 {
        return Vec::new();
    }
    let output = match Command::new("wpctl").args(["status", "-k"]).output() {
        Ok(value) if value.status.success() => String::from_utf8_lossy(&value.stdout).into_owned(),
        _ => return Vec::new(),
    };
    let mut in_streams = false;
    let mut ducked = Vec::new();
    for line in output.lines() {
        if line.trim() == "└─ Streams:" || line.trim() == "├─ Streams:" {
            in_streams = true;
            continue;
        }
        if in_streams && (line.contains("Video") || line.trim().is_empty()) {
            if line.contains("Video") {
                break;
            }
            continue;
        }
        if !in_streams {
            continue;
        }
        let Some(id) = parse_node_id(line) else {
            continue;
        };
        let lower = line.to_lowercase();
        if lower.contains("task fabric svn") || lower.contains("notification") || lower.contains("tts") {
            continue;
        }
        let Ok(volume_output) = Command::new("wpctl")
            .args(["get-volume", &id.to_string()])
            .output()
        else {
            continue;
        };
        if !volume_output.status.success() {
            continue;
        }
        let volume_text = String::from_utf8_lossy(&volume_output.stdout);
        let Some(current) = volume_text
            .split_whitespace()
            .find_map(|token| token.parse::<f32>().ok())
        else {
            continue;
        };
        let duck_factor = 1.0 - f32::from(config.media_duck_percent.min(80)) / 100.0;
        let target = (current * duck_factor).max(0.02);
        // Smooth duck ramp
        let mid = current - (current - target) * 0.5;
        let _ = Command::new("wpctl")
            .args(["set-volume", &id.to_string(), &format!("{mid:.2}")])
            .status();
        thread::sleep(Duration::from_millis(15));
        if Command::new("wpctl")
            .args(["set-volume", &id.to_string(), &format!("{target:.2}")])
            .status()
            .map(|value| value.success())
            .unwrap_or(false)
        {
            ducked.push((id, current, target));
        }
    }
    ducked
}

fn restore_media(ducked: Vec<(u32, f32, f32)>) {
    for (id, volume, ducked_volume) in ducked {
        let current = Command::new("wpctl")
            .args(["get-volume", &id.to_string()])
            .output()
            .ok()
            .filter(|output| output.status.success())
            .and_then(|output| {
                String::from_utf8_lossy(&output.stdout)
                    .split_whitespace()
                    .find_map(|token| token.parse::<f32>().ok())
            });
        if current.is_none_or(|value| (value - ducked_volume).abs() > 0.015) {
            continue;
        }
        // Smooth exponential 3-step fade-in
        let step1 = ducked_volume + (volume - ducked_volume) * 0.35;
        let step2 = ducked_volume + (volume - ducked_volume) * 0.70;
        let _ = Command::new("wpctl")
            .args(["set-volume", &id.to_string(), &format!("{step1:.2}")])
            .status();
        thread::sleep(Duration::from_millis(20));
        let _ = Command::new("wpctl")
            .args(["set-volume", &id.to_string(), &format!("{step2:.2}")])
            .status();
        thread::sleep(Duration::from_millis(20));
        let _ = Command::new("wpctl")
            .args(["set-volume", &id.to_string(), &format!("{volume:.2}")])
            .status();
    }
}

fn parse_node_id(line: &str) -> Option<u32> {
    let trimmed = line.trim_start();
    let digits: String = trimmed.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() || !trimmed[digits.len()..].starts_with('.') {
        return None;
    }
    digits.parse().ok()
}

fn play_template(decision: &AudioDecision, config: &AudioConfig) -> bool {
    let dir = expand_home(&config.template_dir);
    let root = Path::new(&dir);
    let phrase_path = root.join(format!("{}.oga", phrase_slug(&decision.speech)));
    let category_path = root.join(format!("{}.oga", decision.category));
    let path = if phrase_path.exists() {
        phrase_path
    } else if category_path.exists() {
        category_path
    } else {
        return false;
    };
    let volume = effective_volume(config.speech_volume, config.boost_approved);
    Command::new("pw-play")
        .args([
            "--media-role",
            "Notification",
            "--properties",
            "application.name=Task Fabric SVN Notification Templates",
            "--volume",
            &volume.to_string(),
        ])
        .arg(path)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

fn phrase_slug(value: &str) -> String {
    let mut slug = String::new();
    for character in value.to_lowercase().chars() {
        if character.is_ascii_alphanumeric() {
            slug.push(character);
        } else if !slug.ends_with('-') {
            slug.push('-');
        }
    }
    slug.trim_matches('-').to_string()
}

fn play_piper(text: &str, voice: &str, config: &AudioConfig) -> bool {
    let cache = std::env::var("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            PathBuf::from(format!(
                "{}/.cache",
                std::env::var("HOME").unwrap_or_default()
            ))
        })
        .join("task-fabric-svn/speech");
    let _ = fs::create_dir_all(&cache);
    let key = format!("{:x}", fnv1a(text.as_bytes()));
    let wav = cache.join(format!("{key}.wav"));
    if !wav.exists() {
        let mut child = match Command::new("piper")
            .args(["--model", voice, "--output_file"])
            .arg(&wav)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(c) => c,
            Err(_) => return false,
        };
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(text.as_bytes());
        }
        let start = Instant::now();
        let mut succeeded = false;
        loop {
            match child.try_wait() {
                Ok(Some(status)) => {
                    succeeded = status.success();
                    break;
                }
                Ok(None) => {}
                Err(_) => break,
            }
            if start.elapsed() > Duration::from_secs(8) {
                let _ = child.kill();
                let _ = child.wait();
                let _ = fs::remove_file(&wav);
                return false;
            }
            thread::sleep(Duration::from_millis(25));
        }
        if !succeeded {
            let _ = fs::remove_file(&wav);
            return false;
        }
    }
    if wav.exists() {
        let volume = effective_volume(config.speech_volume, config.boost_approved);
        return Command::new("pw-play")
            .args([
                "--media-role",
                "Notification",
                "--properties",
                "application.name=Task Fabric SVN Notification TTS",
                "--volume",
                &volume.to_string(),
            ])
            .arg(wav)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|status| status.success())
            .unwrap_or(false);
    }
    false
}

fn play_espeak(text: &str, volume: u16, boost_approved: bool) -> bool {
    let mut tts = match Command::new("espeak-ng")
        .args(["-v", "en-us", "-s", "155", "--stdout", text])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(child) => child,
        Err(_) => return false,
    };
    let Some(stdout) = tts.stdout.take() else {
        return false;
    };
    let mut player = match Command::new("pw-play")
        .args([
            "--media-role",
            "Notification",
            "--properties",
            "application.name=Task Fabric SVN Notification TTS",
            "--volume",
            &effective_volume(volume, boost_approved).to_string(),
            "-",
        ])
        .stdin(stdout)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(child) => child,
        Err(_) => return false,
    };
    let _ = tts.wait();
    player
        .wait()
        .map(|status| status.success())
        .unwrap_or(false)
}

fn config_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_default();
    std::env::var("TASK_FABRIC_SVN_AUDIO_CONFIG")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(format!("{home}/.config/task-fabric-svn/audio.toml")))
}

fn profiles_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_default();
    std::env::var("TASK_FABRIC_SVN_PROFILES_FILE")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(format!("{home}/.config/task-fabric-svn/profiles.json")))
}

fn load_profile_document() -> Option<ProfileDocument> {
    let text = fs::read_to_string(profiles_path()).ok()?;
    serde_json::from_str(&text).ok()
}

fn profile_matches(profile: &ProfileEntry, notification: &NotificationEnvelope, category: &str) -> bool {
    let matches = |values: &[String], actual: &str| {
        values.is_empty() || values.iter().any(|value| value.eq_ignore_ascii_case(actual))
    };
    profile.enabled
        && matches(&profile.triggers.apps, &notification.app)
        && matches(&profile.triggers.categories, category)
        && matches(&profile.triggers.urgencies, &notification.urgency)
}

fn apply_profile_overrides(
    notification: &mut NotificationEnvelope,
    config: &mut AudioConfig,
    category: &str,
) {
    let Some(document) = load_profile_document() else { return; };
    let profile = notification
        .profile_id
        .as_deref()
        .and_then(|id| document.profiles.get(id))
        .or_else(|| document
            .profiles
            .values()
            .filter(|profile| profile_matches(profile, notification, category))
            .min_by_key(|profile| (profile.priority, profile.id.clone())))
        .filter(|profile| profile.enabled || notification.profile_id.is_some());
    let Some(profile) = profile else { return; };
    notification.profile_id = Some(profile.id.clone());
    notification.profile_priority = profile.priority.max(0) as u32;
    notification.speech_queue_policy = Some("priority".into());
    config.humor_kind = match profile.humor.kind.as_str() {
        "playful" => HumorKind::Playful,
        "dry" => HumorKind::Dry,
        "blunt" => HumorKind::Blunt,
        "noir" => HumorKind::Noir,
        "haiku" => HumorKind::Haiku,
        _ => HumorKind::Off,
    };
    config.humor_persona = profile.humor.persona_prompt.clone();
    config.humor_failure = profile.humor.on_failure;
    config.humor_failure_rate = profile.humor.failure_rate;
    config.humor_max_per_10_minutes = profile.humor.max_per_10_minutes.min(u16::from(u8::MAX)) as u8;
    let profile_voice = match profile.voice.mode.as_str() {
        "single" if !profile.voice.fixed.is_empty() => Some(profile.voice.fixed.as_str()),
        "round_robin" => profile.voice.rotation.first().map(String::as_str),
        _ => None,
    };
    if let Some(voice) = profile_voice.filter(|voice| KOKORO_VOICES.contains(voice)) {
        notification.resolved_voice = Some(voice.into());
        notification.resolved_voice_source = Some(format!("profile:{}", profile.id));
    } else {
        notification.resolved_voice_source = Some("global_cascade".into());
    }
}

/// Parse config text, applying the legacy `humor` key fallback. Returns None
/// when the TOML does not parse, so a reload can keep the last-known-good
/// config instead of silently resetting to defaults.
fn parse_config_text(text: &str) -> Option<AudioConfig> {
    let mut config: AudioConfig = toml::from_str(text).ok()?;
    // Backwards compatibility: the old `humor = "playful"|"dry"` key feeds
    // the new `humor_kind` when the file doesn't already set humor_kind.
    let has_humor_kind = text
        .lines()
        .any(|line| line.trim_start().starts_with("humor_kind"));
    if !has_humor_kind && !config.humor.is_empty() {
        config.humor_kind = match config.humor.as_str() {
            "playful" => HumorKind::Playful,
            "blunt" => HumorKind::Blunt,
            "noir" => HumorKind::Noir,
            "haiku" => HumorKind::Haiku,
            "off" => HumorKind::Off,
            _ => HumorKind::Dry,
        };
    }
    Some(config)
}

/// Sanity-check a freshly parsed config before it goes live. A bad value
/// here means the file was hand-edited badly or truncated mid-write; the
/// daemon keeps serving the last-known-good config instead.
fn validate_config(config: &AudioConfig) -> bool {
    matches!(config.mode.as_str(), "full" | "muted" | "sounds-only")
        && config.humor_failure_rate <= 100
        && config.humor_persona.chars().count() <= 500
}

fn load_config() -> AudioConfig {
    let path = config_path();
    let Some(text) = fs::read_to_string(&path).ok() else {
        return AudioConfig::default();
    };
    parse_config_text(&text).unwrap_or_default()
}

fn default_phrases_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_default();
    PathBuf::from(format!("{home}/.config/task-fabric-svn/phrases.toml"))
}

fn load_phrases(config: &AudioConfig) -> PhraseBook {
    let path = config
        .phrases_path
        .as_deref()
        .map(expand_home)
        .map(PathBuf::from)
        .unwrap_or_else(default_phrases_path);
    fs::read_to_string(path)
        .ok()
        .and_then(|text| toml::from_str(&text).ok())
        .unwrap_or_default()
}

fn load_humor_spec(config: &AudioConfig) -> HumorSpec {
    let mut spec = HumorSpec::default();
    let path = config
        .phrases_path
        .as_deref()
        .map(expand_home)
        .map(PathBuf::from)
        .unwrap_or_else(default_phrases_path);
    if let Ok(text) = fs::read_to_string(path) {
        if let Ok(value) = toml::from_str::<toml::Value>(&text) {
            if let Some(humor) = value.get("humor") {
                let mut override_spec = HumorSpec::default();
                if let Some(list) = humor.get("dry_success").and_then(|v| v.as_array()) {
                    override_spec.dry_success = strings_from_value(list);
                }
                if let Some(list) = humor.get("playful_success").and_then(|v| v.as_array()) {
                    override_spec.playful_success = strings_from_value(list);
                }
                if let Some(list) = humor.get("blunt_success").and_then(|v| v.as_array()) {
                    override_spec.blunt_success = strings_from_value(list);
                }
                if let Some(list) = humor.get("noir_success").and_then(|v| v.as_array()) {
                    override_spec.noir_success = strings_from_value(list);
                }
                if let Some(list) = humor.get("dry_failure").and_then(|v| v.as_array()) {
                    override_spec.dry_failure = strings_from_value(list);
                }
                if let Some(list) = humor.get("playful_failure").and_then(|v| v.as_array()) {
                    override_spec.playful_failure = strings_from_value(list);
                }
                if let Some(list) = humor.get("blunt_failure").and_then(|v| v.as_array()) {
                    override_spec.blunt_failure = strings_from_value(list);
                }
                if let Some(list) = humor.get("noir_failure").and_then(|v| v.as_array()) {
                    override_spec.noir_failure = strings_from_value(list);
                }
                if let Some(list) = humor.get("haiku").and_then(|v| v.as_array()) {
                    override_spec.haiku = list
                        .iter()
                        .filter_map(|entry| {
                            let arr = entry.as_array()?;
                            if arr.len() != 3 {
                                return None;
                            }
                            let mut lines: [String; 3] = [
                                String::new(),
                                String::new(),
                                String::new(),
                            ];
                            for (i, v) in arr.iter().enumerate().take(3) {
                                lines[i] = v.as_str().map(String::from).unwrap_or_default();
                            }
                            if lines.iter().all(|line| !line.is_empty()) {
                                Some(lines)
                            } else {
                                None
                            }
                        })
                        .collect();
                }
                if let Some(list) = humor.get("codex_success").and_then(|v| v.as_array()) {
                    override_spec.codex_success = strings_from_value(list);
                }
                if let Some(list) = humor.get("codex_failure").and_then(|v| v.as_array()) {
                    override_spec.codex_failure = strings_from_value(list);
                }
                if let Some(list) = humor.get("claude_success").and_then(|v| v.as_array()) {
                    override_spec.claude_success = strings_from_value(list);
                }
                if let Some(list) = humor.get("claude_failure").and_then(|v| v.as_array()) {
                    override_spec.claude_failure = strings_from_value(list);
                }
                if let Some(list) = humor.get("opencode_success").and_then(|v| v.as_array()) {
                    override_spec.opencode_success = strings_from_value(list);
                }
                if let Some(list) = humor.get("opencode_failure").and_then(|v| v.as_array()) {
                    override_spec.opencode_failure = strings_from_value(list);
                }
                if let Some(list) = humor.get("gemini_success").and_then(|v| v.as_array()) {
                    override_spec.gemini_success = strings_from_value(list);
                }
                if let Some(list) = humor.get("gemini_failure").and_then(|v| v.as_array()) {
                    override_spec.gemini_failure = strings_from_value(list);
                }
                if let Some(list) = humor.get("late_night").and_then(|v| v.as_array()) {
                    override_spec.late_night = strings_from_value(list);
                }
                if let Some(list) = humor.get("fast_fix").and_then(|v| v.as_array()) {
                    override_spec.fast_fix = strings_from_value(list);
                }
                if let Some(list) = humor.get("marathon").and_then(|v| v.as_array()) {
                    override_spec.marathon = strings_from_value(list);
                }
                if let Some(list) = humor.get("recovery").and_then(|v| v.as_array()) {
                    override_spec.recovery = strings_from_value(list);
                }
                // Only replace non-empty overrides so a partial [humor] table
                // doesn't wipe the built-ins.
                if override_spec.dry_success.is_empty() {
                    override_spec.dry_success = spec.dry_success.clone();
                }
                if override_spec.playful_success.is_empty() {
                    override_spec.playful_success = spec.playful_success.clone();
                }
                if override_spec.blunt_success.is_empty() {
                    override_spec.blunt_success = spec.blunt_success.clone();
                }
                if override_spec.noir_success.is_empty() {
                    override_spec.noir_success = spec.noir_success.clone();
                }
                if override_spec.dry_failure.is_empty() {
                    override_spec.dry_failure = spec.dry_failure.clone();
                }
                if override_spec.playful_failure.is_empty() {
                    override_spec.playful_failure = spec.playful_failure.clone();
                }
                if override_spec.blunt_failure.is_empty() {
                    override_spec.blunt_failure = spec.blunt_failure.clone();
                }
                if override_spec.noir_failure.is_empty() {
                    override_spec.noir_failure = spec.noir_failure.clone();
                }
                if override_spec.haiku.is_empty() {
                    override_spec.haiku = spec.haiku.clone();
                }
                spec = override_spec;
            }
        }
    }
    spec
}

fn strings_from_value(list: &[toml::Value]) -> Vec<String> {
    list.iter()
        .filter_map(|v| v.as_str().map(String::from))
        .collect()
}

/// Apply the phrase book and extra/body enrichment to the final spoken line,
/// then append any personality clause as a detachable bookend.
///
/// The deterministic frame is always composed first and never altered by
/// personality. When `personality` holds a clause (a generated milestone
/// quip or humor-pool line for an eligible category), it is appended after
/// the frame; when it is `None` — approvals, criticals, or a failed/slow
/// generator — the frame stands alone. When a substantive body arrived (an
/// agent's real final message), it is read instead, bounded to the read cap.
fn finalize_decision_speech(
    notification: &NotificationEnvelope,
    decision: &mut AudioDecision,
    config: &AudioConfig,
    state: &Arc<Mutex<Runtime>>,
    personality: &Option<String>,
) {
    let extra = notification
        .extra
        .clone()
        .unwrap_or_default();
    let book = state
        .lock()
        .ok()
        .map(|runtime| runtime.phrases.clone())
        .unwrap_or_default();
    let cap = usize::from(config.phrase_word_cap.max(1));

    // Read a substantive body verbatim (sanitized) up to the read cap.
    let read_min = usize::from(config.speech_read_min_words);
    let read_cap = usize::from(config.speech_read_cap.clamp(1, 160));
    // Approvals skip the verbatim-body path: a completion lead-in
    // ("{app} done in {repo}") is wrong for a decision request, and the
    // approval branch of finalize_speech already composes who/what/where.
    if read_min > 0 && decision.category != "approval" && !notification.body.trim().is_empty() {
        let body_words = notification.body.split_whitespace().count();
        if body_words >= read_min {
            let text = task_fabric_svn_work_core::notification_audio::sanitize_n(&notification.body, read_cap);
            if !text.is_empty() {
                // A real message is always worth speaking, even when the
                // classifier (e.g. success) would otherwise stay quiet.
                decision.speak = true;
                let lead_in = task_fabric_svn_work_core::notification_audio::persona_leadin(
                    &notification.app,
                    &task_fabric_svn_work_core::notification_audio::ContextVars {
                        repo: notification.workspace.as_deref(),
                        tool: None,
                        duration: notification.duration.as_deref(),
                        time_of_day: notification.time_of_day.as_deref(),
                    },
                );
                decision.speech = if lead_in.is_empty() {
                    text
                } else {
                    format!("{lead_in} {text}")
                };
                if config.mode == "muted" {
                    decision.speak = false;
                    decision.speech.clear();
                }
                return;
            }
        }
    }

    let ctx = SpeechContext {
        id: &notification.id,
        app: &notification.app,
        summary: &notification.summary,
        body: &notification.body,
        extra: &extra,
        workspace: notification.workspace.as_deref().unwrap_or(""),
        details: notification.details.as_deref().unwrap_or(""),
        book: Some(&book),
        word_cap: cap,
    };
    decision.speech = finalize_speech(decision, &ctx);
    // A below-threshold body that still names what the notification is about
    // (the `extra` hint the agent bridges attach) composes a real phrase and
    // is worth speaking — e.g. Antigravity's "finished: <plan title>". A bare
    // "{app}" phrase (a content-less completion like the "Ready for your
    // input." fallback) stays on the earcon only.
    let extra_content = !sanitize(&extra).is_empty();
    if extra_content {
        decision.speak = true;
    }
    // Phase B bookend: personality is appended after the deterministic
    // frame, never interleaved with it. Approvals and criticals never
    // arrive here with a clause (apply_humor returns None for them); a
    // failed or slow generator likewise yields None, so the frame stands
    // alone.
    if let Some(clause) = personality.as_deref().map(str::trim).filter(|c| !c.is_empty()) {
        if decision.speech.trim().is_empty() {
            decision.speech = clause.to_string();
        } else {
            decision.speech = format!("{} {}", decision.speech.trim_end(), clause);
        }
        decision.speech = task_fabric_svn_work_core::notification_audio::bound_phrase(&decision.speech, cap);
        decision.speak = true;
    }
    if config.mode == "muted" {
        decision.speak = false;
        decision.speech.clear();
    }
}
fn effective_volume(value: u16, boost_approved: bool) -> f32 {
    f32::from(value.min(if boost_approved { 150 } else { 100 })) / 100.0
}
fn expand_home(value: &str) -> String {
    let home = std::env::var("HOME").unwrap_or_default();
    value.replace("%h", &home)
}
fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// State dir for the always-on decision ledger. The systemd unit grants
/// read/write on ~/.local/state/task-fabric-svn, so this path is writable
/// under the service's sandbox.
fn decision_log_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_default();
    PathBuf::from(format!("{home}/.local/state/task-fabric-svn/soundd.log"))
}

fn decision_log_line(
    notification: &NotificationEnvelope,
    decision: &AudioDecision,
    personality: &Option<String>,
) -> String {
    serde_json::json!({
        "ts": now_ms(),
        "id": notification.id,
        "app": notification.app,
        "body_len": notification.body.len(),
        "has_task_fabric_svn_hints": notification.task_fabric_svn.is_some(),
        "category": decision.category,
        "earcon": decision.earcon,
        "speak": decision.speak,
        "suppressed": decision.suppressed,
        "suppression_reason": decision.suppression_reason,
        "need": format!("{:?}", decision.need),
        "attention": format!("{:?}", decision.attention_level),
        "humor": personality.is_some(),
        "speech_len": decision.speech.len(),
        "speech": decision.speech,
    })
    .to_string()
}

/// Append one envelope decision line to the ledger, rotating the file once it
/// outgrows a cap (current -> .1, older copies dropped). A single append-open
/// per line keeps concurrent handler threads safe without holding a handle.
fn log_decision(line: &str) {
    const MAX_LOG_BYTES: u64 = 5 * 1024 * 1024;
    let path = decision_log_path();
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if fs::metadata(&path)
        .map(|m| m.len() > MAX_LOG_BYTES)
        .unwrap_or(false)
    {
        let rotated = PathBuf::from(format!("{}.1", path.display()));
        let _ = fs::remove_file(&rotated);
        let _ = fs::rename(&path, &rotated);
    }
    if let Ok(mut file) = fs::OpenOptions::new().create(true).append(true).open(&path) {
        let _ = writeln!(file, "{line}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_config_path(tag: &str) -> PathBuf {
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        std::env::temp_dir().join(format!(
            "task-fabric-svn-soundd-config-test-{}-{}-{}.toml",
            std::process::id(),
            n,
            tag
        ))
    }

    fn write_config(path: &Path, body: &str) {
        fs::write(path, body).unwrap();
    }

    #[test]
    fn config_reload_swaps_on_valid_change() {
        let path = temp_config_path("swap");
        write_config(&path, "humor_kind = \"playful\"\n");
        let mut runtime = Runtime::default();
        runtime.maybe_reload_config(&path);
        assert!(matches!(
            runtime.cached_config.as_ref().unwrap().humor_kind,
            HumorKind::Playful
        ));
        // Force a re-read regardless of mtime granularity.
        runtime.cached_config_mtime = Some(UNIX_EPOCH);
        write_config(&path, "humor_kind = \"noir\"\n");
        runtime.maybe_reload_config(&path);
        assert!(matches!(
            runtime.cached_config.as_ref().unwrap().humor_kind,
            HumorKind::Noir
        ));
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn config_reload_keeps_last_known_good_on_malformed_toml() {
        let path = temp_config_path("malformed");
        write_config(&path, "humor_kind = \"blunt\"\n");
        let mut runtime = Runtime::default();
        runtime.maybe_reload_config(&path);
        assert!(matches!(
            runtime.cached_config.as_ref().unwrap().humor_kind,
            HumorKind::Blunt
        ));
        runtime.cached_config_mtime = Some(UNIX_EPOCH);
        write_config(&path, "humor_kind = [unclosed\nmode = ");
        runtime.maybe_reload_config(&path);
        assert!(matches!(
            runtime.cached_config.as_ref().unwrap().humor_kind,
            HumorKind::Blunt
        ));
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn config_reload_keeps_last_known_good_on_invalid_values() {
        let path = temp_config_path("invalid");
        write_config(&path, "mode = \"full\"\nhumor_failure_rate = 25\n");
        let mut runtime = Runtime::default();
        runtime.maybe_reload_config(&path);
        assert_eq!(runtime.cached_config.as_ref().unwrap().humor_failure_rate, 25);
        // Out-of-range percentage is rejected.
        runtime.cached_config_mtime = Some(UNIX_EPOCH);
        write_config(&path, "mode = \"full\"\nhumor_failure_rate = 200\n");
        runtime.maybe_reload_config(&path);
        assert_eq!(runtime.cached_config.as_ref().unwrap().humor_failure_rate, 25);
        // Unknown mode is rejected.
        runtime.cached_config_mtime = Some(UNIX_EPOCH);
        write_config(&path, "mode = \"party\"\n");
        runtime.maybe_reload_config(&path);
        assert_eq!(runtime.cached_config.as_ref().unwrap().humor_failure_rate, 25);
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn config_reload_keeps_last_known_good_when_file_deleted() {
        let path = temp_config_path("deleted");
        write_config(&path, "humor_kind = \"haiku\"\n");
        let mut runtime = Runtime::default();
        runtime.maybe_reload_config(&path);
        assert!(matches!(
            runtime.cached_config.as_ref().unwrap().humor_kind,
            HumorKind::Haiku
        ));
        fs::remove_file(&path).unwrap();
        runtime.cached_config_mtime = Some(UNIX_EPOCH);
        runtime.maybe_reload_config(&path);
        assert!(matches!(
            runtime.cached_config.as_ref().unwrap().humor_kind,
            HumorKind::Haiku
        ));
    }

    #[test]
    fn quip_prompt_uses_fixed_voice_without_persona() {
        let p = quip_prompt("myapp", "repo", "did a thing", "", true);
        assert!(p.contains("witty, humorous"));
        assert!(p.contains("celebrating finishing a major milestone"));
    }

    #[test]
    fn quip_prompt_embodies_persona_for_milestones() {
        let p = quip_prompt("myapp", "repo", "did a thing", "a grumpy old sailor", true);
        assert!(p.contains("You are a grumpy old sailor."));
        assert!(p.contains("in that voice"));
        assert!(p.contains("celebrating finishing a major milestone"));
        assert!(!p.contains("witty, humorous"));
    }

    #[test]
    fn quip_prompt_reacts_briefly_for_persona_non_milestones() {
        let p = quip_prompt("myapp", "repo", "did a thing", "a grumpy old sailor", false);
        assert!(p.contains("You are a grumpy old sailor."));
        assert!(p.contains("reacting briefly to this update"));
        assert!(!p.contains("major milestone"));
    }

    #[test]
    fn persona_for_prompt_keeps_full_persona() {
        let mut config = AudioConfig::default();
        config.humor_persona = "In Blood Meridian Judge Holden speaks as a chilling paradox of eloquence and horror, learned and theatrical and utterly dominant".to_string();
        let persona = persona_for_prompt(&config);
        assert_eq!(persona, config.humor_persona.trim());
        assert!(persona.split_whitespace().count() > 12);
        let p = quip_prompt("myapp", "repo", "did a thing", &persona, true);
        assert!(p.contains("chilling paradox of eloquence and horror"));
    }

    #[test]
    fn persona_for_prompt_caps_at_500_chars() {
        let mut config = AudioConfig::default();
        config.humor_persona = "y".repeat(600);
        assert_eq!(persona_for_prompt(&config).chars().count(), 500);
    }

    #[test]
    fn validate_config_caps_persona_length() {
        let mut config = AudioConfig::default();
        assert!(config.humor_persona.is_empty());
        assert!(validate_config(&config));
        config.humor_persona = "x".repeat(500);
        assert!(validate_config(&config));
        config.humor_persona = "x".repeat(501);
        assert!(!validate_config(&config));
    }

    #[test]
    fn parse_config_text_reads_persona() {
        let config =
            parse_config_text("humor_persona = \"a grumpy old sailor\"\n").unwrap();
        assert_eq!(config.humor_persona, "a grumpy old sailor");
        let config = parse_config_text("humor_kind = \"playful\"\n").unwrap();
        assert!(config.humor_persona.is_empty());
    }

    #[test]
    fn attention_budget_suppresses_repeated_speech() {
        let notification = NotificationEnvelope {
            app: "Codex".into(),
            summary: "Approval required".into(),
            ..Default::default()
        };
        let config = AudioConfig {
            max_speech_per_minute: 1,
            ..Default::default()
        };
        let state = Arc::new(Mutex::new(Runtime::default()));
        let first = apply_attention_budget(&notification, classify(&notification), &config, &state);
        assert!(first.speak);
        record_spoken_event(&state);
        let second =
            apply_attention_budget(&notification, classify(&notification), &config, &state);
        assert!(!second.speak);
        assert_eq!(
            second.suppression_reason.as_deref(),
            Some("attention_budget")
        );
    }

    #[test]
    fn humor_preview_is_short_and_only_applies_to_safe_categories() {
        let state = Arc::new(Mutex::new(Runtime::default()));
        let config = AudioConfig {
            humor_kind: HumorKind::Dry,
            humor: "dry".into(),
            ..Default::default()
        };
        let success = NotificationEnvelope {
            id: "humor-preview-test".into(),
            app: "Task Fabric SVN".into(),
            summary: "Task complete".into(),
            ..Default::default()
        };
        let success_decision = classify(&success);
        let personality = apply_humor(&success, &success_decision, &config, &state);
        assert!(personality.is_some(), "playful humor applies to every success");
        assert!(
            personality.unwrap().split_whitespace().count() <= 18,
            "personality clause stays short"
        );

        let approval = NotificationEnvelope {
            id: "humor-preview-approval".into(),
            app: "Codex".into(),
            summary: "Approval required".into(),
            ..Default::default()
        };
        let approval_decision = classify(&approval);
        assert!(
            apply_humor(&approval, &approval_decision, &config, &state).is_none(),
            "approvals never get personality"
        );

        let routine = NotificationEnvelope {
            id: "humor-preview-routine".into(),
            app: "Unknown app".into(),
            summary: "Incomplete output".into(),
            ..Default::default()
        };
        let routine_decision = classify(&routine);
        assert!(
            apply_humor(&routine, &routine_decision, &config, &state).is_none(),
            "non-success routine events get no personality"
        );
    }

    #[test]
    fn previews_do_not_consume_attention_budget() {
        let notification = NotificationEnvelope {
            id: "speech-preview-approval-test".into(),
            app: "Codex".into(),
            summary: "Approval required".into(),
            ..Default::default()
        };
        let mut state = Runtime::default();
        state.speech_events.extend([now_ms(), now_ms(), now_ms()]);
        let state = Arc::new(Mutex::new(state));
        let decision = apply_attention_budget(
            &notification,
            classify(&notification),
            &AudioConfig::default(),
            &state,
        );
        assert!(decision.speak);
    }

    #[test]
    fn playful_humor_applies_to_every_success() {
        let state = Arc::new(Mutex::new(Runtime::default()));
        let config = AudioConfig {
            humor_kind: HumorKind::Playful,
            ..Default::default()
        };
        let mut seen = Vec::new();
        for id in 0..12u64 {
            let n = NotificationEnvelope {
                id: format!("success-{id}"),
                app: "opencode".into(),
                summary: "task finished".into(),
                ..Default::default()
            };
            let mut d = classify(&n);
            d.category = "success".into();
            let personality = apply_humor(&n, &d, &config, &state);
            assert!(personality.is_some(), "playful should always apply id {id}");
            let clause = personality.unwrap();
            assert!(!clause.is_empty());
            seen.push(clause);
        }
        seen.sort();
        seen.dedup();
        assert!(seen.len() >= 3, "expected variety across playful list");
    }

    #[test]
    fn off_humor_yields_no_personality() {
        let state = Arc::new(Mutex::new(Runtime::default()));
        let config = AudioConfig {
            humor_kind: HumorKind::Off,
            ..Default::default()
        };
        let n = NotificationEnvelope {
            id: "success-1".into(),
            app: "opencode".into(),
            summary: "task done".into(),
            ..Default::default()
        };
        let d = classify(&n);
        assert!(
            apply_humor(&n, &d, &config, &state).is_none(),
            "humor off means no personality clause"
        );
    }

    #[test]
    fn failure_humor_only_with_opt_in() {
        let state = Arc::new(Mutex::new(Runtime::default()));
        let config = AudioConfig {
            humor_kind: HumorKind::Playful,
            humor_failure: true,
            ..Default::default()
        };
        let n = NotificationEnvelope {
            id: "blocked-1".into(),
            app: "opencode".into(),
            summary: "build failed".into(),
            ..Default::default()
        };
        let d = classify(&n);
        let personality = apply_humor(&n, &d, &config, &state);
        assert!(personality.is_some(), "opt-in failure humor applies");
        assert!(!personality.unwrap().is_empty());

        let no_opt_in = AudioConfig {
            humor_kind: HumorKind::Playful,
            humor_failure: false,
            ..Default::default()
        };
        let d2 = classify(&n);
        assert!(
            apply_humor(&n, &d2, &no_opt_in, &state).is_none(),
            "no opt-in means no failure personality"
        );
    }

    #[test]
    fn finalize_threads_extra_into_speech() {
        let runtime = Runtime {
            phrases: PhraseBook::default(),
            ..Default::default()
        };
        let state = Arc::new(Mutex::new(runtime));
        let n = NotificationEnvelope {
            id: "x1".into(),
            app: "opencode".into(),
            summary: "Approval required".into(),
            body: "".into(),
            extra: Some("wants to run git push".into()),
            ..Default::default()
        };
        let mut d = classify(&n);
        finalize_decision_speech(&n, &mut d, &AudioConfig::default(), &state, &None);
        assert_eq!(d.speech, "Opencode needs your approval to run git push.");
    }

    #[test]
    fn substantive_body_is_read_instead_of_humor() {
        let runtime = Runtime {
            phrases: PhraseBook::default(),
            ..Default::default()
        };
        let state = Arc::new(Mutex::new(runtime));
        let config = AudioConfig {
            humor_kind: HumorKind::Playful,
            speech_read_min_words: 8,
            speech_read_cap: 30,
            ..Default::default()
        };
        let body = "Refactored the auth middleware to use session scopes and wired the new rate limiter in front of the proxy. All 42 tests pass now.";
        let n = NotificationEnvelope {
            id: "rich-done".into(),
            app: "codex".into(),
            summary: "Turn complete".into(),
            body: body.into(),
            ..Default::default()
        };
        let mut d = classify(&n);
        // Force a success classification like the bridges' task_fabric_svn hint.
        d.category = "success".into();
        let personality = apply_humor(&n, &d, &config, &state);
        assert!(personality.is_none(), "substantive body should defer humor");
        finalize_decision_speech(&n, &mut d, &config, &state, &personality);
        assert!(d.speech.contains("Refactored"), "expected the real message");
        assert!(d.speech.split_whitespace().count() <= 30);
        assert!(!d.speech.contains("dragon"), "no humor replaced the text");
    }

    #[test]
    fn short_body_keeps_humor() {
        let runtime = Runtime {
            phrases: PhraseBook::default(),
            ..Default::default()
        };
        let state = Arc::new(Mutex::new(runtime));
        let config = AudioConfig {
            humor_kind: HumorKind::Playful,
            speech_read_min_words: 8,
            speech_read_cap: 30,
            ..Default::default()
        };
        let n = NotificationEnvelope {
            id: "short-done".into(),
            app: "codex".into(),
            summary: "Turn complete".into(),
            body: "Done.".into(),
            ..Default::default()
        };
        let mut d = classify(&n);
        d.category = "success".into();
        let personality = apply_humor(&n, &d, &config, &state);
        assert!(personality.is_some());
        finalize_decision_speech(&n, &mut d, &config, &state, &personality);
        assert!(d.speech.split_whitespace().count() <= 20);
    }

    #[test]
    fn personality_is_bookended_after_deterministic_frame() {
        let runtime = Runtime {
            phrases: PhraseBook::default(),
            ..Default::default()
        };
        let state = Arc::new(Mutex::new(runtime));
        let config = AudioConfig {
            humor_kind: HumorKind::Playful,
            ..Default::default()
        };
        let n = NotificationEnvelope {
            id: "bookend-1".into(),
            app: "opencode".into(),
            summary: "task finished".into(),
            ..Default::default()
        };
        let mut d = classify(&n);
        d.category = "success".into();
        let personality = apply_humor(&n, &d, &config, &state);
        let clause = personality.expect("playful success earns a personality clause");

        // Frame-only baseline: the deterministic speech with no personality.
        let mut frame_only = classify(&n);
        finalize_decision_speech(&n, &mut frame_only, &config, &state, &None);
        assert!(!frame_only.speech.is_empty());

        // Bookended: frame first and byte-identical, clause trailing.
        let mut bookended = classify(&n);
        finalize_decision_speech(&n, &mut bookended, &config, &state, &Some(clause.clone()));
        assert!(
            bookended.speech.starts_with(frame_only.speech.trim_end()),
            "frame must be unchanged, got: {}",
            bookended.speech
        );
        assert!(
            bookended.speech.trim_end().ends_with(clause.trim()),
            "clause must trail the frame, got: {}",
            bookended.speech
        );
    }

    #[test]
    fn frame_stands_alone_when_personality_none() {
        let runtime = Runtime {
            phrases: PhraseBook::default(),
            ..Default::default()
        };
        let state = Arc::new(Mutex::new(runtime));
        let n = NotificationEnvelope {
            id: "approval-bookend".into(),
            app: "opencode".into(),
            summary: "Approval required".into(),
            body: "".into(),
            extra: Some("wants to run git push".into()),
            ..Default::default()
        };
        let d = classify(&n);
        assert_eq!(d.category, "approval");
        assert!(
            apply_humor(&n, &d, &AudioConfig::default(), &state).is_none(),
            "approvals never get personality"
        );
        let mut out = classify(&n);
        finalize_decision_speech(&n, &mut out, &AudioConfig::default(), &state, &None);
        assert_eq!(out.speech, "Opencode needs your approval to run git push.");
    }

    #[test]
    fn milestone_without_server_is_frame_only() {
        // The generated-quip path must degrade to None and apply_humor must
        // yield no personality, so the deterministic frame stands alone. The
        // discard port is guaranteed closed (the test workstation may run a
        // real quip server on the default :12121).
        let state = Arc::new(Mutex::new(Runtime::default()));
        let config = AudioConfig {
            humor_kind: HumorKind::Playful,
            quip_server_url: "http://127.0.0.1:9".into(),
            ..Default::default()
        };
        let n = NotificationEnvelope {
            id: "milestone-frame-only".into(),
            app: "opencode".into(),
            summary: "task finished".into(),
            duration: Some("180s".into()),
            ..Default::default()
        };
        let mut d = classify(&n);
        d.category = "success".into();
        let quip = generate_milestone_quip(&n, &config, true);
        assert!(quip.is_none(), "no server in tests: quip must degrade to None");
        let personality = apply_humor(&n, &d, &config, &state);
        assert!(
            personality.is_none(),
            "generator down: milestone must be frame-only, no pool clause"
        );
        let mut out = classify(&n);
        out.category = "success".into();
        finalize_decision_speech(&n, &mut out, &config, &state, &personality);
        assert!(
            !out.speech.is_empty(),
            "frame-only speech must still be produced"
        );
    }

    #[test]
    fn strip_emoji_removes_emoji_but_keeps_text() {
        assert_eq!(strip_emoji("done \u{1F3C6}\u{1F4AA}!"), "done !");
        assert_eq!(strip_emoji("plain text, ok."), "plain text, ok.");
        assert_eq!(strip_emoji("\u{1F389} party \u{1F604}"), " party ");
        // Flags are regional-indicator pairs.
        assert_eq!(strip_emoji("go \u{1F1FA}\u{1F1F8}"), "go ");
        // ZWJ sequences collapse with their joiners.
        assert_eq!(strip_emoji("hi \u{1F468}\u{200D}\u{1F4BB}"), "hi ");
    }

    #[test]
    fn clean_generated_quip_strips_emoji() {
        // Short quip with trailing emoji: the words survive, the emoji goes.
        let raw = "Task done and it feels great. \u{1F3C6}\u{1F4AA}";
        let cleaned = clean_generated_quip(raw).expect("quip survives cleaning");
        assert_eq!(cleaned, "Task done and it feels great");
        // Longer quip in the shape the benchmark produced: sanitize trims
        // to 12 tokens and any emoji inside is stripped; none may remain.
        let raw = "Finishing this project was like beating the tar \u{1F389} out of a grizzly bear\u{2014}hard, yet satisfying. \u{1F3C6}\u{1F4AA}";
        let cleaned = clean_generated_quip(raw).expect("quip survives cleaning");
        assert!(
            !cleaned.chars().any(is_emoji),
            "no emoji may remain: {cleaned}"
        );
        assert!(
            !cleaned.contains("  "),
            "whitespace normalized: {cleaned}"
        );
    }

    #[test]
    fn clean_generated_quip_first_sentence_fallback() {
        // Ramble past the cap: the first complete sentence survives.
        let raw = "Toss the hammer down and begin the next task. Got it, mate, now hoist the sails and set the course for open water today!";
        assert_eq!(
            clean_generated_quip(raw).as_deref(),
            Some("Toss the hammer down and begin the next task")
        );
        // Meta-commentary after the quip is dropped, not spoken.
        let raw = "The construction is now complete. Good, now off you go! Sure, here is a one-sentence quip in my grumpy old sailor voice";
        assert_eq!(
            clean_generated_quip(raw).as_deref(),
            Some("The construction is now complete")
        );
        // Fragment cut off mid-sentence by the token limit: no terminal
        // punctuation, so it is rejected (frame-only speech).
        let raw = "Well that is a lot of ink I have managed to squander on the quarter but I will be damned if I am not going to";
        assert_eq!(clean_generated_quip(raw), None);
        // First "sentence" too short to be a real quip (e.g. abbreviation):
        // rejected rather than speaking a fragment.
        let raw = "e.g. this is a rambling quip that goes on and on past the word cap without ever really saying anything at all";
        assert_eq!(clean_generated_quip(raw), None);
        // Short quip without terminal punctuation still speaks
        // (sanitize() strips edge punctuation from each token).
        assert_eq!(
            clean_generated_quip("Aye, the build is done").as_deref(),
            Some("Aye the build is done")
        );
    }

    #[test]
    fn should_enqueue_respects_dry_run() {
        let mut n = NotificationEnvelope::default();
        let mut d = classify(&n);
        let config = AudioConfig::default();
        d.suppressed = false;
        assert!(should_enqueue(&n, &d, &config), "normal notification enqueues");
        n.dry_run = true;
        assert!(
            !should_enqueue(&n, &d, &config),
            "dry-run preview must never reach the speakers"
        );
        n.dry_run = false;
        d.suppressed = true;
        assert!(
            !should_enqueue(&n, &d, &config),
            "suppressed decision still does not enqueue"
        );
    }

    #[test]
    fn clean_generated_quip_rejects_nothing_speakable() {
        assert!(clean_generated_quip("\u{1F3C6}\u{1F4AA}").is_none());
        assert!(clean_generated_quip("   ").is_none());
    }

    #[test]
    fn length_plus_config_orchestrates_read() {
        // Ensure the read gate fires only when enabled.
        let runtime = Runtime::default();
        let state = Arc::new(Mutex::new(runtime));
        let config = AudioConfig {
            speech_read_min_words: 0,
            ..Default::default()
        };
        let n = NotificationEnvelope {
            id: "x".into(),
            app: "codex".into(),
            summary: "Turn complete".into(),
            body: "some very long body that should not be read because reading is disabled here indeed".into(),
            ..Default::default()
        };
        let mut d = classify(&n);
        d.category = "success".into();
        let personality = apply_humor(&n, &d, &config, &state);
        finalize_decision_speech(&n, &mut d, &config, &state, &personality);
        // With reading disabled and humor off, speech stays the canned line.
        assert!(d.speech.split_whitespace().count() <= 20);
    }

    #[test]
    #[test]
    fn speak_command_validates_text_and_voice() {
        let ok = serde_json::json!({"command": "speak", "text": "Memento mori.", "voice": "am_michael"});
        assert_eq!(
            validate_speak_request(&ok),
            Ok(("Memento mori.".to_string(), "am_michael".to_string()))
        );
        // Empty voice is allowed: the caller resolves the default.
        let no_voice = serde_json::json!({"command": "speak", "text": "Memento mori."});
        assert_eq!(
            validate_speak_request(&no_voice),
            Ok(("Memento mori.".to_string(), String::new()))
        );
        let empty = serde_json::json!({"command": "speak", "text": "   "});
        assert!(validate_speak_request(&empty).is_err());
        let missing = serde_json::json!({"command": "speak"});
        assert!(validate_speak_request(&missing).is_err());
        let bad_voice = serde_json::json!({"command": "speak", "text": "hi", "voice": "xx_nope"});
        let err = validate_speak_request(&bad_voice).unwrap_err();
        assert!(err.contains("unknown kokoro voice"));
        let long = serde_json::json!({"command": "speak", "text": "x".repeat(2001)});
        assert!(validate_speak_request(&long).is_err());
    }

    #[test]
    fn profile_metadata_and_resolved_voice_round_trip() {
        let notification = NotificationEnvelope {
            profile_id: Some("holden".into()),
            profile_priority: 20,
            resolved_voice: Some("am_onyx".into()),
            resolved_voice_source: Some("global_override".into()),
            speech_queue_policy: Some("priority".into()),
            ..Default::default()
        };
        let encoded = serde_json::to_string(&notification).unwrap();
        let decoded: NotificationEnvelope = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded.profile_id.as_deref(), Some("holden"));
        assert_eq!(decoded.profile_priority, 20);
        assert_eq!(decoded.resolved_voice.as_deref(), Some("am_onyx"));
        assert_eq!(decoded.resolved_voice_source.as_deref(), Some("global_override"));
    }

    fn kokoro_voice_resolves_per_app() {
        let mut voices = HashMap::new();
        voices.insert("claude".into(), "af_heart".into());
        voices.insert("codex".into(), "am_adam".into());
        voices.insert("opencode".into(), "af_bella".into());
        voices.insert("gemini".into(), "af_sky".into());

        let config = AudioConfig {
            kokoro_voice: "af_heart".into(),
            kokoro_voices_by_app: voices,
            ..Default::default()
        };

        assert_eq!(
            config.kokoro_voices_by_app.get("codex").map(String::as_str),
            Some("am_adam")
        );
        assert_eq!(
            config.kokoro_voices_by_app.get("claude").map(String::as_str),
            Some("af_heart")
        );
        assert_eq!(
            config.kokoro_voices_by_app.get("opencode").map(String::as_str),
            Some("af_bella")
        );
        assert_eq!(
            config.kokoro_voices_by_app.get("gemini").map(String::as_str),
            Some("af_sky")
        );
        assert_eq!(
            config.kokoro_voices_by_app.get("unknown").map(String::as_str).unwrap_or(&config.kokoro_voice),
            "af_heart"
        );
    }

    #[test]
    fn kokoro_voice_precedence_override_then_app_then_category() {
        let mut by_app = HashMap::new();
        by_app.insert("codex".into(), "am_adam".into());
        let mut by_category = HashMap::new();
        by_category.insert("approval".into(), "af_bella".into());

        let config = AudioConfig {
            kokoro_voice: "af_heart".into(),
            kokoro_voices_by_app: by_app,
            kokoro_voices_by_category: by_category,
            ..Default::default()
        };

        // Per-app persona wins over per-category.
        assert_eq!(resolve_kokoro_voice(&config, "Codex", "approval"), "am_adam");
        // Category fills in for apps with no persona.
        assert_eq!(resolve_kokoro_voice(&config, "kind-darwin", "approval"), "af_bella");
        // Default fallback when neither layer applies.
        assert_eq!(resolve_kokoro_voice(&config, "kind-darwin", "success"), "af_heart");
        // App lookup is case-insensitive.
        assert_eq!(resolve_kokoro_voice(&config, "CODEX", "success"), "am_adam");

        // Global override beats every layer.
        let overridden = AudioConfig {
            kokoro_voice_override: "af_sky".into(),
            ..config
        };
        assert_eq!(resolve_kokoro_voice(&overridden, "Codex", "approval"), "af_sky");
        assert_eq!(resolve_kokoro_voice(&overridden, "kind-darwin", "success"), "af_sky");
    }

    #[test]
    fn poisoned_play_lock_recovers_instead_of_muting() {
        let lock = Arc::new(Mutex::new(()));
        // Poison the lock the way a panicking play worker would: a guard
        // dropped during unwind marks the mutex poisoned.
        let clone = Arc::clone(&lock);
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
            let _guard = clone.lock().unwrap();
            panic!("simulated play worker panic");
        }));
        assert!(lock.is_poisoned());
        // acquire_play_lock must recover a usable guard (play proceeds) rather
        // than give up and mute. The poison flag stays set — every later
        // lock() still returns Err and into_inner() recovers again — which is
        // exactly the resilience we want.
        let guard = acquire_play_lock(&lock);
        drop(guard);
        drop(acquire_play_lock(&lock));
    }

    #[test]
    fn short_success_with_extra_speaks_the_phrase() {
        let runtime = Runtime {
            phrases: PhraseBook::default(),
            ..Default::default()
        };
        let state = Arc::new(Mutex::new(runtime));
        let config = AudioConfig {
            speech_read_min_words: 8,
            ..Default::default()
        };
        // Antigravity-style: short body below the read threshold, but the
        // bridge attaches an `extra` hint naming what finished.
        let n = NotificationEnvelope {
            id: "short-extra-done".into(),
            app: "Antigravity".into(),
            summary: "[ExampleField] Antigravity finished".into(),
            body: "Voice Session V1 Contract & Implementation Plan".into(),
            extra: Some("Voice Session V1 Contract & Implementation Plan".into()),
            task_fabric_svn: Some(task_fabric_svn_work_core::notification_audio::TaskFabricSvnPresentation {
                category: Some("success".into()),
                need: Some("work completed".into()),
                context: None,
                outcome: Some("completed".into()),
            }),
            ..Default::default()
        };
        let mut d = classify(&n);
        let personality = apply_humor(&n, &d, &config, &state);
        finalize_decision_speech(&n, &mut d, &config, &state, &personality);
        assert!(d.speak, "extra content should read aloud below the body threshold");
        assert!(d.speech.contains("Voice Session"), "phrase should carry the extra content");
    }

    #[test]
    fn short_success_without_content_stays_quiet() {
        let runtime = Runtime {
            phrases: PhraseBook::default(),
            ..Default::default()
        };
        let state = Arc::new(Mutex::new(runtime));
        let config = AudioConfig {
            speech_read_min_words: 8,
            ..Default::default()
        };
        // "Ready for your input." fallback: no extra, body below threshold.
        let n = NotificationEnvelope {
            id: "short-empty-done".into(),
            app: "antigravity".into(),
            summary: "[ExampleField] Antigravity finished".into(),
            body: "Ready for your input.".into(),
            task_fabric_svn: Some(task_fabric_svn_work_core::notification_audio::TaskFabricSvnPresentation {
                category: Some("success".into()),
                need: Some("work completed".into()),
                context: None,
                outcome: Some("completed".into()),
            }),
            ..Default::default()
        };
        let mut d = classify(&n);
        let personality = apply_humor(&n, &d, &config, &state);
        finalize_decision_speech(&n, &mut d, &config, &state, &personality);
        assert!(!d.speak, "a content-less completion stays on the earcon only");
    }

    #[test]
    fn decision_log_line_is_json_and_carries_decision() {
        let runtime = Runtime {
            phrases: PhraseBook::default(),
            ..Default::default()
        };
        let state = Arc::new(Mutex::new(runtime));
        let n = NotificationEnvelope {
            id: "log-me".into(),
            app: "opencode".into(),
            summary: "Approval required".into(),
            body: "".into(),
            ..Default::default()
        };
        let mut d = classify(&n);
        finalize_decision_speech(&n, &mut d, &AudioConfig::default(), &state, &None);
        let line = decision_log_line(&n, &d, &None);
        let parsed: serde_json::Value = serde_json::from_str(&line).expect("valid JSON line");
        assert_eq!(parsed["id"], "log-me");
        assert_eq!(parsed["app"], "opencode");
        assert_eq!(parsed["category"], d.category);
        assert_eq!(parsed["speak"], d.speak);
        assert!(parsed["ts"].is_u64());
    }
}
