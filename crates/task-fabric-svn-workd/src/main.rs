//! Owner-local Worldstate daemon. The socket remains newline based and keeps
//! compatibility with the original status/timeline/project/select requests.

use task_fabric_svn_work_core::horizon::{self, HorizonRequest, Operation, PrivacyConfig};
use task_fabric_svn_work_core::{
    attention_alert::{self, AlertState, DEFAULT_ALERT_COOLDOWN_SECONDS},
    companion::{
        CompanionActionKind, CompanionAttentionLevel, CompanionEventCategory, CompanionEventNeed,
        CompanionEventV1,
    },
    default_config_path, default_socket_path, now_epoch_seconds, worldstate, AttentionItem,
    Project, ProjectConfig, StatusSnapshot, StatusSource,
};
use task_fabric_svn_work_core::experiment;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

struct HorizonRuntime {
    revision: u64,
    ids: HashMap<String, String>,
    capability: Option<(String, String, u64, u64)>,
}
fn safe_transition_id(id: &str) -> bool { !id.is_empty() && id.len() <= worldstate::MAX_STRING && !id.contains('/') && !id.contains('\\') && !id.contains("..") }
fn safe_trace_id(id: &str) -> bool { !id.is_empty() && id.len() <= worldstate::MAX_STRING && !id.chars().any(|c| c.is_control()) }

fn transition_request(target: &str, parsed: &Value, transition_id: &str, trace_id: &str) -> Value {
    json!({
        "world_id": target,
        "transition_id": transition_id,
        "trace_id": trace_id,
        "intent": parsed.get("intent").cloned().unwrap_or(Value::Null),
    })
}

fn main() {
    let (socket, config) = parse_args();
    if let Err(error) = run(&socket, &config) {
        eprintln!("task-fabric-svn-workd: {error}");
        std::process::exit(1);
    }
}

fn parse_args() -> (PathBuf, PathBuf) {
    let mut socket = default_socket_path();
    let mut config = default_config_path();
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--socket" => {
                if let Some(v) = args.next() {
                    socket = v.into()
                }
            }
            "--config" => {
                if let Some(v) = args.next() {
                    config = v.into()
                }
            }
            _ => {}
        }
    }
    (socket, config)
}

fn run(socket: &PathBuf, config_path: &PathBuf) -> std::io::Result<()> {
    if let Some(parent) = socket.parent() {
        fs::create_dir_all(parent)?;
    }
    if socket.exists() {
        fs::remove_file(socket)?;
    }
    let listener = UnixListener::bind(socket)?;
    let state = worldstate::state_dir();
    fs::create_dir_all(&state)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&state, fs::Permissions::from_mode(0o700))?;
    }
    let alerts = Arc::new(Mutex::new(load_alert_state(
        &state.join("attention-alerts.json"),
    )));
    let lock = Arc::new(Mutex::new(false));
    let horizon = Arc::new(Mutex::new(HorizonRuntime {
        revision: 0,
        ids: HashMap::new(),
        capability: None,
    }));
    eprintln!("task-fabric-svn-workd listening on {}", socket.display());
    // Publish every registered World once at daemon startup.  Runtime owns
    // the cache; a failed connection is intentionally harmless and retried
    // by the next meaningful status/transition request.
    if let Ok((config, _)) = ProjectConfig::from_path_or_example(config_path) {
        for project in &config.projects { publish_semantic_context(&config, &project.id, &state); }
    }
    start_idle_inhibitor(config_path.clone(), state.clone());
    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let c = config_path.clone();
                let s = state.clone();
                let a = Arc::clone(&alerts);
                let l = Arc::clone(&lock);
                let h = Arc::clone(&horizon);
                thread::spawn(move || handle(stream, &c, &s, a, l, h));
            }
            Err(e) => eprintln!("task-fabric-svn-workd: connection failed: {e}"),
        }
    }
    Ok(())
}

fn handle(
    mut stream: UnixStream,
    config_path: &PathBuf,
    state: &PathBuf,
    alerts: Arc<Mutex<AlertState>>,
    lock: Arc<Mutex<bool>>,
    horizon: Arc<Mutex<HorizonRuntime>>,
) {
    let mut line = String::new();
    let ok = BufReader::new(&mut stream).read_line(&mut line).is_ok();
    let req = line.trim();
    let response = if !ok {
        json!({"error":"empty request"})
    } else {
        match ProjectConfig::from_path_or_example(config_path) {
            Ok((mut config, example)) => {
                observe_attention(&config, config_path, example, &alerts, state);
                dispatch(
                    req,
                    &mut config,
                    config_path,
                    example,
                    state,
                    &lock,
                    &horizon,
                )
            }
            Err(e) => json!({"error":e.to_string()}),
        }
    };
    let mut bytes = serde_json::to_vec(&response)
        .unwrap_or_else(|_| b"{\"error\":\"encoding failed\"}".to_vec());
    bytes.push(b'\n');
    let _ = stream.write_all(&bytes);
}

fn dispatch(
    req: &str,
    config: &mut ProjectConfig,
    config_path: &PathBuf,
    example: bool,
    state: &PathBuf,
    lock: &Arc<Mutex<bool>>,
    horizon: &Arc<Mutex<HorizonRuntime>>,
) -> Value {
    if req.trim_start().starts_with('{') {
        let Ok(request) = serde_json::from_str::<HorizonRequest>(req) else {
            return json!({"error":"invalid horizon request"});
        };
        return dispatch_horizon(request, config, config_path, state, horizon);
    }
    if req == "status" {
        let selected = resolve_active_project_id(config, load_world_id(&state.join("selected-world")));
        let active = resolve_active_project_id(config, load_active_project(&state.join("active-project")));
        let requested = resolve_active_project_id(config, load_world_id(&state.join("requested-world")));
        let last = load_last_transition(state);
        let world_snapshot = worldstate::snapshot_for_with_state(
            config,
            selected.as_deref(),
            requested.as_deref(),
            active.as_deref(),
            None,
            last.clone(),
        );
        let mut status = StatusSnapshot::local_for_project(
            config,
            config_path,
            example,
            active.as_deref().or(selected.as_deref()),
        ).with_source(StatusSource::Daemon);
        status.selected_world_id = selected;
        status.requested_world_id = requested;
        status.active_world_id = active;
        status.world_activation_status = world_snapshot.world_activation_status;
        status.active_trace_id = world_snapshot.active_trace_id;
        status.last_transition_id = world_snapshot.last_transition.as_ref().map(|reference| reference.id.clone());
        status.degraded.extend(world_snapshot.degraded_reasons);
        return serde_json::to_value(status)
        .unwrap_or_else(|_| json!({"error":"could not encode status"}));
    }
    if req == "worldstate" {
        let selected = resolve_active_project_id(config, load_world_id(&state.join("selected-world")));
        let requested = resolve_active_project_id(config, load_world_id(&state.join("requested-world")));
        let active = resolve_active_project_id(config, load_active_project(&state.join("active-project")));
        return serde_json::to_value(worldstate::snapshot_for_with_state(
            config,
            selected.as_deref(),
            requested.as_deref(),
            active.as_deref(),
            None,
            load_last_transition(state),
        ))
        .unwrap_or_else(|_| json!({"error":"could not encode worldstate"}));
    }
    if req == "study-context" {
        let id = config.first_project().map(|p| p.id.as_str());
        return serde_json::to_value(worldstate::study_context_for(config, id))
            .unwrap_or_else(|_| json!({"error":"could not encode study context"}));
    }
    if let Some(id) = req.strip_prefix("experiment-status ") {
        if id.is_empty()
            || id.len() > 128
            || !id
                .chars()
                .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.'))
        {
            return json!({"error":"invalid experiment id"});
        }
        return match experiment::load_record(state, id) {
            Ok(Some(record)) => serde_json::to_value(record)
                .unwrap_or_else(|_| json!({"error":"could not encode experiment"})),
            Ok(None) => json!({"error":"experiment not found"}),
            Err(error) => json!({"error":error.to_string()}),
        };
    }
    if let Some(id) = req.strip_prefix("semantic-context ") {
        return worldstate::semantic_context_with_baseline(config, Some(id), state);
    }
    if let Some(id) = req.strip_prefix("world ") {
        let mut selected = config.clone();
        selected.projects.retain(|p| {
            p.id.eq_ignore_ascii_case(id) || p.aliases.iter().any(|a| a.eq_ignore_ascii_case(id))
        });
        if selected.projects.is_empty() {
            return json!({"error":"world not found"});
        }
        let inspected = selected.first_project().map(|p| p.id.clone());
        let selected_id = resolve_active_project_id(config, load_world_id(&state.join("selected-world")));
        let requested_id = resolve_active_project_id(config, load_world_id(&state.join("requested-world")));
        let active_id = resolve_active_project_id(config, load_active_project(&state.join("active-project")));
        return serde_json::to_value(worldstate::snapshot_for_with_state(
            &selected,
            inspected.as_deref().or(selected_id.as_deref()),
            requested_id.as_deref().or(inspected.as_deref()),
            active_id.as_deref(),
            None,
            load_last_transition(state),
        ))
        .unwrap_or_else(|_| json!({"error":"could not encode world"}));
    }
    if let Some(raw) = req.strip_prefix("world-events") {
        let limit = raw
            .split_whitespace()
            .next()
            .and_then(|v| v.parse().ok())
            .unwrap_or(100);
        return json!({"schema_version":1,"events":worldstate::read_events(limit).unwrap_or_default()});
    }
    if let Some(id) = req.strip_prefix("transition-plan ") {
        if id.contains('/') || id.len() > worldstate::MAX_STRING {
            return json!({"error":"invalid transition id"});
        }
        return read_json_file(
            &state
                .join("world-transitions/plans")
                .join(format!("{id}.json")),
        );
    }
    if let Some(id) = req.strip_prefix("transition-receipt ") {
        return match worldstate::read_receipt(id) {
            Ok(Some(v)) => serde_json::to_value(v)
                .unwrap_or_else(|_| json!({"error":"could not encode receipt"})),
            Ok(None) => json!({"error":"transition receipt not found"}),
            Err(e) => json!({"error":e.to_string()}),
        };
    }
    if let Some(id) = req.strip_prefix("transition-status ") {
        if !safe_transition_id(id) { return json!({"error":"invalid transition id"}); }
        return read_json_file(&state.join("world-transitions/status").join(format!("{id}.json")));
    }
    if let Some(id) = req.strip_prefix("transition-enter ") {
        if !try_lock(lock) {
            return json!({"error":"transition_in_progress"});
        }
        if let Some(error) = transition_preflight_error(config) { if let Ok(mut g)=lock.lock(){*g=false;} return json!({"status":"rejected","error":error,"effects":[]}); }
        let parsed: Value = serde_json::from_str(id).unwrap_or_else(|_| json!({"world_id":id}));
        let target = parsed.get("world_id").and_then(Value::as_str).unwrap_or("").to_string();
        let world_ok = config.projects.iter().any(|p| p.id.eq_ignore_ascii_case(&target) || p.aliases.iter().any(|a| a.eq_ignore_ascii_case(&target)));
        if !world_ok { if let Ok(mut g)=lock.lock(){*g=false;} return json!({"status":"rejected","error":"world not found","effects":[]}); }
        let transition_id=worldstate::new_id("transition");
        let trace_id=parsed.get("trace_id").and_then(Value::as_str).filter(|value| safe_trace_id(value)).map(str::to_string).unwrap_or_else(worldstate::new_trace_id);
        let intent_id = parsed.get("intent").and_then(|value| value.get("id")).and_then(Value::as_str);
        if let Err(e)=worldstate::atomic_write(&state.join("world-transitions/status").join(format!("{transition_id}.json")), &json!({"transition_id":transition_id,"trace_id":trace_id,"world_id":target.clone(),"intent_id":intent_id,"status":"accepted","terminal":false})) { if let Ok(mut g)=lock.lock(){*g=false;} return json!({"status":"rejected","error":format!("could not persist accepted transition: {e}")}); }
        let request=transition_request(&target, &parsed, &transition_id, &trace_id).to_string(); let cfg=config.clone(); let path=config_path.clone(); let st=state.clone(); let lock2=Arc::clone(lock);
        std::thread::spawn(move || { let _=execute_transition(&request,&cfg,&path,example,&st); if let Ok(mut g)=lock2.lock(){*g=false;} });
        return json!({"status":"accepted","transition_id":transition_id,"trace_id":trace_id,"requested_world_id":target});
    }
    if let Some(id) = req.strip_prefix("transition-dry-run ") {
        if let Some(error) = transition_preflight_error(config) { return json!({"status":"rejected","error":error,"effects":[]}); }
        let Some(world) = config.projects.iter().find(|project| {
            project.id.eq_ignore_ascii_case(id)
                || project.aliases.iter().any(|alias| alias.eq_ignore_ascii_case(id))
        }) else {
            return json!({"error":"world not found"});
        };
        let active = resolve_active_project_id(config, load_active_project(&state.join("active-project")));
        let clients = hypr_json("clients").as_array().cloned();
        let report = worldstate::simulate_transition(world, active.as_deref(), clients.as_deref());
        return serde_json::to_value(report)
            .unwrap_or_else(|_| json!({"error":"could not encode simulation report"}));
    }
    if req == "timeline" {
        let events = config
            .first_project()
            .map(|p| task_fabric_svn_work_core::durable::read_events_for(&p.id).unwrap_or_default())
            .unwrap_or_default();
        return json!({"schema_version":1,"events":events,"world_events":worldstate::read_events(100).unwrap_or_default()});
    }
    if let Some(id) = req.strip_prefix("select ") {
        if select_project(config, id) {
            let selected_id = config.projects.iter().find(|project| {
                project.id.eq_ignore_ascii_case(id)
                    || project.aliases.iter().any(|alias| alias.eq_ignore_ascii_case(id))
            }).map(|project| project.id.clone());
            match selected_id.as_deref()
                .ok_or_else(|| std::io::Error::other("project not found"))
                .and_then(|id| save_world_id(&state.join("selected-world"), id))
            {
                Ok(()) => json!({
                    "status":"selected",
                    "project_id":selected_id,
                    "active_world_id":resolve_active_project_id(config, load_active_project(&state.join("active-project"))),
                    "world_activation_status":"selection_pending"
                }),
                Err(e) => json!({"error":e.to_string()}),
            }
        } else {
            json!({"error":"project not found"})
        }
    } else if let Some(id) = req.strip_prefix("project ") {
        let mut selected = config.clone();
        selected.projects.retain(|p| {
            p.id.eq_ignore_ascii_case(id) || p.aliases.iter().any(|a| a.eq_ignore_ascii_case(id))
        });
        if selected.projects.is_empty() {
            json!({"error":"project not found"})
        } else {
            serde_json::to_value(
                StatusSnapshot::local(&selected, config_path, example)
                    .with_source(StatusSource::Daemon)
                    .project,
            )
            .unwrap_or_else(|_| json!({"error":"could not encode project"}))
        }
    } else {
        json!({"error":"unsupported request"})
    }
}

fn transition_preflight_error(config: &ProjectConfig) -> Option<String> {
    config.validate().into_iter().find(|e| e.contains("resume") || e.contains("dependency") || e.contains("unsupported"))
}

fn publish_semantic_context(config: &ProjectConfig, world_id: &str, state: &Path) {
    let Some(socket) = std::env::var_os("TASK_FABRIC_SVN_RUNTIME_SOCKET").map(PathBuf::from).or_else(|| std::env::var_os("XDG_RUNTIME_DIR").map(|path| PathBuf::from(path).join("task_fabric_svn/runtime.sock"))) else { return; };
    let context = worldstate::semantic_context_with_baseline(config, Some(world_id), state);
    let Ok(mut stream) = UnixStream::connect(socket) else { return; };
    let _ = stream.set_write_timeout(Some(std::time::Duration::from_millis(250)));
    let Ok(payload) = serde_json::to_vec(&serde_json::json!({"kind":"semantic_context_upsert","context":context})) else { return; };
    if stream.write_all(&payload).is_ok() && stream.write_all(b"\n").is_ok() {
        let _ = stream.shutdown(std::net::Shutdown::Write);
        worldstate::record_semantic_baseline(&context, state);
    }
}

fn publish_transition_trace(trace_id: &str, world_id: &str, title: &str, started: u64) {
    let Some(socket) = std::env::var_os("TASK_FABRIC_SVN_RUNTIME_SOCKET")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("XDG_RUNTIME_DIR").map(|path| PathBuf::from(path).join("task_fabric_svn/runtime.sock"))) else { return; };
    let Ok(mut stream) = UnixStream::connect(socket) else { return; };
    let _ = stream.set_write_timeout(Some(Duration::from_millis(250)));
    let _ = stream.set_read_timeout(Some(Duration::from_millis(250)));
    let request = json!({
        "kind": "upsert_trace",
        "trace": {
            "schema_version": "task_fabric_svn.cognitive.v1",
            "trace_id": trace_id,
            "owner_id": "owner-local",
            "state": "open",
            "title": format!("World transition: {title}"),
            "created_at": rfc3339_from_epoch(started),
            "updated_at": rfc3339_from_epoch(started),
            "origin_receipt_id": Value::Null,
            "legacy_id": format!("world:{world_id}")
        }
    });
    if stream.write_all(request.to_string().as_bytes()).is_ok() && stream.write_all(b"\n").is_ok() {
        let _ = stream.shutdown(std::net::Shutdown::Write);
        let mut response = Vec::new();
        let _ = stream.read_to_end(&mut response);
    }
}

/// Format a UTC epoch second using the RFC3339 representation required by
/// the canonical runtime trace schema. This keeps the control-plane crate
/// dependency-light while preserving the actual transition timestamp.
fn rfc3339_from_epoch(seconds: u64) -> String {
    let days = (seconds / 86_400) as i64;
    let day_seconds = seconds % 86_400;
    // Howard Hinnant's civil-from-days conversion, anchored at 1970-01-01.
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = mp + if mp < 10 { 3 } else { -9 };
    let year = year + if month <= 2 { 1 } else { 0 };
    let hour = day_seconds / 3_600;
    let minute = (day_seconds % 3_600) / 60;
    let second = day_seconds % 60;
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

fn active_hyprland_instance(runtime_dir: &Path, configured: Option<&OsStr>) -> Option<OsString> {
    if let Some(configured) = configured.filter(|value| !value.is_empty()) {
        return Some(configured.to_os_string());
    }
    let root = runtime_dir.join("hypr");
    fs::read_dir(root)
        .ok()?
        .filter_map(Result::ok)
        .filter(|entry| entry.path().join(".socket.sock").exists())
        .filter_map(|entry| {
            let modified = entry.metadata().ok()?.modified().ok()?;
            Some((modified, entry.file_name()))
        })
        .max_by_key(|(modified, _)| *modified)
        .map(|(_, name)| name)
}

fn hyprctl_command() -> Command {
    let mut command = Command::new("hyprctl");
    let runtime_dir = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from);
    let configured = std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE");
    if let Some(signature) = runtime_dir
        .as_deref()
        .and_then(|path| active_hyprland_instance(path, configured.as_deref()))
    {
        command.env("HYPRLAND_INSTANCE_SIGNATURE", signature);
    }
    command
}

fn hypr_json(kind: &str) -> Value {
    hyprctl_command()
        .args(["-j", kind])
        .output()
        .ok()
        .and_then(|output| serde_json::from_slice(&output.stdout).ok())
        .unwrap_or_else(|| json!([]))
}

fn horizon_privacy() -> PrivacyConfig {
    let path = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".config")
        })
        .join("task-fabric-svn/horizon.toml");
    fs::read_to_string(path)
        .ok()
        .and_then(|text| toml::from_str(&text).ok())
        .unwrap_or_default()
}

// Task Fabric SVN and Omarchy are the default visual anchors, while every registered
// workstation project remains an available world in the completed system.
fn is_primary_horizon_world(project: &task_fabric_svn_work_core::Project) -> bool {
    let identity = format!("{} {}", project.id, project.label).to_ascii_lowercase();
    identity.contains("task-fabric-svn") || identity.contains("omarchy")
}

fn horizon_projection(
    runtime: &mut HorizonRuntime,
    config: &ProjectConfig,
    active_project_id: Option<&str>,
    active_transition_id: Option<&str>,
    active_trace_id: Option<&str>,
) -> Value {
    runtime.revision = runtime.revision.saturating_add(1);
    let mut desktop = horizon::build_desktop_projection(
        &hypr_json("clients"),
        &hypr_json("workspaces"),
        &hypr_json("monitors"),
        &hypr_json("activeworkspace"),
        runtime.revision,
        horizon_privacy(),
        &mut runtime.ids,
    );
    for window in &mut desktop.windows {
        let app = window.app.to_ascii_lowercase();
        let title = window.title.to_ascii_lowercase();
        let workspace = window.workspace.to_ascii_lowercase();
        window.project_id = config
            .projects
            .iter()
            .find(|project| {
                let id = project.id.to_ascii_lowercase();
                let workspace_match = workspace == id
                    || workspace == format!("world-{id}")
                    || workspace == format!("special:world-{id}");
                let scratchpad_app = app == format!("org.task_fabric_svn.world.{id}-scratchpad")
                    || app == format!("org.task_fabric_svn.world.{id}");
                let explicit_hub_title = title == format!("task_fabric_svn hub · {id}")
                    || title.contains(&format!("[world:{id}]"));
                workspace_match || scratchpad_app || explicit_hub_title
            })
            .map(|project| project.id.clone());
        if window.project_id.is_none() {
            window
                .operations
                .retain(|operation| operation != "gather_semantic_group");
        }
    }
    let worlds: Vec<Value> = config.projects.iter().map(|project| {
        let window_ids = desktop.windows.iter().filter(|window| window.project_id.as_deref() == Some(project.id.as_str())).map(|window| window.id.clone()).collect::<Vec<_>>();
        let workspace_ids = desktop.windows.iter().filter(|window| window.project_id.as_deref() == Some(project.id.as_str())).map(|window| window.workspace_id).collect::<std::collections::BTreeSet<_>>().into_iter().collect::<Vec<_>>();
        json!({
            "id":project.id,
            "label":project.label,
            "objective":project.objective,
            "phase":project.phase,
            "active":Some(project.id.as_str()) == active_project_id,
            "primary":is_primary_horizon_world(project),
            "attention_count":0,
            "workspace_ids":workspace_ids,
            "window_ids":window_ids,
            "actions":project.actions.iter().map(|action| json!({"id":action.id,"label":action.label})).collect::<Vec<_>>()
        })
    }).collect();
    let unsorted_window_ids = desktop
        .windows
        .iter()
        .filter(|window| window.project_id.is_none())
        .map(|window| window.id.clone())
        .collect::<Vec<_>>();
    json!({"schema": horizon::HORIZON_SCHEMA, "revision": runtime.revision, "generated_at": horizon::now(), "desktop": desktop, "projects": worlds.clone(), "worlds": worlds, "spatial":{"active_world_id":active_project_id,"active_transition_id":active_transition_id,"active_thread_id":active_trace_id,"unsorted_window_ids":unsorted_window_ids}, "authority":{"posture":"local","capability_active":runtime.capability.is_some()}, "allowed_operations":["focus_window","focus_workspace","move_window","gather_semantic_group","create_native_group","add_to_native_group","remove_from_native_group","dissolve_native_group","launch_declared_action","dismiss_horizon"], "cognitive":{"active_thread":null,"trajectory":["feed","watch","learn","client","timeline"]}})
}

fn run_hypr(args: &[&str]) -> Result<(), String> {
    let lua = match args {
        ["workspace", workspace] => {
            format!("hl.dsp.focus({{ workspace = {} }})", lua_string(workspace))
        }
        ["focuswindow", window] => {
            format!("hl.dsp.focus({{ window = {} }})", lua_string(window))
        }
        ["togglespecialworkspace", name] => {
            format!("hl.dsp.workspace.toggle_special({})", lua_string(name))
        }
        ["moveintogroup", direction] => {
            format!(
                "hl.dsp.window.move({{ into_group = {} }})",
                lua_string(direction)
            )
        }
        ["movetoworkspacesilent", workspace, window] => {
            let window = window.strip_prefix("address:").unwrap_or(window);
            format!(
                "hl.dsp.window.move({{ workspace = {}, follow = false, window = {} }})",
                lua_string(workspace),
                lua_string(&format!("address:{window}")),
            )
        }
        ["movetoworkspacesilent", target] => {
            let (workspace, window) = target
                .split_once(",address:")
                .ok_or_else(|| "invalid workspace move target".to_string())?;
            format!(
                "hl.dsp.window.move({{ workspace = {}, follow = false, window = {} }})",
                lua_string(workspace),
                lua_string(&format!("address:{window}")),
            )
        }
        _ => return Err("unsupported Hyprland operation".to_string()),
    };
    let status = hyprctl_command()
        .arg("dispatch")
        .arg(lua)
        .status()
        .map_err(|e| e.to_string())?;
    status
        .success()
        .then_some(())
        .ok_or_else(|| format!("hyprctl dispatch failed: {status}"))
}

fn launch_horizon_app() -> Result<(), String> {
    let unit = format!("task-fabric-svn-horizon-launch-{}", opaque_token());
    let state = std::env::var_os("TASK_FABRIC_SVN_WORK_STATE")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("XDG_STATE_HOME")
                .map(PathBuf::from)
                .map(|root| root.join("task-fabric-svn"))
        })
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var_os("HOME").unwrap_or_default())
                .join(".local/state/task-fabric-svn")
        });
    let profile = state.join("horizon-chromium");
    fs::create_dir_all(&profile).map_err(|error| error.to_string())?;
    let status = Command::new("systemd-run")
        .args(["--user", "--collect", "--no-block", "--quiet"])
        .arg(format!("--unit={unit}"))
        .arg("--")
        .arg("/usr/bin/chromium")
        .arg(format!("--user-data-dir={}", profile.display()))
        .args([
            "--app=http://127.0.0.1:3840/horizon",
            "--class=org.task_fabric_svn.Horizon",
            "--no-first-run",
            "--disable-sync",
        ])
        .status()
        .map_err(|error| error.to_string())?;
    status
        .success()
        .then_some(())
        .ok_or_else(|| format!("Horizon transient launch failed: {status}"))
}

fn lua_string(value: &str) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "\"\"".to_string())
}

fn dispatch_horizon(
    request: HorizonRequest,
    config: &ProjectConfig,
    config_path: &PathBuf,
    state: &PathBuf,
    shared: &Arc<Mutex<HorizonRuntime>>,
) -> Value {
    let mut runtime = match shared.lock() {
        Ok(value) => value,
        Err(_) => return json!({"error":"horizon state unavailable"}),
    };
    if request.request == "horizon" || request.request == "horizon_projection" {
        let active_project_id = resolve_active_project_id(config, load_active_project(&state.join("active-project")));
        let selected_world_id = resolve_active_project_id(config, load_world_id(&state.join("selected-world")));
        let requested_world_id = resolve_active_project_id(config, load_world_id(&state.join("requested-world")));
        let last_transition = read_json_file(&state.join("last-transition.json"));
        let active_transition_id = last_transition.get("id").and_then(Value::as_str).map(str::to_owned);
        let active_trace_id = last_transition.get("trace_id").and_then(Value::as_str).map(str::to_owned);
        let mut projection = horizon_projection(
            &mut runtime,
            config,
            active_project_id.as_deref(),
            active_transition_id.as_deref(),
            active_trace_id.as_deref(),
        );
        let world_snapshot = worldstate::snapshot_for_with_state(
            config,
            selected_world_id.as_deref(),
            requested_world_id.as_deref(),
            active_project_id.as_deref(),
            None,
            load_last_transition(state),
        );
        projection["worldstate"] = serde_json::to_value(&world_snapshot).unwrap_or_else(|_| json!({}));
        projection["spatial"]["selected_world_id"] = selected_world_id.clone().map(Value::String).unwrap_or(Value::Null);
        projection["spatial"]["requested_world_id"] = requested_world_id.clone().map(Value::String).unwrap_or(Value::Null);
        projection["spatial"]["world_activation_status"] = Value::String(world_snapshot.world_activation_status.clone());
        // Keep agent and project context in the same owner-local projection
        // consumed by Horizon so the shell does not need a second authority
        // lookup for the active world.
        let status = StatusSnapshot::local(config, config_path, false);
        projection["agent_context"] = serde_json::to_value(status.agents).unwrap_or_else(|_| json!([]));
        projection["project_context"] = serde_json::to_value(status.project).unwrap_or_else(|_| Value::Null);
        return projection;
    }
    if request.request == "horizon_toggle" {
        let existing_client = hypr_json("clients").as_array().and_then(|clients| {
            clients
                .iter()
                .find(|client| is_horizon_client(client))
                .cloned()
        });
        let result = if let Some(ref client) = existing_client {
            let address = client
                .get("address")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if address.is_empty() {
                Err("Horizon window address unavailable".to_string())
            } else {
                let move_target = format!("address:{address}");
                run_hypr(&[
                    "movetoworkspacesilent",
                    "special:task-fabric-svn-horizon",
                    &move_target,
                ])
                .and_then(|_| run_hypr(&["togglespecialworkspace", "task-fabric-svn-horizon"]))
            }
        } else {
            let launch = launch_horizon_app();
            launch.and_then(|_| {
                for _ in 0..30 {
                    if let Some(client) = hypr_json("clients")
                        .as_array()
                        .and_then(|clients| clients.iter().find(|client| is_horizon_client(client)))
                    {
                        let address = client
                            .get("address")
                            .and_then(Value::as_str)
                            .unwrap_or_default();
                        if !address.is_empty() {
                            let move_target = format!("address:{address}");
                            return run_hypr(&[
                                "movetoworkspacesilent",
                                "special:task-fabric-svn-horizon",
                                &move_target,
                            ])
                            .and_then(|_| run_hypr(&["togglespecialworkspace", "task-fabric-svn-horizon"]));
                        }
                    }
                    std::thread::sleep(std::time::Duration::from_millis(100));
                }
                Err("Horizon window did not appear after launch".to_string())
            })
        };
        return match result {
            Ok(()) => json!({"status":"toggled","existing":existing_client.is_some()}),
            Err(error) => json!({"error":error}),
        };
    }
    if request.request == "horizon_capability" {
        let nonce = request.instance_nonce.unwrap_or_default();
        if nonce.is_empty() {
            return json!({"error":"instance nonce required"});
        }
        if request.capability.as_deref() == Some("approve") {
            if safe_mode() {
                return json!({"error":"safe_mode"});
            }
            let token = format!("horizon-cap-{}", opaque_token());
            let now = horizon::now();
            runtime.capability = Some((nonce, token.clone(), now + 1800, now));
            return json!({"status":"granted","expires_at":horizon::now()+1800,"capability":token});
        }
        return json!({"status":"confirmation_required","expires_in_seconds":1800});
    }
    if request.request == "horizon_revoke" {
        runtime.capability = None;
        return json!({"status":"revoked"});
    }
    if request.request == "horizon_heartbeat" {
        if safe_mode() {
            runtime.capability = None;
            return json!({"status":"revoked","reason":"safe_mode"});
        }
        let valid = capability_valid(&runtime, &request);
        if valid {
            if let Some((_, _, _, last_seen)) = runtime.capability.as_mut() {
                *last_seen = horizon::now();
            }
            return json!({"status":"alive"});
        }
        runtime.capability = None;
        return json!({"status":"revoked","reason":"expired_or_mismatched"});
    }
    if request.request == "horizon_arrangements_list" {
        return json!({"arrangements": read_arrangements(state)});
    }
    if request.request == "horizon_arrangement_delete" {
        let Some(id) = request
            .payload
            .as_ref()
            .and_then(|p| p.get("id"))
            .and_then(Value::as_str)
        else {
            return json!({"error":"arrangement id required"});
        };
        if !safe_id(id) {
            return json!({"error":"invalid arrangement id"});
        }
        let path = arrangements_dir(state).join(format!("{id}.json"));
        return match fs::remove_file(path) {
            Ok(()) => json!({"status":"deleted"}),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                json!({"status":"not_found"})
            }
            Err(error) => json!({"error":error.to_string()}),
        };
    }
    if request.request == "horizon_arrangement_save" {
        let Some(payload) = request.payload.as_ref() else {
            return json!({"error":"arrangement payload required"});
        };
        return save_arrangement(&payload, state, &runtime);
    }
    if request.request == "horizon_arrangement_preview"
        || request.request == "horizon_arrangement_restore"
    {
        let Some(payload) = request.payload.as_ref() else {
            return json!({"error":"arrangement payload required"});
        };
        if request.request == "horizon_arrangement_restore" && !capability_valid(&runtime, &request)
        {
            return json!({"error":"capability_required"});
        }
        if request.request == "horizon_arrangement_restore"
            && request.revision != Some(runtime.revision)
        {
            return json!({"error":"stale_projection","revision":runtime.revision});
        }
        return restore_arrangement(
            payload,
            state,
            &runtime,
            request.request == "horizon_arrangement_restore",
        );
    }
    if request.request == "horizon_operation" {
        let Some(operation) = request.operation else {
            return json!({"error":"operation required"});
        };
        if request.revision != Some(runtime.revision) {
            return json!({"error":"stale_projection","revision":runtime.revision});
        }
        if request.preview {
            return json!({"status":"preview","revision":runtime.revision,"operation":operation.clone()});
        }
        if operation.mutating() {
            let valid = runtime
                .capability
                .as_ref()
                .map(|(nonce, token, expiry, last_seen)| {
                    request.instance_nonce.as_deref() == Some(nonce)
                        && request.capability.as_deref() == Some(token)
                        && horizon::now() < *expiry
                        && horizon::now().saturating_sub(*last_seen) <= 300
                })
                .unwrap_or(false);
            if safe_mode() {
                runtime.capability = None;
                return json!({"error":"safe_mode"});
            }
            if !valid {
                runtime.capability = None;
                return json!({"error":"capability_required"});
            }
        }
        let result = match operation {
            Operation::FocusWorkspace { workspace_id } => {
                run_hypr(&["workspace", &format!("id:{workspace_id}")])
            }
            Operation::FocusWindow { target_id } => {
                resolve_and_dispatch(&runtime, &target_id, &["focuswindow"])
            }
            Operation::MoveWindow {
                target_id,
                workspace_id,
            } => resolve_and_dispatch(
                &runtime,
                &target_id,
                &[
                    "movetoworkspacesilent",
                    &format!("{workspace_id},address:TARGET"),
                ],
            ),
            Operation::DismissHorizon => {
                runtime.capability = None;
                Ok(())
            }
            Operation::RemoveFromNativeGroup { target_id } => {
                resolve_and_dispatch(&runtime, &target_id, &["moveoutofgroup"])
            }
            Operation::CreateNativeGroup { target_ids } => native_group(&runtime, &target_ids),
            Operation::AddToNativeGroup {
                target_id,
                group_target_id,
            } => native_group(&runtime, &[group_target_id, target_id]),
            Operation::DissolveNativeGroup { target_id } => {
                resolve_and_dispatch(&runtime, &target_id, &["moveoutofgroup"])
            }
            Operation::GatherSemanticGroup {
                project_id,
                workspace_id,
            } => gather_project(&runtime, config, &project_id, workspace_id),
            Operation::LaunchDeclaredAction {
                project_id,
                action_id,
            } => launch_action(config, &project_id, &action_id),
        };
        return match result {
            Ok(()) => json!({"status":"completed","revision":runtime.revision}),
            Err(error) => json!({"status":"failed","error":error}),
        };
    }
    if request.request == "horizon_save_arrangement" {
        let _ = state;
        return json!({"status":"deferred","reason":"arrangement persistence follows the projection contract"});
    }
    json!({"error":"unsupported horizon request"})
}

fn is_horizon_client(client: &Value) -> bool {
    ["class", "initialClass"]
        .iter()
        .filter_map(|key| client.get(*key).and_then(Value::as_str))
        .any(|value| {
            value == "org.task_fabric_svn.Horizon" || value.starts_with("chrome-127.0.0.1__horizon-")
        })
        || ["title", "initialTitle"]
            .iter()
            .filter_map(|key| client.get(*key).and_then(Value::as_str))
            .any(|value| value == "127.0.0.1:3840/horizon")
}

fn arrangements_dir(state: &PathBuf) -> PathBuf {
    state.join("horizon/arrangements")
}
fn safe_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 96
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

fn read_arrangements(state: &PathBuf) -> Vec<Value> {
    fs::read_dir(arrangements_dir(state))
        .ok()
        .into_iter()
        .flatten()
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| fs::read_to_string(entry.path()).ok())
        .filter_map(|text| serde_json::from_str(&text).ok())
        .take(64)
        .collect()
}

fn save_arrangement(payload: &Value, state: &PathBuf, runtime: &HorizonRuntime) -> Value {
    let name = payload
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("Unnamed arrangement")
        .trim();
    let ids = payload
        .get("window_ids")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if name.is_empty() || name.len() > 120 || ids.len() > 128 {
        return json!({"error":"invalid bounded arrangement"});
    }
    let mut windows = Vec::new();
    let clients = hypr_json("clients");
    for id in ids.iter().filter_map(Value::as_str) {
        if let Some(client) = find_client(runtime, &clients, id) {
            let class = client
                .get("class")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            let title = client
                .get("title")
                .and_then(Value::as_str)
                .unwrap_or("Untitled");
            let privacy = horizon_privacy();
            if horizon::is_private(class, title, &privacy) {
                continue;
            }
            let workspace = client.get("workspace").cloned().unwrap_or_default();
            windows.push(json!({"app":class.chars().take(96).collect::<String>(),"title":title.chars().take(160).collect::<String>(),"workspace_id":workspace.get("id").and_then(Value::as_i64).unwrap_or(-1),"monitor":client.get("monitor").and_then(Value::as_i64).unwrap_or(-1),"native_group_key":Value::Null}));
        }
    }
    let id = format!("arrangement-{}", opaque_token());
    let arrangement = json!({"id":id,"name":name,"project_id":payload.get("project_id"),"windows":windows,"created_at":horizon::now()});
    let dir = arrangements_dir(state);
    if let Err(error) = fs::create_dir_all(&dir) {
        return json!({"error":error.to_string()});
    }
    let path = dir.join(format!("{id}.json"));
    match serde_json::to_vec_pretty(&arrangement)
        .map_err(|e| e.to_string())
        .and_then(|bytes| fs::write(path, bytes).map_err(|e| e.to_string()))
    {
        Ok(()) => json!({"status":"saved","arrangement":arrangement}),
        Err(error) => json!({"error":error}),
    }
}

fn find_client(runtime: &HorizonRuntime, clients: &Value, opaque_id: &str) -> Option<Value> {
    let stable = runtime
        .ids
        .iter()
        .find(|(_, value)| value.as_str() == opaque_id)
        .map(|(key, _)| key)?;
    clients
        .as_array()?
        .iter()
        .find(|client| {
            client
                .get("stableId")
                .and_then(Value::as_i64)
                .map(|v| v.to_string())
                == Some(stable.clone())
                || client.get("address").and_then(Value::as_str) == Some(stable.as_str())
        })
        .cloned()
}

fn capability_valid(runtime: &HorizonRuntime, request: &HorizonRequest) -> bool {
    runtime
        .capability
        .as_ref()
        .map(|(nonce, token, expiry, last_seen)| {
            request.instance_nonce.as_deref() == Some(nonce)
                && request.capability.as_deref() == Some(token)
                && horizon::now() < *expiry
                && horizon::now().saturating_sub(*last_seen) <= 300
        })
        .unwrap_or(false)
}

fn safe_mode() -> bool {
    ["TASK_FABRIC_SVN_SAFE_MODE", "TASK_FABRIC_SVN_SAFE_MODE"]
        .iter()
        .any(|key| std::env::var(key).ok().as_deref() == Some("1"))
}

fn native_group(runtime: &HorizonRuntime, ids: &[String]) -> Result<(), String> {
    if ids.len() < 2 || ids.len() > 16 {
        return Err("native group requires 2-16 windows".into());
    }
    for id in ids {
        if !runtime.ids.values().any(|value| value == id) {
            return Err("unknown or private window".into());
        }
    }
    resolve_and_dispatch(runtime, &ids[0], &["focuswindow"])?;
    for id in ids.iter().skip(1) {
        resolve_and_dispatch(runtime, id, &["focuswindow"])?;
        run_hypr(&["moveintogroup", "l"])?;
    }
    Ok(())
}

fn gather_project(
    runtime: &HorizonRuntime,
    config: &ProjectConfig,
    project_id: &str,
    workspace_id: i64,
) -> Result<(), String> {
    if !config
        .projects
        .iter()
        .any(|project| project.id == project_id)
    {
        return Err("project not found".into());
    }
    let clients = hypr_json("clients");
    let ids: Vec<String> = runtime
        .ids
        .iter()
        .filter_map(|(stable, opaque)| {
            let client = clients.as_array()?.iter().find(|client| {
                client
                    .get("stableId")
                    .and_then(Value::as_i64)
                    .map(|id| id.to_string())
                    == Some(stable.clone())
                    || client.get("address").and_then(Value::as_str) == Some(stable.as_str())
            })?;
            let class = client
                .get("class")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_ascii_lowercase();
            let title = client
                .get("title")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_ascii_lowercase();
            let project = project_id.to_ascii_lowercase();
            (class.contains(&project) || title.contains(&project)).then_some(opaque.clone())
        })
        .collect();
    for id in ids {
        resolve_and_dispatch(
            runtime,
            &id,
            &[
                "movetoworkspacesilent",
                &format!("{workspace_id},address:TARGET"),
            ],
        )?;
    }
    Ok(())
}

fn launch_action(config: &ProjectConfig, project_id: &str, action_id: &str) -> Result<(), String> {
    let project = config
        .projects
        .iter()
        .find(|project| project.id == project_id)
        .ok_or("project not found")?;
    let action = project
        .actions
        .iter()
        .find(|action| action.id == action_id)
        .ok_or("declared action not found")?;
    if action.command.is_empty() || action.command.len() > 32 {
        return Err("declared action is invalid".into());
    }
    let mut command = Command::new(&action.command[0]);
    command.args(&action.command[1..]);
    command.current_dir(action.cwd.as_deref().unwrap_or(&project.repo));
    command
        .spawn()
        .map(|_| ())
        .map_err(|error| format!("could not launch declared action: {error}"))
}

fn restore_arrangement(
    payload: &Value,
    state: &PathBuf,
    _runtime: &HorizonRuntime,
    execute: bool,
) -> Value {
    let _ = state;
    let windows = payload
        .get("windows")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if windows.len() > 128 {
        return json!({"error":"arrangement exceeds 128 windows"});
    }
    let clients = hypr_json("clients");
    let empty_clients = Vec::new();
    let mut effects = Vec::new();
    for item in windows {
        let app = item.get("app").and_then(Value::as_str).unwrap_or("unknown");
        let title = item
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or("Untitled");
        let target_workspace = item
            .get("workspace_id")
            .and_then(Value::as_i64)
            .unwrap_or(-1);
        let found = clients
            .as_array()
            .unwrap_or(&empty_clients)
            .iter()
            .find(|client| {
                client.get("class").and_then(Value::as_str) == Some(app)
                    && client.get("title").and_then(Value::as_str) == Some(title)
            });
        match found {
            Some(client) if execute => { let address = client.get("address").and_then(Value::as_str).unwrap_or(""); let result = run_hypr(&["movetoworkspacesilent", &format!("{target_workspace},address:{address}")]); effects.push(json!({"app":app,"title":title,"outcome":if result.is_ok(){"completed"}else{"failed"},"detail":if result.is_ok(){"matched and moved"}else{"hyprland dispatch failed"}})); },
            Some(_) => effects.push(json!({"app":app,"title":title,"outcome":"matched","detail":"existing safe window"})),
            None => effects.push(json!({"app":app,"title":title,"outcome":"unresolved","detail":"no exact safe window match; no launch reference declared"})),
        }
    }
    json!({"status":if execute{"restored"}else{"preview"},"effects":effects})
}

fn resolve_and_dispatch(runtime: &HorizonRuntime, id: &str, args: &[&str]) -> Result<(), String> {
    if !runtime.ids.values().any(|value| value == id) {
        return Err("unknown or private window".into());
    }
    let clients = hypr_json("clients");
    let stable = runtime
        .ids
        .iter()
        .find(|(_, value)| value.as_str() == id)
        .map(|(key, _)| key.clone())
        .ok_or("unknown window")?;
    let empty_clients = Vec::new();
    let client = clients
        .as_array()
        .unwrap_or(&empty_clients)
        .iter()
        .find(|client| {
            client
                .get("stableId")
                .and_then(Value::as_i64)
                .map(|v| v.to_string())
                == Some(stable.clone())
                || client.get("address").and_then(Value::as_str) == Some(stable.as_str())
        })
        .ok_or("window changed")?;
    let address = client
        .get("address")
        .and_then(Value::as_str)
        .ok_or("window address unavailable")?;
    let rendered: Vec<String> = args
        .iter()
        .map(|arg| arg.replace("TARGET", address))
        .collect();
    let refs: Vec<&str> = rendered.iter().map(String::as_str).collect();
    run_hypr(&refs)
}

fn opaque_token() -> String {
    let mut bytes = [0u8; 24];
    if fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut bytes))
        .is_err()
    {
        bytes[..8].copy_from_slice(&horizon::now().to_le_bytes());
    }
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn execute_transition(
    id: &str,
    config: &ProjectConfig,
    config_path: &PathBuf,
    example: bool,
    state: &PathBuf,
) -> Value {
    let (target_world_id, intent_record) = if id.trim_start().starts_with('{') {
        if let Ok(v) = serde_json::from_str::<Value>(id) {
            let wid = v.get("world_id").and_then(Value::as_str).unwrap_or(id).to_string();
            let intent = v.get("intent").and_then(|i| serde_json::from_value::<worldstate::IntentRecord>(i.clone()).ok());
            (wid, intent)
        } else {
            (id.to_string(), None)
        }
    } else {
        (id.to_string(), None)
    };

    let Some(world) = config.projects.iter().find(|p| {
        p.id.eq_ignore_ascii_case(&target_world_id) || p.aliases.iter().any(|a| a.eq_ignore_ascii_case(&target_world_id))
    }) else {
        return json!({"error":"world not found"});
    };
    let started = Instant::now();
    let start_epoch = now_epoch_seconds();
    let transition_id = serde_json::from_str::<Value>(id).ok().and_then(|v| v.get("transition_id").and_then(Value::as_str).map(String::from)).unwrap_or_else(|| worldstate::new_id("transition"));
    let trace_id = serde_json::from_str::<Value>(id).ok().and_then(|v| v.get("trace_id").and_then(Value::as_str).map(String::from)).unwrap_or_else(|| worldstate::new_trace_id());
    update_transition_status(state, &transition_id, json!({"transition_id":transition_id,"trace_id":trace_id,"world_id":world.id,"status":"planning","terminal":false,"updated_at":now_epoch_seconds()}));
    // A transition records both the owner's requested world and the durable
    // selection. The active world file is only updated after validation.
    let _ = save_world_id(&state.join("selected-world"), &world.id);
    let _ = save_world_id(&state.join("requested-world"), &world.id);
    let intent = intent_record.unwrap_or_else(|| worldstate::IntentRecord {
        id: worldstate::new_id("intent"),
        caller: worldstate::IntentCaller::HumanCli,
        summary: format!("Enter world {}", world.label),
        raw_prompt: Some(format!("task-fabric-svn-work enter {}", target_world_id)),
        parent_trace_id: None,
        authorization_ref: None,
        timestamp_epoch_seconds: start_epoch,
    });
    let active = resolve_active_project_id(config, load_active_project(&state.join("active-project")));
    let before = worldstate::snapshot_for_with_state(
        config,
        Some(&world.id),
        Some(&world.id),
        active.as_deref(),
        Some(&transition_id),
        load_last_transition(state),
    );
    let plan = worldstate::plan_for_with_intent(
        world,
        &before.snapshot_id,
        Some(trace_id.clone()),
        Some(intent.clone()),
    );
    let plan_path = state
        .join("world-transitions/plans")
        .join(format!("{}.json", plan.id));
    if let Err(e) = worldstate::atomic_write(&plan_path, &plan) {
        return json!({"error":format!("could not persist transition plan: {e}")});
    }
    let mut ids = Vec::new();
    let mut effects = Vec::new();
    let mut emit = |kind: &str,
                    summary: String,
                    effect: Option<String>,
                    outcome: Option<worldstate::EffectOutcome>| {
        let e = worldstate::WorldEvent {
            schema_version: 1,
            id: worldstate::new_id("event"),
            timestamp_epoch_seconds: now_epoch_seconds(),
            world_id: Some(world.id.clone()),
            transition_id: Some(transition_id.clone()),
            trace_id: Some(trace_id.clone()),
            intent_id: Some(intent.id.clone()),
            kind: kind.into(),
            source: "task-fabric-svn-workd".into(),
            summary,
            object_ref: Some(format!("world:{}", world.id)),
            effect_ref: effect,
            outcome: outcome.clone(),
            presentation: Some(task_fabric_svn_presentation(kind, outcome.as_ref())),
            evidence_refs: Vec::new(),
        };
        let _ = worldstate::append_event(&e);
        ids.push(e.id);
    };
    emit(
        "intent.requested",
        format!("Requested entry into {}", world.label),
        Some("record-intent".into()),
        Some(worldstate::EffectOutcome::Completed),
    );
    emit(
        "transition.planned",
        format!("Prepared {} typed effects", plan.effects.len()),
        None,
        Some(worldstate::EffectOutcome::Completed),
    );
    effects.push(result(
        "record-intent",
        worldstate::EffectOutcome::Completed,
        "intent persisted",
    ));
    if !world.repo.is_dir() {
        effects.push(result(
            "validate-world",
            worldstate::EffectOutcome::Failed,
            "repository does not exist",
        ));
        return finish(
            transition_id,
            trace_id,
            intent,
            world,
            started,
            start_epoch,
            before,
            ids,
            effects,
            config,
            config_path,
            example,
            state,
        );
    }
    effects.push(result(
        "validate-world",
        worldstate::EffectOutcome::Completed,
        "world definition and repository validated",
    ));
    if let Err(e) = save_active_project(&state.join("active-project"), &world.id) {
        effects.push(result(
            "activate-world",
            worldstate::EffectOutcome::Failed,
            &e.to_string(),
        ));
    } else {
        effects.push(result(
            "activate-world",
            worldstate::EffectOutcome::Completed,
            "active world recorded",
        ));
        emit(
            "world.activated",
            format!("{} is active", world.label),
            Some("activate-world".into()),
            Some(worldstate::EffectOutcome::Completed),
        );
    }
    for service in &world.services {
        let is_systemd = matches!(service.kind.as_str(), "systemd" | "user-systemd") || service.unit.is_some();
        if !is_systemd {
            effects.push(result(
                &format!("service-{}", service.id),
                worldstate::EffectOutcome::Skipped,
                "non-systemd service is observed read-only",
            ));
            continue;
        }
        let unit = service.unit.as_deref().unwrap_or(&service.target);
        let active = Command::new("systemctl")
            .args(["--user", "is-active", unit])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if active {
            effects.push(result(
                &format!("service-{}", service.id),
                worldstate::EffectOutcome::Skipped,
                "already active",
            ));
            continue;
        }

        // Port conflict reconciliation: safely clear stale/conflicting listeners before starting
        let port_opt = service.port.or_else(|| {
            service.target.split("://").nth(1)
                .and_then(|s| s.split('/').next())
                .and_then(|s| s.split(':').nth(1))
                .and_then(|s| s.parse::<u16>().ok())
        });
        // A listening port is observed state, not proof of ownership. Never
        // terminate an unknown process; only an explicitly owned unit may be
        // reconciled by a future ownership-aware adapter.
        let _owned_port = port_opt.filter(|_| false);

        let ok = Command::new("systemctl")
            .args(["--user", "start", unit])
            .status()
            .map(|s| s.success())
            .unwrap_or(false);

        // Poll up to 2 seconds for service readiness
        let mut ready = false;
        if ok {
            for _ in 0..8 {
                if Command::new("systemctl")
                    .args(["--user", "is-active", unit])
                    .output()
                    .map(|o| o.status.success())
                    .unwrap_or(false)
                {
                    ready = true;
                    break;
                }
                thread::sleep(Duration::from_millis(250));
            }
        }

        effects.push(result(
            &format!("service-{}", service.id),
            if ready {
                worldstate::EffectOutcome::Completed
            } else if ok {
                worldstate::EffectOutcome::Degraded
            } else {
                worldstate::EffectOutcome::Failed
            },
            if ready {
                "service started and verified active"
            } else if ok {
                "service start requested (pending ready)"
            } else {
                "service could not be started"
            },
        ));
    }
    if let Some(session) = &world.herdr_session {
        if herdr_ready(session) {
            effects.push(result(
                "herdr-session",
                worldstate::EffectOutcome::Skipped,
                "session already reachable",
            ));
        } else {
            let launched = Command::new("omarchy-launch-tui")
                .args([
                    "--app-id=org.task-fabric-svn.work.example",
                    "herdr",
                    "--session",
                    session,
                ])
                .current_dir(&world.repo)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .is_ok();
            let ready = launched && poll_herdr(session, Duration::from_secs(15));
            effects.push(result(
                "herdr-session",
                if ready {
                    worldstate::EffectOutcome::Completed
                } else {
                    worldstate::EffectOutcome::Degraded
                },
                if ready {
                    "Herdr session restored"
                } else {
                    "Herdr session unavailable after launch"
                },
            ));
        }
        effects.push(result(
            "observe-workspaces",
            if herdr_ready(session) {
                worldstate::EffectOutcome::Completed
            } else {
                worldstate::EffectOutcome::Degraded
            },
            "observed real Herdr session state",
        ));
    }
    let title = format!("Task Fabric SVN Hub · {}", world.id);
    let marker = state.join(format!("hub-project-{}.active", world.id));
    let already = marker.exists()
        || hyprctl_command()
            .args(["-j", "clients"])
            .output()
            .ok()
            .map(|o| String::from_utf8_lossy(&o.stdout).contains(&title))
            .unwrap_or(false);
    let launched = !already
        && Command::new("omarchy-launch-webapp")
            .arg(format!("http://127.0.0.1:3840/projects/{}", world.id))
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .is_ok();
    if launched {
        let _ = fs::write(&marker, format!("{}\n", now_epoch_seconds()));
    }
    let hub_outcome = if already {
        worldstate::EffectOutcome::Skipped
    } else if launched {
        worldstate::EffectOutcome::Completed
    } else {
        worldstate::EffectOutcome::Degraded
    };
    effects.push(result(
        "hub-project",
        hub_outcome,
        if already {
            "matching Hub project window already focused"
        } else if launched {
            "Hub project route launched"
        } else {
            "Hub launcher unavailable"
        },
    ));

    // Autonomous World Scratchpad Provisioning
    if let Some(sp) = &world.scratchpad {
        if sp.enabled {
            let special_name = format!("special:world-{}", world.id);
            let clients = hypr_json("clients");
            let has_client = clients.as_array().map_or(false, |arr| {
                arr.iter().any(|c| {
                    c.get("workspace")
                        .and_then(|w| w.get("name"))
                        .and_then(Value::as_str)
                        == Some(&special_name)
                })
            });

            if has_client {
                effects.push(result(
                    "scratchpad-provision",
                    worldstate::EffectOutcome::Skipped,
                    &format!("scratchpad already active on {special_name}"),
                ));
            } else {
                let term = sp.terminal.as_deref().unwrap_or("foot");
                let app_id = format!("org.task_fabric_svn.world.{}-scratchpad", world.id);
                let title = format!("{} (Scratchpad)", world.label);
                let mut cmd = Command::new(term);
                cmd.args(["-a", &app_id, "-T", &title, "-D", &world.repo.to_string_lossy()]);

                let wrapper = PathBuf::from(std::env::var_os("HOME").unwrap_or_default())
                    .join(".local/bin/task-fabric-svn-world-terminal");
                if wrapper.is_file() {
                    cmd.arg(&wrapper).arg(&world.id);
                } else if let Some(custom) = &sp.command {
                    cmd.args(["sh", "-c", custom]);
                }
                cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());

                match cmd.spawn() {
                    Ok(_) => {
                        thread::sleep(Duration::from_millis(150));
                        let lua_move = format!(
                            "hl.dsp.window.move({{ workspace = {}, follow = false, window = {} }})",
                            lua_string(&special_name),
                            lua_string(&format!("class:^{}$", app_id))
                        );
                        let _ = hyprctl_command().arg("dispatch").arg(lua_move).status();

                        effects.push(result(
                            "scratchpad-provision",
                            worldstate::EffectOutcome::Completed,
                            &format!("spawned scratchpad on {special_name}"),
                        ));
                        emit(
                            "world.scratchpad.provisioned",
                            format!("Scratchpad active on {special_name}"),
                            Some("scratchpad-provision".into()),
                            Some(worldstate::EffectOutcome::Completed),
                        );
                    }
                    Err(e) => {
                        effects.push(result(
                            "scratchpad-provision",
                            worldstate::EffectOutcome::Failed,
                            &format!("failed to spawn scratchpad: {e}"),
                        ));
                    }
                }
            }
        }
    }

    // Optional Ambient Theme Harmonization
    if let Some(theme) = &world.theme {
        let ok = Command::new("omarchy-theme-set")
            .arg(theme)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        effects.push(result(
            "theme-harmonize",
            if ok { worldstate::EffectOutcome::Completed } else { worldstate::EffectOutcome::Degraded },
            &format!("applied Omarchy theme '{theme}'"),
        ));
    }

    // Restore declared world dependencies and surfaces in dependency order.
    // Each entry is explicit owner configuration; no titles or implicit
    // project defaults are used to invent a surface.
    let mut resume_outcomes = std::collections::HashMap::<String, worldstate::EffectOutcome>::new();
    for entry in ordered_resume_entries(world) {
        let effect_id = format!("resume-{}", entry.id);
        let blocked = entry.depends_on.iter().any(|dependency| {
            matches!(resume_outcomes.get(dependency), Some(worldstate::EffectOutcome::Failed))
        });
        if blocked {
            let outcome = if entry.required { worldstate::EffectOutcome::Failed } else { worldstate::EffectOutcome::Skipped };
            effects.push(result(&effect_id, outcome.clone(), "dependency failed; resume entry not attempted"));
            resume_outcomes.insert(entry.id.clone(), outcome);
            continue;
        }
        let (outcome, detail) = match entry.kind.as_str() {
            "agent" | "herdr" => {
                let Some(session) = entry.session.as_deref().filter(|value| !value.is_empty()) else {
                    let outcome = worldstate::EffectOutcome::Failed;
                    effects.push(result(&effect_id, outcome.clone(), "resume agent has no session"));
                    resume_outcomes.insert(entry.id.clone(), outcome);
                    continue;
                };
                if herdr_ready(session) {
                    (worldstate::EffectOutcome::Skipped, "agent session already reachable")
                } else {
                    let launched = Command::new("omarchy-launch-tui")
                        .args(["--app-id=org.task-fabric-svn.work.resume", "herdr", "--session", session])
                        .current_dir(&world.repo)
                        .stdout(Stdio::null()).stderr(Stdio::null()).spawn().is_ok();
                    if launched && poll_herdr(session, Duration::from_secs(10)) {
                        (worldstate::EffectOutcome::Completed, "agent session restored")
                    } else {
                        (if entry.required { worldstate::EffectOutcome::Failed } else { worldstate::EffectOutcome::Degraded }, "agent session unavailable after launch")
                    }
                }
            }
            "portal" | "browser" | "surface" => {
                let target = entry.resource.as_deref().and_then(|id| world.resources.iter().find(|resource| resource.id == id).map(|resource| resource.target.as_str())).or(entry.target.as_deref());
                let Some(target) = target.filter(|value| value.starts_with("http://") || value.starts_with("https://")) else {
                    let outcome = worldstate::EffectOutcome::Failed;
                    effects.push(result(&effect_id, outcome.clone(), "resume portal has no http(s) target"));
                    resume_outcomes.insert(entry.id.clone(), outcome);
                    continue;
                };
                let launched = Command::new("omarchy-launch-webapp").arg(target).stdout(Stdio::null()).stderr(Stdio::null()).spawn().is_ok();
                (if launched { worldstate::EffectOutcome::Completed } else if entry.required { worldstate::EffectOutcome::Failed } else { worldstate::EffectOutcome::Degraded }, if launched { "portal surface opened" } else { "portal launcher unavailable" })
            }
            _ => (worldstate::EffectOutcome::Skipped, "unknown resume kind observed read-only"),
        };
        effects.push(result(&effect_id, outcome.clone(), detail));
        resume_outcomes.insert(entry.id.clone(), outcome);
    }

    effects.push(result(
        "publish-state",
        worldstate::EffectOutcome::Completed,
        "receipt persisted",
    ));
    finish(
        transition_id.clone(),
        trace_id,
        intent,
        world,
        started,
        start_epoch,
        before,
        ids,
        effects,
        config,
        config_path,
        example,
        state,
    )
}
fn task_fabric_svn_presentation(
    kind: &str,
    outcome: Option<&worldstate::EffectOutcome>,
) -> task_fabric_svn_work_core::notification_audio::TaskFabricSvnPresentation {
    let outcome_name = outcome.map(|value| {
        match value {
            worldstate::EffectOutcome::Completed => "completed",
            worldstate::EffectOutcome::Skipped => "skipped",
            worldstate::EffectOutcome::Degraded => "degraded",
            worldstate::EffectOutcome::Failed => "failed",
        }
        .to_string()
    });
    let category = match outcome {
        Some(worldstate::EffectOutcome::Failed) => "critical",
        Some(worldstate::EffectOutcome::Degraded) => "warning",
        Some(worldstate::EffectOutcome::Completed | worldstate::EffectOutcome::Skipped) => {
            "success"
        }
        None if kind.contains("intent") => "routine",
        None => "message",
    };
    let need = match category {
        "critical" => "a Task Fabric SVN transition needs immediate attention",
        "warning" => "a Task Fabric SVN transition needs attention",
        "success" => "nothing; the Task Fabric SVN transition completed",
        _ => "a Task Fabric SVN transition update arrived",
    };
    task_fabric_svn_work_core::notification_audio::TaskFabricSvnPresentation {
        category: Some(category.into()),
        need: Some(need.into()),
        context: None,
        outcome: outcome_name,
    }
}

fn ordered_resume_entries(world: &Project) -> Vec<task_fabric_svn_work_core::ResumeEntry> {
    let mut remaining = worldstate::resume_entries_for(world);
    let mut ordered = Vec::with_capacity(remaining.len());
    let mut completed = std::collections::HashSet::new();
    while !remaining.is_empty() {
        let before = remaining.len();
        let mut next = Vec::new();
        for entry in remaining {
            if entry.depends_on.iter().all(|dependency| completed.contains(dependency)) {
                completed.insert(entry.id.clone());
                ordered.push(entry);
            } else {
                next.push(entry);
            }
        }
        if next.len() == before { return Vec::new(); }
        remaining = next;
    }
    ordered
}

fn finish(
    transition_id: String,
    trace_id: String,
    intent: worldstate::IntentRecord,
    world: &task_fabric_svn_work_core::Project,
    started: Instant,
    start_epoch: u64,
    before: worldstate::WorldStateSnapshot,
    ids: Vec<String>,
    effects: Vec<worldstate::WorldTransitionEffectResult>,
    config: &ProjectConfig,
    _config_path: &PathBuf,
    _example: bool,
    state: &PathBuf,
) -> Value {
    let outcome = if effects
        .iter()
        .any(|e| e.outcome == worldstate::EffectOutcome::Failed)
    {
        "failed"
    } else if effects
        .iter()
        .any(|e| e.outcome == worldstate::EffectOutcome::Degraded)
    {
        "degraded"
    } else {
        "completed"
    };
    let finished = now_epoch_seconds();
    let reference = worldstate::TransitionReference {
        id: transition_id.clone(),
        outcome: outcome.into(),
        started_at_epoch_seconds: start_epoch,
        finished_at_epoch_seconds: Some(finished),
        trace_id: Some(trace_id.clone()),
    };
    if let Some(tid) = reference.trace_id.as_deref() {
        publish_transition_trace(tid, &world.id, &world.label, start_epoch);
    }
    let selected_id = resolve_active_project_id(config, load_world_id(&state.join("selected-world")));
    let requested_id = resolve_active_project_id(config, load_world_id(&state.join("requested-world")));
    let active_id = resolve_active_project_id(config, load_active_project(&state.join("active-project")));
    let after = worldstate::snapshot_for_with_state(
        config,
        selected_id.as_deref(),
        requested_id.as_deref(),
        active_id.as_deref(),
        None,
        Some(reference.clone()),
    );
    let receipt = worldstate::WorldTransitionReceipt {
        schema_version: 1,
        id: worldstate::new_id("receipt"),
        transition_id: transition_id.clone(),
        world_id: world.id.clone(),
        started_at_epoch_seconds: start_epoch,
        finished_at_epoch_seconds: finished,
        duration_ms: started.elapsed().as_millis(),
        outcome: outcome.into(),
        trace_id: Some(trace_id.clone()),
        intent: Some(intent.clone()),
        before_snapshot: before,
        after_snapshot: after,
        event_ids: ids,
        effects,
    };
    let _ = worldstate::atomic_write(&state.join("last-transition.json"), &reference);
    let final_event = worldstate::WorldEvent {
        schema_version: 1,
        id: worldstate::new_id("event"),
        timestamp_epoch_seconds: finished,
        world_id: Some(world.id.clone()),
        transition_id: Some(transition_id.clone()),
        trace_id: Some(trace_id.clone()),
        intent_id: Some(intent.id),
        kind: "transition.finished".into(),
        source: "task-fabric-svn-workd".into(),
        summary: format!("Transition {}", outcome),
        object_ref: Some(format!("world:{}", world.id)),
        effect_ref: None,
        outcome: Some(match outcome {
            "completed" => worldstate::EffectOutcome::Completed,
            "degraded" => worldstate::EffectOutcome::Degraded,
            _ => worldstate::EffectOutcome::Failed,
        }),
        presentation: None,
        evidence_refs: Vec::new(),
    };
    let _ = worldstate::append_event(&final_event);
    // Publish using the persisted world-event identity. The checkpoint
    // replay path will see the same ID after a restart, so retries remain
    // deduplicated instead of creating a second mobile alert.
    publish_companion_event(companion_event_for_world(&final_event));
    match worldstate::write_receipt(&receipt) {
        Ok(()) => { update_transition_status(state, &transition_id, json!({"transition_id":transition_id,"trace_id":trace_id,"world_id":world.id,"status":if outcome == "completed" { "active" } else { outcome },"outcome":outcome,"terminal":true,"receipt_id":receipt.id,"started_at":start_epoch,"finished_at":finished,"updated_at":finished})); publish_semantic_context(config, &world.id, state); serde_json::to_value(receipt).unwrap_or_else(|_| json!({"error":"could not encode receipt"})) }
        Err(e) => json!({"error":format!("could not persist transition receipt: {e}")}),
    }
}

fn update_transition_status(state: &Path, id: &str, value: Value) {
    let _ = worldstate::atomic_write(&state.join("world-transitions/status").join(format!("{id}.json")), &value);
}
fn result(
    id: &str,
    outcome: worldstate::EffectOutcome,
    detail: &str,
) -> worldstate::WorldTransitionEffectResult {
    worldstate::WorldTransitionEffectResult {
        effect_id: id.into(),
        outcome,
        detail: detail.into(),
        evidence_refs: Vec::new(),
    }
}
fn try_lock(lock: &Arc<Mutex<bool>>) -> bool {
    lock.try_lock()
        .map(|mut g| {
            if *g {
                false
            } else {
                *g = true;
                true
            }
        })
        .unwrap_or(false)
}
fn herdr_ready(session: &str) -> bool {
    Command::new("herdr")
        .args(["--session", session, "api", "snapshot"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}
fn poll_herdr(session: &str, timeout: Duration) -> bool {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if herdr_ready(session) {
            return true;
        }
        thread::sleep(Duration::from_millis(500));
    }
    false
}
fn load_last_transition(state: &PathBuf) -> Option<worldstate::TransitionReference> {
    fs::read(state.join("last-transition.json"))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
}
fn read_json_file(path: &PathBuf) -> Value {
    match fs::read(path) {
        Ok(b) => serde_json::from_slice(&b).unwrap_or_else(|_| json!({"error":"malformed record"})),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => json!({"error":"record not found"}),
        Err(e) => json!({"error":e.to_string()}),
    }
}
fn observe_attention(
    config: &ProjectConfig,
    path: &PathBuf,
    example: bool,
    alerts: &Arc<Mutex<AlertState>>,
    state: &PathBuf,
) {
    publish_pending_world_events(state);
    let snapshot = StatusSnapshot::local(config, path, example).with_source(StatusSource::Daemon);
    if let Ok(mut guard) = alerts.lock() {
        let active_window = attention_alert::query_active_window();
        let (items, spend_records) = guard.observe_with_focus(
            &snapshot,
            now_epoch_seconds(),
            DEFAULT_ALERT_COOLDOWN_SECONDS,
            active_window.as_ref(),
        );
        let _ = save_alert_state(&state.join("attention-alerts.json"), &guard);
        for record in &spend_records {
            let _ = attention_alert::append_spend_record(record);
        }
        if !items.is_empty() {
            for item in &items {
                let _ = worldstate::append_event(&worldstate::WorldEvent {
                    schema_version: 1,
                    id: worldstate::new_id("event"),
                    timestamp_epoch_seconds: now_epoch_seconds(),
                    world_id: None,
                    transition_id: None,
                    trace_id: None,
                    intent_id: None,
                    kind: "attention.opened".into(),
                    source: "task-fabric-svn-workd".into(),
                    summary: bounded_text(&item.title, 256),
                    object_ref: Some(item.id.clone()),
                    effect_ref: None,
                    outcome: None,
                    presentation: None,
                    evidence_refs: Vec::new(),
                });
                publish_companion_event(companion_event_for_attention(item));
            }
            deliver_alerts(&items);
        }
    }
}

fn companion_event_for_attention(item: &AttentionItem) -> CompanionEventV1 {
    let (category, need, level) = match item.severity.as_str() {
        "critical" => (
            CompanionEventCategory::Critical,
            CompanionEventNeed::UrgentAction,
            CompanionAttentionLevel::Interrupt,
        ),
        "error" => (
            CompanionEventCategory::Blocked,
            CompanionEventNeed::Intervention,
            CompanionAttentionLevel::Interrupt,
        ),
        _ => (
            CompanionEventCategory::Blocked,
            CompanionEventNeed::Awareness,
            CompanionAttentionLevel::Attention,
        ),
    };
    CompanionEventV1 {
        schema_version: 1,
        event_id: format!("attention:{}", item.id),
        created_at: now_epoch_seconds(),
        category,
        need,
        attention_level: level,
        title: bounded_text(&item.title, 96),
        safe_context: bounded_text(&item.detail, 256),
        speech_phrase: bounded_text(&item.title, 160),
        expires_at: None,
        action_kind: CompanionActionKind::None,
        action_ref: None,
    }
}

fn bounded_text(value: &str, max_bytes: usize) -> String {
    let mut result = String::new();
    for character in value.chars() {
        if result.len() + character.len_utf8() > max_bytes {
            break;
        }
        if character.is_control() {
            continue;
        }
        result.push(character);
    }
    result
}

fn publish_companion_event(event: CompanionEventV1) {
    if event.validate().is_err() {
        return;
    }
    let socket = std::env::var("TASK_FABRIC_SVN_COMPANION_EVENT_SOCKET")
        .unwrap_or_else(|_| "/run/task_fabric_svn/companion-events.sock".into());
    let Ok(mut stream) = UnixStream::connect(socket) else {
        return;
    };
    let Ok(payload) = serde_json::to_vec(&event) else {
        return;
    };
    if payload.len() > 4 * 1024 {
        return;
    }
    let _ = stream.write_all(&payload);
    let _ = stream.write_all(b"\n");
    let _ = stream.set_read_timeout(Some(Duration::from_millis(500)));
    let mut ack = [0_u8; 256];
    let _ = stream.read(&mut ack);
}

#[derive(serde::Serialize, serde::Deserialize, Default)]
struct CompanionPublicationCheckpoint {
    initialized: bool,
    last_event_id: Option<String>,
}

fn publish_pending_world_events(state: &PathBuf) {
    let path = state.join("companion-publication.json");
    let mut checkpoint: CompanionPublicationCheckpoint = fs::read(&path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default();
    let events = worldstate::read_events_append_order(512).unwrap_or_default();
    if !checkpoint.initialized {
        checkpoint.initialized = true;
        checkpoint.last_event_id = events.last().map(|event| event.id.clone());
        let _ = worldstate::atomic_write(&path, &checkpoint);
        return;
    }
    let start = checkpoint
        .last_event_id
        .as_ref()
        .and_then(|id| events.iter().position(|event| &event.id == id))
        .map(|index| index + 1)
        .unwrap_or(0);
    for event in events.iter().skip(start) {
        if event.kind == "transition.finished" || event.kind == "attention.opened" {
            publish_companion_event(companion_event_for_world(event));
        }
        checkpoint.last_event_id = Some(event.id.clone());
    }
    let _ = worldstate::atomic_write(&path, &checkpoint);
}

fn companion_event_for_world(event: &worldstate::WorldEvent) -> CompanionEventV1 {
    let category = match event
        .presentation
        .as_ref()
        .and_then(|p| p.category.as_deref())
    {
        Some("critical") => CompanionEventCategory::Critical,
        Some("warning") => CompanionEventCategory::Blocked,
        _ => CompanionEventCategory::Completion,
    };
    let need = match category {
        CompanionEventCategory::Critical => CompanionEventNeed::UrgentAction,
        CompanionEventCategory::Blocked => CompanionEventNeed::Intervention,
        _ => CompanionEventNeed::None,
    };
    CompanionEventV1 {
        schema_version: 1,
        event_id: format!("world:{}", event.id),
        created_at: event.timestamp_epoch_seconds,
        category,
        need,
        attention_level: if matches!(category, CompanionEventCategory::Critical) {
            CompanionAttentionLevel::Interrupt
        } else {
            CompanionAttentionLevel::Aware
        },
        title: bounded_text(&event.summary, 96),
        safe_context: bounded_text(&event.summary, 256),
        speech_phrase: bounded_text(&event.summary, 160),
        expires_at: None,
        action_kind: CompanionActionKind::None,
        action_ref: None,
    }
}
fn load_alert_state(path: &PathBuf) -> AlertState {
    fs::read(path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}
fn save_alert_state(path: &PathBuf, state: &AlertState) -> std::io::Result<()> {
    if let Some(p) = path.parent() {
        fs::create_dir_all(p)?;
    }
    let tmp = path.with_extension("tmp");
    fs::write(
        &tmp,
        serde_json::to_vec(state).map_err(std::io::Error::other)?,
    )?;
    fs::rename(tmp, path)
}
fn deliver_alerts(items: &[AttentionItem]) {
    let title = if items.len() == 1 {
        "Task Fabric SVN Pulse needs attention".into()
    } else {
        format!("Task Fabric SVN Pulse: {} items need attention", items.len())
    };
    let body = items
        .iter()
        .take(3)
        .map(|i| i.title.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    let _ = Command::new("notify-send")
        .args([
            "--app-name=Task Fabric SVN Pulse",
            "--urgency=normal",
            "--expire-time=8000",
            "--hint=string:task-fabric-svn-audio-category:warning",
            "--hint=string:task-fabric-svn-audio-need:a Task Fabric SVN project service needs attention",
            "--hint=string:task-fabric-svn-audio-outcome:degraded",
            &title,
            &body,
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}
fn resolve_active_project_id(config: &ProjectConfig, persisted: Option<String>) -> Option<String> {
    persisted.and_then(|id| {
        config
            .projects
            .iter()
            .find(|project| project.id.eq_ignore_ascii_case(&id) || project.aliases.iter().any(|alias| alias.eq_ignore_ascii_case(&id)))
            .map(|project| project.id.clone())
    })
}
fn load_active_project(path: &PathBuf) -> Option<String> {
    fs::read_to_string(path)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

fn load_world_id(path: &PathBuf) -> Option<String> {
    fs::read_to_string(path)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn save_world_id(path: &PathBuf, id: &str) -> std::io::Result<()> {
    if id.is_empty() || id.len() > worldstate::MAX_STRING || id.contains('/') {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "invalid world id"));
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, format!("{id}\n"))?;
    fs::rename(tmp, path)
}

fn save_active_project(path: &PathBuf, id: &str) -> std::io::Result<()> {
    if let Some(p) = path.parent() {
        fs::create_dir_all(p)?;
    }
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, format!("{id}\n"))?;
    fs::rename(tmp, path)
}
fn select_project(config: &mut ProjectConfig, id: &str) -> bool {
    let Some(i) = config.projects.iter().position(|p| {
        p.id.eq_ignore_ascii_case(id) || p.aliases.iter().any(|a| a.eq_ignore_ascii_case(id))
    }) else {
        return false;
    };
    config.projects.rotate_left(i);
    true
}

fn start_idle_inhibitor(config_path: PathBuf, state_dir: PathBuf) {
    thread::spawn(move || {
        let mut inhibitor_child: Option<std::process::Child> = None;
        let inhibitor_file = state_dir.join("idle-inhibitor.json");

        loop {
            thread::sleep(Duration::from_secs(2));

            let is_any_running = if let Ok((config, example)) = ProjectConfig::from_path_or_example(&config_path) {
                let snapshot = StatusSnapshot::local(&config, &config_path, example);
                snapshot.agents.iter().any(|a| a.state == "running")
            } else {
                false
            } || Command::new("pgrep")
                .args(["-f", "llama-server"])
                .status()
                .map(|s| s.success())
                .unwrap_or(false);

            if is_any_running {
                if inhibitor_child.is_none() {
                    match Command::new("systemd-inhibit")
                        .args([
                            "--what=idle:sleep",
                            "--who=Task Fabric SVN",
                            "--why=Autonomous agent active",
                            "--mode=block",
                            "sleep",
                            "infinity",
                        ])
                        .stdin(Stdio::null())
                        .stdout(Stdio::null())
                        .stderr(Stdio::null())
                        .spawn()
                    {
                        Ok(child) => {
                            let pid = child.id();
                            inhibitor_child = Some(child);
                            let _ = fs::write(
                                &inhibitor_file,
                                json!({
                                    "active": true,
                                    "pid": pid,
                                    "what": "idle:sleep",
                                    "who": "Task Fabric SVN",
                                    "why": "Autonomous agent active",
                                    "timestamp_epoch_seconds": now_epoch_seconds(),
                                })
                                .to_string(),
                            );
                        }
                        Err(e) => {
                            eprintln!("task-fabric-svn-workd: failed to spawn systemd-inhibit: {e}");
                        }
                    }
                }
            } else if let Some(mut child) = inhibitor_child.take() {
                let _ = child.kill();
                let _ = child.wait();
                let _ = fs::write(
                    &inhibitor_file,
                    json!({
                        "active": false,
                        "pid": Value::Null,
                        "released_at_epoch_seconds": now_epoch_seconds(),
                    })
                    .to_string(),
                );
            }
        }
    });
}

#[cfg(test)]
mod compositor_environment_tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn runtime_fixture() -> PathBuf {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        std::env::temp_dir().join(format!("task-fabric-svn-workd-hypr-{}-{suffix}", std::process::id()))
    }

    #[test]
    fn persisted_active_world_is_resolved_without_falling_back_to_first_project() {
        let mut config = ProjectConfig::example_pilot();
        let mut second = config.projects[0].clone();
        second.id = "second-world".into();
        second.label = "Second world".into();
        config.projects.push(second);
        let expected = config.projects[1].id.clone();
        assert_eq!(resolve_active_project_id(&config, Some(expected.clone())), Some(expected));
        assert_eq!(resolve_active_project_id(&config, Some("missing-world".into())), None);
    }

    #[test]
    fn transition_trace_timestamp_is_rfc3339() {
        assert_eq!(rfc3339_from_epoch(0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339_from_epoch(1_788_714_857), "2026-09-06T17:14:17Z");
    }

    #[test]
    fn transition_submission_preserves_caller_trace_and_intent() {
        let parsed = json!({
            "world_id": "world-a",
            "trace_id": "trace-owner-1",
            "intent": {"id": "intent-approval-1", "authorization_ref": "approval:approval-1"}
        });
        let request = transition_request("world-a", &parsed, "transition-1", "trace-owner-1");
        assert_eq!(request["transition_id"], "transition-1");
        assert_eq!(request["trace_id"], "trace-owner-1");
        assert_eq!(request["intent"]["id"], "intent-approval-1");
        assert_eq!(request["intent"]["authorization_ref"], "approval:approval-1");
    }

    #[test]
    fn configured_hyprland_instance_remains_authoritative() {
        let runtime = runtime_fixture();
        assert_eq!(
            active_hyprland_instance(&runtime, Some(OsStr::new("configured-instance"))),
            Some(OsString::from("configured-instance"))
        );
    }

    #[test]
    fn discovers_the_newest_live_owner_socket_when_environment_is_missing() {
        let runtime = runtime_fixture();
        let old = runtime.join("hypr/old-instance");
        let current = runtime.join("hypr/current-instance");
        fs::create_dir_all(&old).expect("old instance");
        fs::write(old.join(".socket.sock"), []).expect("old socket marker");
        std::thread::sleep(Duration::from_millis(10));
        fs::create_dir_all(&current).expect("current instance");
        fs::write(current.join(".socket.sock"), []).expect("current socket marker");

        assert_eq!(
            active_hyprland_instance(&runtime, None),
            Some(OsString::from("current-instance"))
        );
        fs::remove_dir_all(runtime).expect("remove fixture");
    }
}
