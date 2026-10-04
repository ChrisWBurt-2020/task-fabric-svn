use task_fabric_svn_work_core::{
    attention_alert, default_config_path, default_socket_path, durable, is_supported_action_risk,
    now_epoch_seconds, worldstate, ProjectConfig, StatusSnapshot,
};
use task_fabric_svn_work_core::experiment;
use task_fabric_svn_work_core::notification_audio::fnv1a;
use serde_json::json;
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;

mod task_cli;
use std::time::{Duration, Instant};

fn main() {
    let exit_code = match run(std::env::args().skip(1).collect()) {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("task-fabric-svn-work: {error}");
            1
        }
    };
    std::process::exit(exit_code);
}

fn run(args: Vec<String>) -> Result<(), String> {
    let command = args.first().map(String::as_str).unwrap_or("help");
    match command {
        "status" => status_command(&args[1..]),
        "select" => select_command(&args[1..]),
        "enter" => enter_command(&args[1..]),
        "simulate" => enter_simulate_command(&args[1..]),
        "why" => why_command(&args[1..]),
        "resume" => resume_command(&args[1..]),
        "agent-attach" => agent_attach_command(&args[1..]),
        "doctor" => doctor_command(&args[1..]),
        "work" => enter_command(&args[1..]),
        "menu-provider" => menu_provider_command(),
        "memory" => memory_command(&args[1..]),
        "world" => world_command(&args[1..]),
        "worlds" => worlds_command(&args[1..]),
        "scratchpad" => scratchpad_command(&args[1..]),
        "station" if args.get(1).map(String::as_str) == Some("sync") => {
            station_sync_command(&args[2..])
        }
        "worldstate" => read_command("worldstate", &args[1..]),
        "world-events" => read_command("world-events", &args[1..]),
        "study-context" => study_context_command(&args[1..]),
        "semantic-context" => read_command("semantic-context", &args[1..]),
        "transition-receipt" => read_command("transition-receipt", &args[1..]),
        "experiment-status" => read_command("experiment-status", &args[1..]),
        "action" if args.get(1).map(String::as_str) == Some("run") => action_command(&args[2..]),
        "action" if args.get(1).map(String::as_str) == Some("dry-run") => action_dry_run_command(&args[2..]),
        "signal" => signal_command(&args[1..]),
        "improve" => improve_command(&args[1..]),
        "experiment" => experiment_command(&args[1..]),
        "learn" => learn_command(&args[1..]),
        "audio" => audio_command(&args[1..]),
        "horizon" => horizon_command(&args[1..]),
        "attention" => attention_command(&args[1..]),
        "task" => task_cli::task_command(&args[1..]),
        "config" if args.get(1).map(String::as_str) == Some("example") => print_example_config(),
        "help" | "--help" | "-h" => {
            print_help();
            Ok(())
        }
        "--version" | "-V" => {
            println!(
                "task-fabric-svn-work {} ({})",
                env!("CARGO_PKG_VERSION"),
                option_env!("TASK_FABRIC_SVN_SOURCE_COMMIT").unwrap_or("unknown")
            );
            Ok(())
        }
        other => Err(format!("unknown command {other} (try task-fabric-svn-work help)")),
    }
}

fn memory_command(args: &[String]) -> Result<(), String> {
    let sub = args.first().map(String::as_str).unwrap_or("help");
    match sub {
        "sync" | "ingest" => {
            let mut cmd = Command::new("task-fabric-svn-exocortex-sync");
            if args.iter().any(|a| a == "--force") {
                cmd.arg("--force");
            }
            let status = cmd.status().map_err(|e| format!("failed to run task-fabric-svn-exocortex-sync: {e}"))?;
            if status.success() {
                Ok(())
            } else {
                Err("memory sync failed".to_string())
            }
        }
        "stats" => {
            let status = Command::new("task-fabric-svn-exocortex-sync")
                .arg("--stats")
                .status()
                .map_err(|e| format!("failed to run task-fabric-svn-exocortex-sync: {e}"))?;
            if status.success() {
                Ok(())
            } else {
                Err("memory stats failed".to_string())
            }
        }
        "search" => {
            let query = args.get(1).ok_or_else(|| "usage: task-fabric-svn-work memory search \"<query>\" [--world <id>] [--limit <n>] [--json]".to_string())?;
            let mut cmd = Command::new("task-fabric-svn-memory-query");
            cmd.arg("search").arg(query);
            for arg in &args[2..] {
                cmd.arg(arg);
            }
            let status = cmd.status().map_err(|e| format!("failed to run task-fabric-svn-memory-query: {e}"))?;
            if status.success() {
                Ok(())
            } else {
                Err("memory search failed".to_string())
            }
        }
        "query" => {
            let question = args.get(1).ok_or_else(|| "usage: task-fabric-svn-work memory query \"<question>\" [--world <id>] [--voice] [--json]".to_string())?;
            let mut cmd = Command::new("task-fabric-svn-memory-query");
            cmd.arg("query").arg(question);
            for arg in &args[2..] {
                cmd.arg(arg);
            }
            let status = cmd.status().map_err(|e| format!("failed to run task-fabric-svn-memory-query: {e}"))?;
            if status.success() {
                Ok(())
            } else {
                Err("memory query failed".to_string())
            }
        }
        "recall" => {
            let mut cmd = Command::new("task-fabric-svn-memory-query");
            cmd.arg("recall");
            for arg in &args[1..] {
                cmd.arg(arg);
            }
            let status = cmd.status().map_err(|e| format!("failed to run task-fabric-svn-memory-query: {e}"))?;
            if status.success() {
                Ok(())
            } else {
                Err("memory recall failed".to_string())
            }
        }
        _ => {
            println!("Usage: task-fabric-svn-work memory <ingest|search|query|recall|stats> [options]");
            println!("  task-fabric-svn-work memory ingest [--force]             Sync and index workstation memories");
            println!("  task-fabric-svn-work memory search \"<query>\" [--world ID] Semantic similarity search");
            println!("  task-fabric-svn-work memory query \"<question>\" [--voice] Grounded local Qwen3-4B Q&A");
            println!("  task-fabric-svn-work memory recall --world <id>           Recall key past events & decisions");
            println!("  task-fabric-svn-work memory stats                         Print cognitive graph statistics");
            Ok(())
        }
    }
}

fn world_command(args: &[String]) -> Result<(), String> {
    let sub = args.first().map(String::as_str).unwrap_or("list");
    match sub {
        "list" => worlds_command(&args[1..]),
        "scan" => world_scan_command(&args[1..]),
        "add" => world_add_command(&args[1..]),
        "reconcile" => world_reconcile_command(&args[1..]),
        _ => {
            if sub.starts_with('-') {
                worlds_command(args)
            } else {
                println!("Usage: task-fabric-svn-work world <list|scan|add|reconcile> [options]");
                println!("  task-fabric-svn-work world list [--aliases]          List all registered cognitive worlds");
                println!("  task-fabric-svn-work world scan [--root <dir>] [--apply] [--dry-run] [--json]");
                println!("                                             Scan workspace root for unregistered projects");
                println!("  task-fabric-svn-work world add <path> [--apply] [--dry-run] [--json]");
                println!("                                             Mine and register a specific project directory");
                println!("  task-fabric-svn-work world reconcile <world-id> [--dry-run] [--json]");
                println!("                                             Preview world activation and invariants");
                Ok(())
            }
        }
    }
}

fn world_reconcile_command(args: &[String]) -> Result<(), String> {
    let world_id = args
        .iter()
        .find(|arg| !arg.starts_with('-'))
        .ok_or_else(|| "usage: task-fabric-svn-work world reconcile <world-id> [--dry-run] [--json]".to_string())?;
    // Reconcile is deliberately a preview-only tracer. Keep accepting an
    // explicit --dry-run for script clarity, but never permit mutation here.
    let mut forwarded = vec![world_id.clone(), "--dry-run".to_string()];
    if args.iter().any(|arg| arg == "--json") {
        forwarded.push("--json".to_string());
    }
    enter_command(&forwarded)
}

fn world_scan_command(args: &[String]) -> Result<(), String> {
    let (config, config_path) = load_config()?;
    let mut root = match std::env::var_os("HOME") {
        Some(h) => PathBuf::from(h).join("Work"),
        None => PathBuf::from("."),
    };
    let mut apply = false;
    let mut json_output = false;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--root" => {
                if let Some(r) = args.get(i + 1) {
                    root = PathBuf::from(r);
                    i += 1;
                }
            }
            "--apply" => apply = true,
            "--dry-run" => apply = false,
            "--json" => json_output = true,
            _ => {}
        }
        i += 1;
    }

    let report = task_fabric_svn_work_core::world_miner::scan_unregistered_projects(&root, &config)
        .map_err(|e| format!("Failed to scan projects in {}: {e}", root.display()))?;

    if json_output {
        let json_val = serde_json::to_string_pretty(&report).map_err(|e| e.to_string())?;
        println!("{json_val}");
        return Ok(());
    }

    println!("Scanned root: {}", report.scanned_root.display());
    println!("Existing registered worlds: {}", report.existing_count);
    println!("Discovered unconfigured worlds: {}", report.discovered_count);

    if report.projects.is_empty() {
        println!("All projects in {} are already registered.", root.display());
        return Ok(());
    }

    println!("\nDiscovered Candidates:");
    for p in &report.projects {
        let service_names: Vec<String> = p.services.iter().map(|s| s.target.clone()).collect();
        let service_str = if service_names.is_empty() {
            "none".to_string()
        } else {
            service_names.join(", ")
        };
        let action_count = p.actions.len();
        println!("  • {} ({})", p.id, p.label);
        println!("    repo: {}", p.repo.display());
        println!("    objective: {}", p.objective);
        println!("    services: {}", service_str);
        println!("    actions: {} configured", action_count);
    }

    if apply {
        let added = task_fabric_svn_work_core::world_miner::append_projects_to_config(&config_path, &report.projects)
            .map_err(|e| format!("Failed to update config at {}: {e}", config_path.display()))?;
        println!("\n[task-fabric-svn-work] Successfully registered {} new cognitive worlds in {}", added, config_path.display());
    } else {
        println!("\n[SIMULATION / DRY-RUN] Zero configuration modifications performed.");
        println!("Run 'task-fabric-svn-work world scan --apply' to persist these discovered worlds to {}.", config_path.display());
    }

    Ok(())
}

fn world_add_command(args: &[String]) -> Result<(), String> {
    let (config, config_path) = load_config()?;
    let path_str = args.iter().find(|a| !a.starts_with('-')).ok_or_else(|| {
        "usage: task-fabric-svn-work world add <path> [--apply] [--dry-run] [--json]".to_string()
    })?;
    let apply = args.iter().any(|a| a == "--apply");
    let json_output = args.iter().any(|a| a == "--json");

    let dir = Path::new(path_str);
    if !dir.is_dir() {
        return Err(format!("path is not a directory: {}", dir.display()));
    }

    let project = task_fabric_svn_work_core::world_miner::mine_project_directory(dir)
        .ok_or_else(|| format!("could not mine project at {}", dir.display()))?;

    // Check if already registered
    if config.projects.iter().any(|p| p.id.eq_ignore_ascii_case(&project.id) || p.repo == project.repo) {
        return Err(format!("project {} is already registered in {}", project.id, config_path.display()));
    }

    if json_output {
        let json_val = serde_json::to_string_pretty(&project).map_err(|e| e.to_string())?;
        println!("{json_val}");
        return Ok(());
    }

    println!("Mined Candidate:");
    println!("  • {} ({})", project.id, project.label);
    println!("    repo: {}", project.repo.display());
    println!("    objective: {}", project.objective);
    println!("    services: {}", project.services.iter().map(|s| s.target.as_str()).collect::<Vec<_>>().join(", "));
    println!("    actions: {} configured", project.actions.len());

    if apply {
        task_fabric_svn_work_core::world_miner::append_projects_to_config(&config_path, &[project.clone()])
            .map_err(|e| format!("Failed to update config at {}: {e}", config_path.display()))?;
        println!("\n[task-fabric-svn-work] Successfully registered {} in {}", project.id, config_path.display());
    } else {
        println!("\n[SIMULATION / DRY-RUN] Zero configuration modifications performed.");
        println!("Run 'task-fabric-svn-work world add {} --apply' to persist.", dir.display());
    }

    Ok(())
}

fn worlds_command(args: &[String]) -> Result<(), String> {
    let (config, _) = load_config()?;
    let with_aliases = args.iter().any(|a| a == "--aliases" || a == "-a");
    for p in &config.projects {
        println!("{}", p.id);
        if with_aliases {
            for alias in &p.aliases {
                println!("{alias}");
            }
        }
    }
    Ok(())
}

fn horizon_command(args: &[String]) -> Result<(), String> {
    let action = args.first().map(String::as_str).unwrap_or("status");
    let request = match action {
        "status" => serde_json::json!({"request":"horizon"}),
        "open" | "toggle" => serde_json::json!({"request":"horizon_toggle"}),
        "hide" => serde_json::json!({"request":"horizon_revoke"}),
        _ => return Err("usage: task-fabric-svn-work horizon open|toggle|hide|status".into()),
    };
    let value = request_value(&default_socket_path(), &request.to_string())?;
    println!(
        "{}",
        serde_json::to_string_pretty(&value).map_err(|e| e.to_string())?
    );
    value
        .get("error")
        .is_none()
        .then_some(())
        .ok_or_else(|| value["error"].to_string())
}

fn scratchpad_command(args: &[String]) -> Result<(), String> {
    let action = args.first().map(String::as_str).unwrap_or("toggle");
    let explicit_world = args.windows(2).find(|w| w[0] == "--world" || w[0] == "-w").map(|w| w[1].clone());

    // 1. Query active Hyprland workspace
    let active_ws_output = Command::new("hyprctl")
        .args(["activeworkspace", "-j"])
        .output()
        .ok()
        .and_then(|o| serde_json::from_slice::<serde_json::Value>(&o.stdout).ok());

    let active_ws_name = active_ws_output
        .as_ref()
        .and_then(|v| v.get("name").and_then(serde_json::Value::as_str))
        .unwrap_or("");

    let (config, _) = load_config().unwrap_or_else(|_| (ProjectConfig::default(), PathBuf::new()));

    // Hyprland is a surface observation, not WorldState authority. Without
    // an explicit target, use the daemon's verified active_world_id; a
    // workspace name must never independently claim that a World is active.
    let canonical_active_world = request_value(&default_socket_path(), "status")
        .ok()
        .and_then(|value| value.get("active_world_id").and_then(serde_json::Value::as_str).map(String::from));
    let matched_project = explicit_world
        .as_deref()
        .or(canonical_active_world.as_deref())
        .and_then(|world_id| find_project(&config, world_id).cloned());

    match action {
        "toggle" => {
            if let Some(project) = matched_project {
                let special_ws = format!("world-{}", project.id);
                let full_special_name = format!("special:{special_ws}");

                // Check if any client exists on this special workspace
                let clients_val = Command::new("hyprctl")
                    .args(["clients", "-j"])
                    .output()
                    .ok()
                    .and_then(|o| serde_json::from_slice::<serde_json::Value>(&o.stdout).ok())
                    .unwrap_or_else(|| serde_json::json!([]));

                let has_client = clients_val.as_array().map_or(false, |arr| {
                    arr.iter().any(|c| {
                        c.get("workspace")
                            .and_then(|w| w.get("name"))
                            .and_then(serde_json::Value::as_str)
                            == Some(&full_special_name)
                    })
                });

                if !has_client {
                    let app_id = format!("org.task_fabric_svn.world.{}-scratchpad", project.id);
                    let title = format!("{} (Scratchpad)", project.label);
                    let repo_dir = project.repo.to_string_lossy().to_string();

                    let mut cmd = Command::new("foot");
                    cmd.args(["-a", &app_id, "-T", &title, "-D", &repo_dir]);

                    let wrapper = PathBuf::from(std::env::var_os("HOME").unwrap_or_default())
                        .join(".local/bin/task-fabric-svn-world-terminal");
                    if wrapper.is_file() {
                        cmd.arg(&wrapper).arg(&project.id);
                    }

                    cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
                    if let Ok(_child) = cmd.spawn() {
                        std::thread::sleep(Duration::from_millis(150));
                        let lua_move = format!(
                            "hl.dsp.window.move({{ workspace = \"{}\", follow = false, window = \"class:^{}$\" }})",
                            full_special_name, app_id
                        );
                        let _ = Command::new("hyprctl").args(["dispatch", &lua_move]).status();
                    }
                }

                // Dispatch toggle
                let lua_toggle = format!("hl.dsp.workspace.toggle_special(\"{}\")", special_ws);
                let _ = Command::new("hyprctl").args(["dispatch", &lua_toggle]).status();
            } else {
                // Fallback to Omarchy's default scratchpad
                let _ = Command::new("hyprctl")
                    .args(["dispatch", "hl.dsp.workspace.toggle_special(\"scratchpad\")"])
                    .status();
            }
            Ok(())
        }
        "send" => {
            if let Some(project) = matched_project {
                let full_special_name = format!("special:world-{}", project.id);
                let lua_move = format!("hl.dsp.window.move({{ workspace = \"{}\", follow = false }})", full_special_name);
                let _ = Command::new("hyprctl").args(["dispatch", &lua_move]).status();
            } else {
                let _ = Command::new("hyprctl")
                    .args(["dispatch", "hl.dsp.window.move({ workspace = \"special:scratchpad\", follow = false })"])
                    .status();
            }
            Ok(())
        }
        "status" => {
            let active_world = matched_project.as_ref().map(|p| p.id.as_str()).unwrap_or("none");
            let json = serde_json::json!({
                "active_workspace": active_ws_name,
                "active_world": active_world,
                "scratchpad_target": matched_project.as_ref().map(|p| format!("special:world-{}", p.id)).unwrap_or_else(|| "special:scratchpad".to_string())
            });
            println!("{}", serde_json::to_string_pretty(&json).map_err(|e| e.to_string())?);
            Ok(())
        }
        other => Err(format!("unknown scratchpad subcommand '{other}' (expected toggle, send, status)")),
    }
}

fn enter_simulate_command(args: &[String]) -> Result<(), String> {
    let mut forwarded = args.to_vec();
    if !forwarded.iter().any(|a| a == "--dry-run") {
        forwarded.push("--dry-run".to_string());
    }
    enter_command(&forwarded)
}

fn get_hypr_clients() -> Option<Vec<serde_json::Value>> {
    Command::new("hyprctl")
        .args(["clients", "-j"])
        .output()
        .ok()
        .and_then(|o| serde_json::from_slice::<Vec<serde_json::Value>>(&o.stdout).ok())
}

fn print_simulation_report(report: &worldstate::SimulationReport) {
    let status_str = match report.level {
        worldstate::PreflightLevel::Ready => "READY",
        worldstate::PreflightLevel::Warning => "WARNING",
        worldstate::PreflightLevel::Blocked => "BLOCKED",
    };
    println!("✈  Flight Simulator · Counterfactual Simulation for '{}' ({})", report.world_label, report.world_id);
    println!("Status: {} ({} blockers, {} warnings) · ZERO MUTATIONS EXECUTED\n", status_str, report.blockers.len(), report.warnings.len());
    println!("Requested world : {}", report.requested_world_id);
    println!("Observed active : {}", report.observed_active_world_id.as_deref().unwrap_or("none"));
    println!("Projected active: {}", report.projected_active_world_id);
    println!("Approval needed : {}", if report.required_approval { "yes" } else { "no" });
    println!("Expected receipt: {}\n", report.expected_receipt);

    println!("Pre-flight Checks:");
    for check in &report.preflight_checks {
        let mark = if check.passed { "✓" } else { "✗" };
        println!("  [{mark}] {:<12} · {}: {}", check.category, check.label, check.detail);
    }
    println!();

    println!("Predicted Effect Timeline:");
    for (idx, effect) in report.predicted_effects.iter().enumerate() {
        let outcome_str = match &effect.predicted_outcome {
            worldstate::SimulatedOutcome::Completed { detail } => format!("Predicted: Completed ({detail})"),
            worldstate::SimulatedOutcome::Skipped { reason } => format!("Predicted: Skipped ({reason})"),
            worldstate::SimulatedOutcome::Conflict { port, conflicting_process, remediation } => {
                let proc_info = conflicting_process.as_deref().unwrap_or("unknown process");
                format!("Predicted: CONFLICT on port {port} ({proc_info}) -> {remediation}")
            }
            worldstate::SimulatedOutcome::Failed { reason } => format!("Predicted: FAILED ({reason})"),
        };
        println!("  [{}] {:<22} · {}", idx + 1, effect.id, outcome_str);
    }

    if !report.blockers.is_empty() {
        println!("\nBlockers:");
        for b in &report.blockers {
            println!("  [!] {b}");
        }
    }
    if !report.warnings.is_empty() {
        println!("\nWarnings:");
        for w in &report.warnings {
            println!("  [*] {w}");
        }
    }
    if !report.remediation_hints.is_empty() {
        println!("\nRemediation Hints:");
        for h in &report.remediation_hints {
            println!("  -> {h}");
        }
    }
}

fn attention_command(args: &[String]) -> Result<(), String> {
    let sub = args.first().map(String::as_str).unwrap_or("spend");
    let as_json = args.iter().any(|a| a == "--json");
    let hours: u32 = args
        .windows(2)
        .find(|w| w[0] == "--hours")
        .and_then(|w| w[1].parse().ok())
        .unwrap_or(24);

    match sub {
        "spend" | "report" => {
            let records = attention_alert::read_spend_records(500);
            let active_window = attention_alert::query_active_window();
            let summary = attention_alert::compile_spend_summary(
                &records,
                hours,
                now_epoch_seconds(),
                active_window,
            );
            if as_json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&summary)
                        .map_err(|e| format!("failed to serialize attention summary: {e}"))?
                );
            } else {
                print_attention_summary(&summary);
            }
            Ok(())
        }
        "status" => {
            let active_window = attention_alert::query_active_window();
            let in_flow = attention_alert::detect_flow_state(active_window.as_ref());
            if as_json {
                let status = serde_json::json!({
                    "active_window": active_window,
                    "in_flow_state": in_flow,
                });
                println!("{}", serde_json::to_string_pretty(&status).unwrap());
            } else {
                println!("🧠 Focus Guardian · Live Cognitive Flow Status");
                if let Some(w) = &active_window {
                    println!("  Focused App   : {}", w.class);
                    println!("  Window Title  : {}", w.title);
                    println!("  Fullscreen    : {}", w.fullscreen);
                } else {
                    println!("  Focused Window: None / Headless");
                }
                println!(
                    "  Flow State    : {}",
                    if in_flow {
                        "🟢 ACTIVE (Non-critical interruptions suppressed)"
                    } else {
                        "⚪ IDLE (Standard attention budget applies)"
                    }
                );
            }
            Ok(())
        }
        "help" | "--help" | "-h" => {
            println!("Usage:");
            println!("  task-fabric-svn-work attention spend [--hours <N>] [--json]");
            println!("  task-fabric-svn-work attention report [--hours <N>] [--json]");
            println!("  task-fabric-svn-work attention status [--json]");
            Ok(())
        }
        other => Err(format!(
            "unknown attention subcommand '{other}'; try 'task-fabric-svn-work attention spend' or 'status'"
        )),
    }
}

fn print_attention_summary(summary: &attention_alert::AttentionSpendSummary) {
    println!(
        "🛡  Focus Guardian · Attention Economics Spend Report (Past {}h)",
        summary.window_hours
    );
    println!("─────────────────────────────────────────────────────────────────────────────");

    // 1. Current Cognitive State
    println!("Live Cognitive Environment:");
    if let Some(w) = &summary.current_window {
        println!("  Focused App   : {}", w.class);
        println!("  Window Title  : {}", w.title);
    } else {
        println!("  Focused App   : none / compositor idle");
    }
    println!(
        "  Flow State    : {}",
        if summary.in_flow_state {
            "🟢 ACTIVE (Engineering flow protected)"
        } else {
            "⚪ IDLE (Ambient listening)"
        }
    );
    println!();

    // 2. Attention Metrics
    println!("Attention Economics & Spend Ledger:");
    println!("  Attention Tokens Spent : {} tokens", summary.total_spend_tokens);
    println!("  Interruption Deliveries: {} delivered", summary.delivered_count);
    println!("  Flow State Suppressions: {} protected", summary.suppressed_flow_count);
    println!("  Cooldown Queue Defers  : {} deferred", summary.suppressed_cooldown_count);
    println!("  Flow Preservation Rate : {:.1}%", summary.flow_preservation_rate);
    println!();

    // 3. Recent Events
    if !summary.recent_records.is_empty() {
        println!("Recent Attention Events:");
        for record in &summary.recent_records {
            let outcome_badge = match record.outcome {
                attention_alert::AttentionSpendOutcome::Delivered => "DELIVERED",
                attention_alert::AttentionSpendOutcome::SuppressedFlow => "PROTECTED",
                attention_alert::AttentionSpendOutcome::SuppressedCooldown => "COOLDOWN",
                attention_alert::AttentionSpendOutcome::BaselineQuiet => "QUIET",
            };
            let app = record.focused_app.as_deref().unwrap_or("unknown");
            println!(
                "  [{:>9}] ({:>2} tokens, urgency: {:<10}) [{}] {}",
                outcome_badge, record.cost_tokens, record.urgency, app, record.summary
            );
        }
    } else {
        println!("Recent Attention Events: (no events recorded in this window)");
    }
    println!("─────────────────────────────────────────────────────────────────────────────");
}

fn enter_command(args: &[String]) -> Result<(), String> {
    let id = args
        .first()
        .ok_or_else(|| "usage: task-fabric-svn-work enter <world-id> [--dry-run] [--plan] [--json]".to_string())?;
    let json_output = args.iter().any(|arg| arg == "--json");
    let dry_run = args.iter().any(|arg| arg == "--dry-run");
    let (config, _) = load_config()?;
    let project = find_project(&config, id).ok_or_else(|| format!("unknown world {id}"))?;

    if dry_run {
        let clients = get_hypr_clients();
        let active_world = request_value(&default_socket_path(), "status")
            .ok()
            .and_then(|v| {
                v.get("active_world_id")
                    .or_else(|| v.get("world_id"))
                    .and_then(|id| id.as_str())
                    .map(String::from)
            });
        let report = worldstate::simulate_transition(project, active_world.as_deref(), clients.as_deref());
        if json_output {
            println!(
                "{}",
                serde_json::to_string_pretty(&report).map_err(|e| e.to_string())?
            );
        } else {
            print_simulation_report(&report);
        }
        return Ok(());
    }

    if args.iter().any(|arg| arg == "--plan") {
        let state = worldstate::state_dir();
        let selected = read_state_world_id(&state.join("selected-world"));
        let active = read_state_world_id(&state.join("active-project"));
        let snapshot = worldstate::snapshot_for_with_state(
            &config,
            selected.as_deref().or(Some(&project.id)),
            Some(&project.id),
            active.as_deref(),
            None,
            None,
        );
        let plan = worldstate::plan_for(project, &snapshot.snapshot_id);
        if json_output {
            println!(
                "{}",
                serde_json::to_string_pretty(&plan).map_err(|e| e.to_string())?
            );
        } else {
            println!("Plan {} for {}", plan.id, plan.world_id);
            for effect in plan.effects {
                println!("  {} · {} · {}", effect.id, effect.kind, effect.target_ref);
            }
        }
        return Ok(());
    }
    let mut value = request_value(
        &default_socket_path(),
        &format!("transition-enter {}", project.id),
    )?;
    if let Ok(projection) = request_runtime_projection(&project.id) {
        value["semantic_projection"] = projection;
    }
    if json_output {
        println!(
            "{}",
            serde_json::to_string_pretty(&value).map_err(|e| e.to_string())?
        );
    } else if let Some(error) = value.get("error").and_then(|v| v.as_str()) {
        return Err(error.into());
    } else {
        println!(
            "Entered {} · outcome {}",
            project.label,
            value
                .get("outcome")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown")
        );
        if let Some(projection) = value.get("semantic_projection") {
            let revision = projection.get("bundle_revision")
                .or_else(|| projection.get("bundle_id"))
                .or_else(|| projection.get("revision"))
                .and_then(|v| v.as_str()).unwrap_or("canonical");
            let mode = projection.get("selected_mode")
                .or_else(|| projection.get("mode"))
                .and_then(|v| v.as_str()).unwrap_or("resume");
            let terminal = projection.get("terminal_projection")
                .or_else(|| projection.get("terminal_variant"))
                .and_then(|v| v.as_str())
                .or_else(|| projection.get("variants").and_then(|v| v.as_array()).and_then(|items| items.iter().find(|item| item.get("mode").and_then(|m| m.as_str()) == Some(mode)).and_then(|item| item.get("text")).and_then(|t| t.as_str())))
                .unwrap_or("Canonical World entry is available.");
            println!("Projection {revision} · {mode}");
            println!("{terminal}");
        }
        if let Some(receipt) = value.get("receipt_id").and_then(|v| v.as_str()) {
            println!("Transition receipt: {receipt}");
        }
        if !json_output {
            if let Ok(output) = Command::new("task-fabric-svn-memory-query")
                .args(["recall", "--world", &project.id, "--limit", "2"])
                .output()
            {
                if output.status.success() {
                    let recall_str = String::from_utf8_lossy(&output.stdout);
                    if !recall_str.trim().is_empty() {
                        println!("\n🧠 {}", recall_str.trim());
                    }
                }
            }
        }
    }

    // Context-Aware Hyprland Spatial World Orchestration
    let workspace_target = format!("name:{}", project.id);
    let lua_dispatch = format!("hl.dsp.focus({{ workspace = \"{}\" }})", workspace_target);
    let hypr_status = Command::new("hyprctl")
        .args(["dispatch", &lua_dispatch])
        .status();
    let workspace_switched = hypr_status.map(|s| s.success()).unwrap_or(false);

    // Automatically launch or attach project surfaces (browser webapp, companion display, dev environments)
    let open_dev = args.iter().any(|a| a == "--dev" || a == "--editor");
    let open_agent = args.iter().any(|a| a == "--dev" || a == "--agent" || a == "--claude" || a == "--codex");
    let has_browser_surface = project.resources.iter().any(|r| r.kind == "browser" || r.kind == "companion" || r.surface_kind.as_deref() == Some("display"));

    for resource in &project.resources {
        if resource.surface_kind.as_deref() == Some("display") {
            let _ = Command::new("dart-heaven-launch")
                .arg("--display")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn();
        } else if resource.kind == "browser" || resource.kind == "companion" || resource.surface_kind.as_deref() == Some("webapp") {
            if resource.target.starts_with("http") {
                let _ = Command::new("omarchy-launch-or-focus-webapp")
                    .args([&project.id, &resource.target])
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn();
            }
        }
    }

    // Dev environment and AI agent orchestration
    if (open_dev || !has_browser_surface) && project.repo.is_dir() {
        let _ = Command::new("omarchy-launch-editor")
            .arg(&project.repo)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
    }
    if open_agent && project.repo.is_dir() {
        let agent_cmd = if command_available("claude") {
            "claude"
        } else if command_available("codex") {
            "codex"
        } else {
            "opencode"
        };
        let _ = Command::new("omarchy-launch-tui")
            .args([
                &format!("--app-id=org.task_fabric_svn.agent.{}", project.id),
                agent_cmd,
            ])
            .current_dir(&project.repo)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
    }

    // Emit structured compositor world-change event
    let event = worldstate::WorldEvent {
        schema_version: 1,
        id: worldstate::new_id("event"),
        timestamp_epoch_seconds: task_fabric_svn_work_core::now_epoch_seconds(),
        world_id: Some(project.id.clone()),
        transition_id: value.get("receipt_id").and_then(|v| v.as_str()).map(|s| s.to_string()),
        trace_id: value.get("trace_id").and_then(|v| v.as_str()).map(|s| s.to_string()),
        intent_id: value.get("intent").and_then(|i| i.get("id")).and_then(|v| v.as_str()).map(|s| s.to_string()),
        kind: "compositor.world_change".to_string(),
        source: "task-fabric-svn-work".to_string(),
        summary: format!("Switched compositor workspace to {} ({})", project.label, workspace_target),
        object_ref: Some(format!("workspace:{}", workspace_target)),
        effect_ref: Some("switch-workspace".to_string()),
        outcome: Some(if workspace_switched {
            worldstate::EffectOutcome::Completed
        } else {
            worldstate::EffectOutcome::Degraded
        }),
        presentation: None,
        evidence_refs: Vec::new(),
    };
    let _ = worldstate::append_event(&event);

    Ok(())
}

fn resume_command(args: &[String]) -> Result<(), String> {
    if args.first().map(String::as_str) == Some("plan") {
        let mut plan_args = args[1..].to_vec();
        plan_args.push("--plan".into());
        return enter_command(&plan_args);
    }
    enter_command(args)
}

fn agent_attach_command(args: &[String]) -> Result<(), String> {
    let id = args
        .first()
        .ok_or_else(|| "usage: task-fabric-svn-work agent-attach <world-id>".to_string())?;
    let (config, _) = load_config()?;
    let project = find_project(&config, id).ok_or_else(|| format!("unknown world {id}"))?;
    let session = project
        .herdr_session
        .as_deref()
        .ok_or_else(|| format!("{} has no named Herdr session", project.id))?;
    if !project.repo.is_dir() {
        return Err(format!(
            "project repository does not exist: {}",
            project.repo.display()
        ));
    }
    let status = Command::new("omarchy-launch-tui")
        .args([
            "--app-id=org.task-fabric-svn.work.herdr",
            "herdr",
            "--session",
            session,
        ])
        .current_dir(&project.repo)
        .status()
        .map_err(|error| format!("could not start Herdr: {error}"))?;
    status
        .success()
        .then_some(())
        .ok_or_else(|| format!("Herdr exited with {status}"))
}

fn read_command(kind: &str, args: &[String]) -> Result<(), String> {
    let suffix = args
        .iter()
        .find(|arg| arg.as_str() != "--json")
        .cloned()
        .unwrap_or_default();
    let request = if suffix.is_empty() {
        kind.to_string()
    } else {
        format!("{kind} {suffix}")
    };
    let value = request_value(&default_socket_path(), &request)?;
    println!(
        "{}",
        serde_json::to_string_pretty(&value).map_err(|e| e.to_string())?
    );
    Ok(())
}

fn study_context_command(args: &[String]) -> Result<(), String> {
    let json_output = args.iter().any(|arg| arg == "--json");
    let config_path = option_path(args, "--config").unwrap_or_else(default_config_path);
    let (config, _) =
        ProjectConfig::from_path_or_example(&config_path).map_err(|error| error.to_string())?;
    let value = request_value(&default_socket_path(), "study-context").unwrap_or_else(|_| {
        serde_json::to_value(task_fabric_svn_work_core::worldstate::study_context_for(
            &config, None,
        ))
        .unwrap_or_else(|_| serde_json::json!({"error":"could not encode study context"}))
    });
    if json_output || value.get("error").is_some() {
        println!(
            "{}",
            serde_json::to_string_pretty(&value).map_err(|e| e.to_string())?
        );
    } else {
        println!(
            "{}",
            serde_json::to_string_pretty(&value).map_err(|e| e.to_string())?
        );
    }
    Ok(())
}

fn select_command(args: &[String]) -> Result<(), String> {
    let project_id = args
        .first()
        .ok_or_else(|| "usage: task-fabric-svn-work select <project-id>".to_string())?;
    let socket_path = default_socket_path();
    let mut stream = UnixStream::connect(&socket_path)
        .map_err(|error| format!("could not connect to task-fabric-svn-workd: {error}"))?;
    stream
        .write_all(format!("select {project_id}\n").as_bytes())
        .map_err(|error| format!("could not request project selection: {error}"))?;
    let mut response = String::new();
    BufReader::new(stream)
        .read_line(&mut response)
        .map_err(|error| format!("could not read project selection: {error}"))?;
    let payload: serde_json::Value =
        serde_json::from_str(&response).map_err(|error| error.to_string())?;
    if let Some(error) = payload.get("error").and_then(|value| value.as_str()) {
        return Err(error.to_string());
    }
    println!("Selected {project_id}");
    Ok(())
}

fn load_config() -> Result<(ProjectConfig, PathBuf), String> {
    let path = default_config_path();
    let (config, _) =
        ProjectConfig::from_path_or_example(&path).map_err(|error| error.to_string())?;
    Ok((config, path))
}

fn find_project<'a>(config: &'a ProjectConfig, id: &str) -> Option<&'a task_fabric_svn_work_core::Project> {
    config.projects.iter().find(|project| {
        project.id.eq_ignore_ascii_case(id)
            || project
                .aliases
                .iter()
                .any(|alias| alias.eq_ignore_ascii_case(id))
    })
}

fn action_command(args: &[String]) -> Result<(), String> {
    action_command_with_config(args, &default_config_path())
}

fn action_command_with_config(args: &[String], config_path: &Path) -> Result<(), String> {
    let project_id = args.first().ok_or_else(action_usage)?;
    let action_id = args.get(1).ok_or_else(action_usage)?;
    for option in args.iter().skip(2) {
        if option != "--confirm" {
            return Err(format!(
                "unknown action option {option}; expected --confirm"
            ));
        }
    }
    let confirmed = args.iter().skip(2).any(|arg| arg == "--confirm");
    let (config, _) =
        ProjectConfig::from_path_or_example(config_path).map_err(|error| error.to_string())?;
    let project =
        find_project(&config, project_id).ok_or_else(|| format!("unknown project {project_id}"))?;
    if action_id.eq_ignore_ascii_case("task-fabric-svn-test") {
        return run_task_fabric_svn_test(project, confirmed);
    }
    let action = project
        .actions
        .iter()
        .find(|action| action.id.eq_ignore_ascii_case(action_id))
        .ok_or_else(|| format!("unknown action {action_id} for {}", project.id))?;
    if action.command.is_empty() {
        return Err(format!("action {} has no command", action.id));
    }
    if action.command.iter().any(|arg| arg.contains('\0')) {
        return Err("action contains a NUL byte".into());
    }
    if !is_supported_action_risk(&action.risk) {
        return Err(format!(
            "unknown risk {} for action {}; expected low or review",
            action.risk, action.id
        ));
    }
    let cwd = action.cwd.as_deref().unwrap_or(project.repo.as_path());
    if !cwd.is_dir() {
        return Err(format!(
            "action working directory does not exist: {}",
            cwd.display()
        ));
    }
    if action.risk.eq_ignore_ascii_case("review") {
        print_action_confirmation(action.id.as_str(), &action.command, cwd, confirmed);
        if !confirmed {
            return Err(format!(
                "action {} requires explicit confirmation; rerun with --confirm",
                action.id
            ));
        }
    }
    let mut command = Command::new(&action.command[0]);
    command.args(&action.command[1..]).current_dir(cwd);
    eprintln!(
        "task-fabric-svn-work: running {} in {}",
        action.command.join(" "),
        cwd.display()
    );
    let status = command
        .status()
        .map_err(|error| format!("could not start {}: {error}", action.command[0]))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("action {} exited with {}", action.id, status))
    }
}

fn action_usage() -> String {
    "usage: task-fabric-svn-work action run <project-id> <action-id> [--confirm] | action dry-run <project-id> <action-id> [--json]".to_string()
}

fn action_dry_run_command(args: &[String]) -> Result<(), String> {
    let project_id = args.first().ok_or_else(|| "usage: task-fabric-svn-work action dry-run <world-id> <action-id> [--json]".to_string())?;
    let action_id = args.get(1).ok_or_else(|| "usage: task-fabric-svn-work action dry-run <world-id> <action-id> [--json]".to_string())?;
    let json_output = args.iter().any(|arg| arg == "--json");
    let (config, _) = load_config()?;
    let project = find_project(&config, project_id).ok_or_else(|| format!("unknown world {project_id}"))?;

    let report = worldstate::simulate_action_dry_run(project, action_id)?;
    if json_output {
        println!("{}", serde_json::to_string_pretty(&report).map_err(|e| e.to_string())?);
    } else {
        println!("✈  Flight Simulator · Action Dry-Run: '{}' ({}) in world '{}'", report.label, report.action_id, report.world_id);
        println!("Status: {} (risk: {}) · ZERO MUTATIONS EXECUTED\n", if report.ready { "READY" } else { "BLOCKED" }, report.risk);
        println!("Command Execution Preview:");
        println!("  Working Directory : {} (exists: {})", report.cwd.display(), report.cwd_exists);
        println!("  Binary Resolved   : {} (exists: {}, executable: {})",
            report.resolved_binary.as_deref().unwrap_or("<not found>"),
            report.binary_exists,
            report.binary_executable
        );
        println!("  Argv              : {:?}", report.command);

        if !report.blockers.is_empty() {
            println!("\nBlockers:");
            for b in &report.blockers {
                println!("  [!] {b}");
            }
        }
        if !report.warnings.is_empty() {
            println!("\nWarnings:");
            for w in &report.warnings {
                println!("  [*] {w}");
            }
        }
    }
    Ok(())
}

fn print_action_confirmation(action_id: &str, command: &[String], cwd: &Path, confirmed: bool) {
    eprintln!(
        "task-fabric-svn-work: review action {action_id}{}\n  argv: {command:?}\n  cwd: {}",
        if confirmed {
            " confirmed"
        } else {
            " requires confirmation"
        },
        cwd.display()
    );
}

fn run_task_fabric_svn_test(project: &task_fabric_svn_work_core::Project, confirmed: bool) -> Result<(), String> {
    if !project.repo.is_dir() {
        return Err(format!(
            "project repository does not exist: {}",
            project.repo.display()
        ));
    }
    let prompt = "Run $durable-value for the current proposal or diff. Use the repository and current user outcome as context; do not read or request Codex transcripts. Return the compact TASK FABRIC SVN TEST format.";
    let command = [
        "omarchy-launch-tui",
        "--app-id=org.task-fabric-svn.work.durable-value",
        "codex",
        "-C",
        project.repo.to_str().unwrap_or("."),
        prompt,
    ];
    let command_strings: Vec<String> = command.iter().map(|arg| (*arg).into()).collect();
    print_action_confirmation("task-fabric-svn-test", &command_strings, &project.repo, confirmed);
    if !confirmed {
        return Err(
            "action task-fabric-svn-test requires explicit confirmation; rerun with --confirm".into(),
        );
    }
    let status = Command::new(command[0])
        .args(&command[1..])
        .current_dir(&project.repo)
        .status()
        .map_err(|error| format!("could not start Codex: {error}"))?;
    status
        .success()
        .then_some(())
        .ok_or_else(|| format!("Codex exited with {status}"))
}

fn signal_command(args: &[String]) -> Result<(), String> {
    match args.first().map(String::as_str) {
        Some("record") => {
            let project_id = args.get(1).ok_or_else(|| {
                "usage: task-fabric-svn-work signal record <project> <kind> [--note TEXT]".to_string()
            })?;
            let kind = args.get(2).ok_or_else(|| {
                "usage: task-fabric-svn-work signal record <project> <kind> [--note TEXT]".to_string()
            })?;
            let note = option_value(args, "--note");
            let (config, _) = load_config()?;
            let project = find_project(&config, project_id)
                .ok_or_else(|| format!("unknown project {project_id}"))?;
            let event = durable::append_event(&project.id, kind, note.as_deref())
                .map_err(|error| error.to_string())?;
            println!(
                "Recorded {} for {} ({})",
                event.kind, event.project_id, event.id
            );
            Ok(())
        }
        Some("list") => {
            let project_id = args
                .get(1)
                .ok_or_else(|| "usage: task-fabric-svn-work signal list <project> [--json]".to_string())?;
            let (config, _) = load_config()?;
            let project = find_project(&config, project_id)
                .ok_or_else(|| format!("unknown project {project_id}"))?;
            let events =
                durable::read_events_for(&project.id).map_err(|error| error.to_string())?;
            if args.iter().any(|arg| arg == "--json") {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&events).map_err(|error| error.to_string())?
                );
            } else if events.is_empty() {
                println!("No reality signals recorded for {project_id}");
            } else {
                for event in events {
                    println!("{}  {}  {}", event.id, event.kind, event.recorded_at);
                }
            }
            Ok(())
        }
        Some("remove") => {
            let project_id = args.get(1).ok_or_else(|| {
                "usage: task-fabric-svn-work signal remove <project> <event-id>".to_string()
            })?;
            let event_id = args.get(2).ok_or_else(|| {
                "usage: task-fabric-svn-work signal remove <project> <event-id>".to_string()
            })?;
            let (config, _) = load_config()?;
            let project = find_project(&config, project_id)
                .ok_or_else(|| format!("unknown project {project_id}"))?;
            if durable::remove_event(&project.id, event_id).map_err(|error| error.to_string())? {
                println!("Removed {event_id}");
            } else {
                return Err(format!("signal {event_id} was not found"));
            }
            Ok(())
        }
        _ => Err("usage: task-fabric-svn-work signal record|list|remove ...".into()),
    }
}

fn improve_command(args: &[String]) -> Result<(), String> {
    let as_json = args.iter().any(|a| a == "--json");
    let as_diff = args.iter().any(|a| a == "--diff");

    let subcommand = args.iter().find(|arg| !arg.starts_with('-')).map(String::as_str);
    let state = worldstate::state_dir();

    if matches!(subcommand, Some("review" | "show")) {
        let proposal_id = args.iter().skip_while(|arg| *arg != "review" && *arg != "show")
            .skip(1).find(|arg| !arg.starts_with('-'))
            .ok_or_else(|| "usage: task-fabric-svn-work improve review <proposal-id> [--json]".to_string())?;
        let projection = task_fabric_svn_work_core::improvement::review(&state, proposal_id)
            .map_err(|error| format!("could not load proposal: {error}"))?
            .ok_or_else(|| format!("proposal {proposal_id} was not found; run `task-fabric-svn-work improve propose` first"))?;
        if as_json {
            println!("{}", serde_json::to_string_pretty(&projection).map_err(|error| error.to_string())?);
        } else {
            print_review_projection(&projection);
        }
        return Ok(());
    }

    if matches!(subcommand, Some("queue-trial")) {
        let proposal_id = args.iter().skip_while(|arg| *arg != "queue-trial").skip(1)
            .find(|arg| !arg.starts_with('-'))
            .ok_or_else(|| "usage: task-fabric-svn-work improve queue-trial <proposal-id>".to_string())?;
        let experiment_id = task_fabric_svn_work_core::improvement::queue_bounded_experiment(&state, proposal_id)
            .map_err(|error| error.to_string())?;
        if as_json { println!("{}", serde_json::json!({"proposal_id": proposal_id, "experiment_id": experiment_id, "executed": false, "owner_approval_required": true})); }
        else { println!("Proposal {proposal_id} queued for bounded experiment {experiment_id}; no target was modified and owner approval remains required."); }
        return Ok(());
    }

    // Optional target world filter (ignore flags and subcommands like 'review' / 'list' / 'propose')
    let target_world = args.iter().find(|a| !a.starts_with("--") && *a != "review" && *a != "list" && *a != "propose").map(String::as_str);

    let (config, _) = load_config().map_err(|e| format!("failed to load config: {e}"))?;
    let events = worldstate::read_events(500).unwrap_or_default();
    let report = worldstate::analyze_workstation_friction(&config, &events, target_world);
    let proposal_report = task_fabric_svn_work_core::improvement::compile(&config, &events, &state, target_world)
        .map_err(|error| format!("failed to compile improvement proposals: {error}"))?;
    task_fabric_svn_work_core::improvement::persist_report(&state, &proposal_report)
        .map_err(|error| format!("failed to persist improvement proposals: {error}"))?;

    if as_json {
        println!("{}", serde_json::to_string_pretty(&serde_json::json!({
            "friction_report": report,
            "improvement_proposals": proposal_report,
        })).map_err(|e| e.to_string())?);
        return Ok(());
    }

    if as_diff {
        println!("# Friction Compiler · Proposed Declarative TOML Patches");
        let patches: Vec<_> = report.patterns.iter().filter_map(|p| p.proposed_toml_patch.as_ref()).collect();
        if patches.is_empty() {
            println!("# No TOML patches required. Workstation configuration is fully harmonized.");
        } else {
            for patch in patches {
                println!("{patch}");
            }
        }
        return Ok(());
    }

    println!("⚡ Friction Compiler · Workstation Friction Analysis");
    if let Some(w) = target_world {
        println!("Target World: {w}");
    } else {
        println!("Target Scope: Workstation-wide ({} worlds analyzed)", config.projects.len());
    }
    println!("Friction Score: {} ({} patterns detected across {} events)\n",
        report.overall_friction_score, report.patterns.len(), report.analyzed_events_count);

    if report.patterns.is_empty() {
        println!("✨ Zero friction detected! All repositories, scratchpads, and actions are harmonized.");
        return Ok(());
    }

    let blockers: Vec<_> = report.patterns.iter().filter(|p| p.severity == worldstate::FrictionSeverity::Blocker).collect();
    let warnings: Vec<_> = report.patterns.iter().filter(|p| p.severity == worldstate::FrictionSeverity::Warning).collect();
    let suggestions: Vec<_> = report.patterns.iter().filter(|p| p.severity == worldstate::FrictionSeverity::Suggestion).collect();

    if !blockers.is_empty() {
        println!("Blockers (Immediate Remediation Required):");
        for b in blockers {
            println!("  [!] [{}] {}: {}", b.world_id, b.title, b.rationale);
        }
        println!();
    }

    if !warnings.is_empty() {
        println!("Warnings (Friction Points):");
        for w in warnings {
            println!("  [*] [{}] {}: {}", w.world_id, w.title, w.rationale);
        }
        println!();
    }

    if !suggestions.is_empty() {
        println!("Declarative Improvements & Suggestions:");
        for s in suggestions {
            let patch_hint = if s.proposed_toml_patch.is_some() { " [patch available]" } else { "" };
            println!("  [+] [{}] {}{}: {}", s.world_id, s.title, patch_hint, s.rationale);
        }
        println!();
    }

    println!("Improvement proposals (durable, owner-reviewed): {}", proposal_report.proposals.len());
    for proposal in &proposal_report.proposals {
        println!("  • {} [{}] {} · recurrence={} · evidence={} · confidence={:?}",
            proposal.proposal_id, format!("{:?}", proposal.status).to_lowercase(),
            proposal.observed_friction, proposal.recurrence.occurrence_count,
            proposal.source_evidence.len(), proposal.confidence);
    }
    println!("Tip: Run `task-fabric-svn-work improve review <proposal-id>` to inspect the causal chain, or `task-fabric-svn-work improve --diff` for legacy TOML patches.");
    Ok(())
}

fn print_review_projection(projection: &task_fabric_svn_work_core::improvement::ProposalReviewProjection) {
    let p = &projection.proposal;
    println!("Proposal: {}", p.proposal_id);
    println!("target: {} / {} ({})", p.target.experiment_target, p.target.component, p.target.component_class);
    println!("\nobserved:\n  {}", p.observed_friction);
    println!("\nevidence:\n  {} occurrences across retained observations", p.recurrence.occurrence_count);
    println!("  source digests:");
    for item in &p.source_evidence { println!("    {} = {} [{}]", item.id, item.digest, format!("{:?}", item.state).to_lowercase()); }
    println!("\nhypothesis:\n  {}", p.falsifiable_hypothesis);
    println!("\npredicted benefit:\n  {}", p.predicted_measurable_effect);
    println!("\nprotected behavior:");
    for invariant in &p.protected_invariants { println!("  - {}", invariant); }
    println!("\nnegative controls:");
    for control in &p.negative_controls { println!("  - {}", control); }
    println!("\nalternatives considered:");
    for alternative in &p.alternatives_considered { println!("  - {}", alternative); }
    println!("\nconfidence: {:?}", p.confidence);
    println!("experiment: {:?}", p.status);
    if let Some(experiment) = &projection.linked_experiment {
        println!("linked experiment: {} ({:?})", experiment.identity.experiment_id, experiment.state);
        if let Some(evaluation) = &experiment.evaluation {
            println!("evaluation: {} -> {} [{:?}]", evaluation.baseline_value.map_or("unknown".into(), |v| v.to_string()), evaluation.candidate_value.map_or("unknown".into(), |v| v.to_string()), evaluation.evidence_state);
            println!("cost: tokens={} monetary_micros={} complete={}", evaluation.cost.usage_tokens.map_or("unknown".into(), |v| v.to_string()), evaluation.cost.monetary_micros.map_or("unknown".into(), |v| v.to_string()), evaluation.cost.is_complete());
        }
    }
    if !p.missing_evidence.is_empty() {
        println!("\nmissing evidence (fail-closed):");
        for missing in &p.missing_evidence { println!("  - {}", missing); }
    }
    println!("\nowner action: inspect / queue trial / reject / approve canary (approval is external)");
}

fn experiment_command(args: &[String]) -> Result<(), String> {
    let state = worldstate::state_dir();
    let value = |flag: &str| {
        args.windows(2)
            .find(|pair| pair[0] == flag)
            .map(|pair| pair[1].clone())
            .ok_or_else(|| format!("missing required {flag}"))
    };
    match args.first().map(String::as_str) {
        Some("queue") => {
            let id = args.get(1).ok_or_else(|| "usage: experiment queue <job-id> --experiment ID --trial ID --run ID --session ID --task ID --harness DIGEST --cwd PATH --timeout N -- COMMAND [ARGS...]".to_string())?;
            if id.is_empty()
                || id.len() > 128
                || !id
                    .chars()
                    .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.'))
            {
                return Err("invalid worker job id".into());
            }
            let separator = args
                .iter()
                .position(|argument| argument == "--")
                .ok_or_else(|| "experiment queue requires -- before the command".to_string())?;
            let command = args[separator + 1..].to_vec();
            if command.is_empty() || command.iter().any(|argument| argument.contains('\0')) {
                return Err("experiment queue requires a non-empty command without NUL bytes".into());
            }
            let timeout: u64 = value("--timeout")?
                .parse()
                .map_err(|error| format!("invalid --timeout: {error}"))?;
            let cwd = value("--cwd")?;
            if !Path::new(&cwd).is_absolute() || !Path::new(&cwd).is_dir() {
                return Err("--cwd must be an existing absolute directory".into());
            }
            let mut environment = serde_json::Map::new();
            for pair in args.windows(2).filter(|pair| pair[0] == "--env") {
                let Some((key, value)) = pair[1].split_once('=') else {
                    return Err("--env must use KEY=VALUE".into());
                };
                if key.is_empty() || key.contains('=') || key.contains('\0') || value.contains('\0') {
                    return Err("invalid --env entry".into());
                }
                environment.insert(key.into(), value.into());
            }
            let release_target = args
                .windows(2)
                .find(|pair| pair[0] == "--release-target")
                .map(|pair| pair[1].clone());
            let expected_manifest_digest = args
                .windows(2)
                .find(|pair| pair[0] == "--manifest")
                .map(|pair| pair[1].clone());
            let router_receipts_path = args
                .windows(2)
                .find(|pair| pair[0] == "--router-receipts")
                .map(|pair| pair[1].clone());
            if let Some(path) = &router_receipts_path {
                if !Path::new(path).is_absolute() || !Path::new(path).is_file() {
                    return Err("--router-receipts must be an existing absolute file".into());
                }
            }
            if release_target.is_some() != expected_manifest_digest.is_some() {
                return Err("--release-target and --manifest must be supplied together".into());
            }
            let job = serde_json::json!({
                "schema_version": 1,
                "job_id": id,
                "experiment_id": value("--experiment")?,
                "trial_id": value("--trial")?,
                "run_id": value("--run")?,
                "session_id": value("--session")?,
                "task_revision": value("--task")?,
                "harness_manifest_digest": value("--harness")?,
                "cwd": cwd,
                "command": command,
                "env": environment,
                "timeout_seconds": timeout,
                "release_target": release_target,
                "expected_manifest_digest": expected_manifest_digest,
                "router_receipts_path": router_receipts_path,
            });
            let inbox = state.join("experiments").join("worker").join("inbox");
            std::fs::create_dir_all(&inbox).map_err(|error| error.to_string())?;
            let path = inbox.join(format!("{id}.json"));
            if path.exists() {
                return Err(format!("worker job {id} already exists"));
            }
            worldstate::atomic_write(&path, &job).map_err(|error| error.to_string())?;
            println!("Experiment worker job {id} queued at {}", path.display());
            Ok(())
        }
        Some("baseline") => {
            let id = args.get(1).ok_or_else(|| "usage: experiment baseline <id> <path>...".to_string())?;
            let paths = args[2..]
                .iter()
                .filter(|arg| arg.as_str() != "--json")
                .map(PathBuf::from)
                .collect::<Vec<_>>();
            if paths.is_empty() {
                return Err("experiment baseline needs at least one path".into());
            }
            let manifest = experiment::manifest_for_paths(&paths).map_err(|e| e.to_string())?;
            experiment::save_manifest(&state, id, &manifest).map_err(|e| e.to_string())?;
            if args.iter().any(|arg| arg == "--json") {
                println!("{}", serde_json::to_string_pretty(&serde_json::json!({
                    "manifest_id": id,
                    "digest": manifest.digest(),
                    "path": experiment::baseline_manifest_path(&state, id).map_err(|e| e.to_string())?,
                    "manifest": manifest,
                })).map_err(|e| e.to_string())?);
            } else {
                println!("Baseline manifest {id} saved: {}", manifest.digest());
            }
            Ok(())
        }
        Some("create") => {
            let id = args.get(1).ok_or_else(|| "usage: experiment create <id> ...".to_string())?.clone();
            let identity = experiment::ExperimentIdentity {
                experiment_id: id.clone(),
                case_id: value("--case")?,
                candidate_id: value("--candidate")?,
                run_id: value("--run")?,
                session_id: value("--session")?,
                task_revision: value("--task")?,
                harness_manifest_digest: value("--harness")?,
                trace_id: args.windows(2).find(|p| p[0] == "--trace").map(|p| p[1].clone()),
            };
            let mut record = experiment::ExperimentRecord::new(
                identity,
                value("--parent-manifest")?,
                value("--candidate-manifest")?,
                value("--hypothesis")?,
                value("--effect")?,
            );
            record.decision = Some("awaiting_evidence".into());
            experiment::save_record(&state, &record).map_err(|e| e.to_string())?;
            println!("Experiment {id} persisted");
            Ok(())
        }
        Some("evaluate") => {
            let id = args.get(1).ok_or_else(|| "usage: experiment evaluate <id> ...".to_string())?;
            let mut record = experiment::load_record(&state, id)
                .map_err(|e| e.to_string())?
                .ok_or_else(|| format!("experiment {id} was not found"))?;
            record.transition(experiment::ExperimentState::Evaluating).map_err(|e| e.to_string())?;
            let parse = |flag: &str| -> Result<Option<u64>, String> {
                args.windows(2)
                    .find(|p| p[0] == flag)
                    .map(|p| p[1].parse::<u64>().map_err(|e| format!("invalid {flag}: {e}")))
                    .transpose()
            };
            let evidence_state = match value("--evidence").unwrap_or_else(|_| "incomplete".into()).as_str() {
                "verified" => experiment::EvidenceState::Verified,
                "accepted" => experiment::EvidenceState::Accepted,
                "inferred" => experiment::EvidenceState::Inferred,
                "incomplete" => experiment::EvidenceState::Incomplete,
                other => return Err(format!("unknown evidence state {other}")),
            };
            let mut cost_evidence = Vec::new();
            for pair in args.windows(2).filter(|pair| pair[0] == "--cost-evidence") {
                let (evidence_id, evidence_path) = pair[1]
                    .split_once('=')
                    .ok_or_else(|| "--cost-evidence must use ID=PATH".to_string())?;
                cost_evidence.push(
                    experiment::evidence_ref_for_file(
                        Path::new(evidence_path),
                        evidence_id.to_string(),
                        now_epoch_seconds(),
                        experiment::EvidenceState::Verified,
                    )
                    .map_err(|e| e.to_string())?,
                );
            }
            record.evaluation = Some(experiment::EvaluationSummary {
                evaluator_digest: value("--evaluator")?,
                results_digest: value("--results")?,
                primary_friction_measure: value("--measure")?,
                baseline_value: parse("--baseline")?,
                candidate_value: parse("--candidate")?,
                quality_regression: args.iter().any(|arg| arg == "--quality-regression"),
                essential_escalation_regression: args.iter().any(|arg| arg == "--essential-escalation-regression"),
                matched_conditions: args.iter().any(|arg| arg == "--matched"),
                evidence_state,
                cost: experiment::CostAccounting {
                    acquisition_seconds: parse("--acquisition-seconds")?,
                    generation_seconds: parse("--generation-seconds")?,
                    evaluation_seconds: parse("--evaluation-seconds")?,
                    review_seconds: parse("--review-seconds")?,
                    maintenance_seconds: parse("--maintenance-seconds")?,
                    usage_tokens: parse("--usage-tokens")?,
                    monetary_micros: parse("--monetary-micros")?,
                    evidence: cost_evidence,
                    unknown_components: args
                        .windows(2)
                        .filter(|pair| pair[0] == "--unknown-cost")
                        .map(|pair| pair[1].clone())
                        .collect(),
                },
            });
            if args.iter().any(|arg| arg == "--inconclusive") {
                record.transition(experiment::ExperimentState::Inconclusive).map_err(|e| e.to_string())?;
            }
            experiment::save_record(&state, &record).map_err(|e| e.to_string())?;
            println!("Experiment {id} recorded as {:?}", record.state);
            Ok(())
        }
        Some("show") => {
            let id = args.get(1).ok_or_else(|| "usage: experiment show <id> [--json]".to_string())?;
            let record = experiment::load_record(&state, id).map_err(|e| e.to_string())?.ok_or_else(|| format!("experiment {id} was not found"))?;
            if args.iter().any(|arg| arg == "--json") {
                println!("{}", serde_json::to_string_pretty(&record).map_err(|e| e.to_string())?);
            } else {
                println!("{id} state={:?} candidate={}", record.state, record.identity.candidate_id);
            }
            Ok(())
        }
        Some("request-approval") => {
            let id = args.get(1).ok_or_else(|| "usage: experiment request-approval <id>".to_string())?;
            let mut record = experiment::load_record(&state, id).map_err(|e| e.to_string())?.ok_or_else(|| format!("experiment {id} was not found"))?;
            record.transition(experiment::ExperimentState::AwaitingApproval).map_err(|e| e.to_string())?;
            experiment::save_record(&state, &record).map_err(|e| e.to_string())?;
            println!("Experiment {id} is awaiting owner approval");
            Ok(())
        }
        Some("recheck") => {
            let id = args.get(1).ok_or_else(|| "usage: experiment recheck <id>".to_string())?;
            let mut record = experiment::load_record(&state, id)
                .map_err(|e| e.to_string())?
                .ok_or_else(|| format!("experiment {id} was not found"))?;
            let incomplete_cost = record
                .evaluation
                .as_ref()
                .is_none_or(|evaluation| !evaluation.cost.is_complete());
            if incomplete_cost && record.state == experiment::ExperimentState::AwaitingApproval {
                record.decision = Some("inconclusive: complete acquisition, generation, evaluation, review, usage, and monetary cost coverage is missing".into());
                record.transition(experiment::ExperimentState::Inconclusive).map_err(|e| e.to_string())?;
                experiment::save_record(&state, &record).map_err(|e| e.to_string())?;
                println!("Experiment {id} returned to inconclusive: cost coverage is incomplete");
            } else {
                println!("Experiment {id} requires no cost-coverage state change");
            }
            Ok(())
        }
        Some("deny") => {
            let id = args.get(1).ok_or_else(|| "usage: experiment deny <id> --reason TEXT".to_string())?;
            let reason = value("--reason")?;
            let mut record = experiment::load_record(&state, id).map_err(|e| e.to_string())?.ok_or_else(|| format!("experiment {id} was not found"))?;
            record.deny(reason).map_err(|e| e.to_string())?;
            experiment::save_record(&state, &record).map_err(|e| e.to_string())?;
            println!("Experiment {id} denied");
            Ok(())
        }
        Some("approve") => {
            let id = args.get(1).ok_or_else(|| "usage: experiment approve <id> ...".to_string())?;
            let mut record = experiment::load_record(&state, id).map_err(|e| e.to_string())?.ok_or_else(|| format!("experiment {id} was not found"))?;
            record.release_authorization = Some(experiment::ReleaseAuthorization {
                candidate_id: value("--candidate")?,
                parent_manifest_digest: value("--parent-manifest")?,
                evaluator_digest: value("--evaluator")?,
                results_digest: value("--results")?,
                target: value("--target")?,
                expires_at_epoch_seconds: value("--expires")?.parse().map_err(|e| format!("invalid --expires: {e}"))?,
                owner_id: value("--owner")?,
                review_reference: value("--review")?,
                approved_at_epoch_seconds: now_epoch_seconds(),
            });
            record.transition(experiment::ExperimentState::Approved).map_err(|e| e.to_string())?;
            experiment::save_record(&state, &record).map_err(|e| e.to_string())?;
            println!("Experiment {id} approved");
            Ok(())
        }
        Some("stage") => {
            let id = args.get(1).ok_or_else(|| "usage: experiment stage <id> --target TARGET --source PATH".to_string())?;
            let target = value("--target")?;
            let source = PathBuf::from(value("--source")?);
            let record = experiment::load_record(&state, id).map_err(|e| e.to_string())?.ok_or_else(|| format!("experiment {id} was not found"))?;
            let staged = experiment::stage_release(&state, &record, &target, &source, now_epoch_seconds()).map_err(|e| e.to_string())?;
            println!("Experiment {id} staged at {}", staged.staged_path);
            Ok(())
        }
        Some("prove-loaded") => {
            let id = args.get(1).ok_or_else(|| "usage: experiment prove-loaded <id> --target TARGET --candidate DIGEST --run RUN_ID".to_string())?;
            let target = value("--target")?;
            let candidate = value("--candidate")?;
            let run_id = value("--run")?;
            let proof = experiment::prove_release_loaded(&state, &target, &candidate, &run_id, now_epoch_seconds()).map_err(|e| e.to_string())?;
            println!("Experiment {id} loaded by fresh run {}", proof.run_id);
            Ok(())
        }
        Some("canary-start") => {
            let id = args.get(1).ok_or_else(|| "usage: experiment canary-start <id> --target TARGET --seconds N --runs N".to_string())?;
            let target = value("--target")?;
            let seconds: u64 = value("--seconds")?.parse().map_err(|e| format!("invalid --seconds: {e}"))?;
            let runs: u32 = value("--runs")?.parse().map_err(|e| format!("invalid --runs: {e}"))?;
            let mut record = experiment::load_record(&state, id).map_err(|e| e.to_string())?.ok_or_else(|| format!("experiment {id} was not found"))?;
            let window = experiment::start_canary(&state, &mut record, &target, now_epoch_seconds(), seconds, runs).map_err(|e| e.to_string())?;
            println!("Experiment {id} canary started until {}", window.ends_at_epoch_seconds);
            Ok(())
        }
        Some("canary-observe") => {
            let id = args.get(1).ok_or_else(|| "usage: experiment canary-observe <id> --run RUN --task TASK --manifest DIGEST --outcome-digest DIGEST".to_string())?;
            let mut record = experiment::load_record(&state, id).map_err(|e| e.to_string())?.ok_or_else(|| format!("experiment {id} was not found"))?;
            let observation = experiment::CanaryRunObservation {
                run_id: value("--run")?,
                task_revision: value("--task")?,
                manifest_digest: value("--manifest")?,
                outcome_digest: value("--outcome-digest")?,
                quality_regression: args.iter().any(|arg| arg == "--quality-regression"),
                observed_at_epoch_seconds: now_epoch_seconds(),
            };
            let result = experiment::observe_canary(&mut record, observation);
            experiment::save_record(&state, &record).map_err(|e| e.to_string())?;
            result.map_err(|e| e.to_string())?;
            println!("Experiment {id} canary observation recorded");
            Ok(())
        }
        Some("fork") => {
            let id = args.get(1).ok_or_else(|| "usage: experiment fork <id> --from ID --case ID --candidate ID --run ID --session ID --task ID --harness DIGEST --candidate-manifest DIGEST --hypothesis TEXT --effect TEXT".to_string())?.clone();
            let predecessor_id = value("--from")?;
            let predecessor = experiment::load_record(&state, &predecessor_id).map_err(|e| e.to_string())?.ok_or_else(|| format!("experiment {predecessor_id} was not found"))?;
            let identity = experiment::ExperimentIdentity {
                experiment_id: id.clone(),
                case_id: value("--case")?,
                candidate_id: value("--candidate")?,
                run_id: value("--run")?,
                session_id: value("--session")?,
                task_revision: value("--task")?,
                harness_manifest_digest: value("--harness")?,
                trace_id: args.windows(2).find(|p| p[0] == "--trace").map(|p| p[1].clone()),
            };
            let successor = experiment::fork_successor_record(&predecessor, identity, value("--candidate-manifest")?, value("--hypothesis")?, value("--effect")?)
                .map_err(|e| e.to_string())?;
            experiment::save_record(&state, &successor).map_err(|e| e.to_string())?;
            println!("Experiment {id} forked from {predecessor_id}");
            Ok(())
        }
        Some("promote") | Some("rollback") => {
            let id = args.get(1).ok_or_else(|| "usage: experiment promote <id> --target TARGET --source PATH | rollback <id> --target TARGET".to_string())?;
            let target = value("--target")?;
            let mut record = experiment::load_record(&state, id).map_err(|e| e.to_string())?.ok_or_else(|| format!("experiment {id} was not found"))?;
            let now = now_epoch_seconds();
            let pointer = if args[0] == "promote" {
                let source = PathBuf::from(value("--source")?);
                let (pointer, staged) = experiment::promote_staged_record(&state, &mut record, &target, &source, now).map_err(|e| e.to_string())?;
                println!("Experiment {id} staged at {}", staged.staged_path);
                pointer
            } else {
                experiment::rollback_record(&state, &mut record, &target, now).map_err(|e| e.to_string())?
            };
            println!("Experiment {id} pointer={}", pointer.active_manifest_digest);
            Ok(())
        }
        Some("inspect-governor") => {
            let run_state = args.get(1).ok_or_else(|| "usage: experiment inspect-governor <run-state.json> <telemetry.jsonl> [--json]".to_string())?;
            let telemetry_path = args.get(2).ok_or_else(|| "usage: experiment inspect-governor <run-state.json> <telemetry.jsonl> [--json]".to_string())?;
            let projection = serde_json::json!({
                "run_state": experiment::load_governor_run_state(Path::new(run_state)).map_err(|e| e.to_string())?,
                "telemetry": experiment::summarize_governor_telemetry(Path::new(telemetry_path)).map_err(|e| e.to_string())?,
                "interventions": experiment::mine_governor_interventions(Path::new(telemetry_path)).map_err(|e| e.to_string())?,
                "evidence_note": "role verdicts remain external evidence; lab success requires executable checks",
            });
            println!("{}", serde_json::to_string_pretty(&projection).map_err(|e| e.to_string())?);
            Ok(())
        }
        Some("inspect-router") => {
            let receipts = args.get(1).ok_or_else(|| "usage: experiment inspect-router <receipts.jsonl> --experiment ID".to_string())?;
            let experiment_id = value("--experiment")?;
            let summary = experiment::summarize_router_receipts(Path::new(receipts), &experiment_id)
                .map_err(|e| e.to_string())?;
            println!("{}", serde_json::to_string_pretty(&summary).map_err(|e| e.to_string())?);
            Ok(())
        }
        Some("inspect-opencode") => {
            let events = args.get(1).ok_or_else(|| "usage: experiment inspect-opencode <events.jsonl>".to_string())?;
            let summary = experiment::summarize_opencode_events(Path::new(events))
                .map_err(|e| e.to_string())?;
            println!("{}", serde_json::to_string_pretty(&summary).map_err(|e| e.to_string())?);
            Ok(())
        }
        Some("attach-router-cost") => {
            let id = args.get(1).ok_or_else(|| "usage: experiment attach-router-cost <id> --receipts PATH".to_string())?;
            let receipts = PathBuf::from(value("--receipts")?);
            let mut record = experiment::load_record(&state, id)
                .map_err(|e| e.to_string())?
                .ok_or_else(|| format!("experiment {id} was not found"))?;
            let summary = experiment::attach_router_receipt_cost(
                &mut record,
                &receipts,
                now_epoch_seconds(),
            )
            .map_err(|e| e.to_string())?;
            experiment::save_record(&state, &record).map_err(|e| e.to_string())?;
            println!(
                "Experiment {id} attached {} router rows ({} tokens, {} micros)",
                summary.attributed_rows,
                summary.prompt_tokens.saturating_add(summary.completion_tokens),
                summary.cost_micros.unwrap_or_default(),
            );
            Ok(())
        }
        Some("collect-result") => {
            let id = args.get(1).ok_or_else(|| "usage: experiment collect-result <id> --result PATH [--manifest DIGEST]".to_string())?;
            let result_path = PathBuf::from(value("--result")?);
            let expected_manifest = args
                .windows(2)
                .find(|pair| pair[0] == "--manifest")
                .map(|pair| pair[1].as_str());
            let mut record = experiment::load_record(&state, id)
                .map_err(|e| e.to_string())?
                .ok_or_else(|| format!("experiment {id} was not found"))?;
            let evidence = experiment::attach_worker_trial_result(
                &mut record,
                &result_path,
                expected_manifest,
                now_epoch_seconds(),
            )
            .map_err(|e| e.to_string())?;
            experiment::save_record(&state, &record).map_err(|e| e.to_string())?;
            println!("Experiment {id} attached worker result {}", evidence.id);
            Ok(())
        }
        Some("attach-artifact") => {
            let id = args.get(1).ok_or_else(|| "usage: experiment attach-artifact <id> --artifact ID=PATH [--state verified|accepted|inferred|incomplete]".to_string())?;
            let artifact = value("--artifact")?;
            let (artifact_id, artifact_path) = artifact
                .split_once('=')
                .ok_or_else(|| "--artifact must use ID=PATH".to_string())?;
            let path = PathBuf::from(artifact_path);
            if !path.is_absolute() || !path.is_file() {
                return Err("artifact path must be an existing absolute file".into());
            }
            let evidence_state = match value("--state").unwrap_or_else(|_| "verified".into()).as_str() {
                "verified" => experiment::EvidenceState::Verified,
                "accepted" => experiment::EvidenceState::Accepted,
                "inferred" => experiment::EvidenceState::Inferred,
                "incomplete" => experiment::EvidenceState::Incomplete,
                other => return Err(format!("unknown evidence state {other}")),
            };
            let mut record = experiment::load_record(&state, id)
                .map_err(|e| e.to_string())?
                .ok_or_else(|| format!("experiment {id} was not found"))?;
            let evidence = experiment::attach_trial_artifact(
                &mut record,
                &path,
                artifact_id.to_string(),
                now_epoch_seconds(),
                evidence_state,
            )
            .map_err(|e| e.to_string())?;
            experiment::save_record(&state, &record).map_err(|e| e.to_string())?;
            println!("Experiment {id} attached trial artifact {}", evidence.id);
            Ok(())
        }
        _ => Err("usage: experiment queue|baseline|create|evaluate|show|request-approval|recheck|approve|deny|stage|prove-loaded|canary-start|canary-observe|fork|promote|rollback|inspect-governor|inspect-router|inspect-opencode|attach-router-cost|collect-result|attach-artifact ...".into()),
    }
}

fn learn_command(args: &[String]) -> Result<(), String> {
    let sub = args.first().map(String::as_str).unwrap_or("help");
    let as_json = args.iter().any(|a| a == "--json");

    match sub {
        "drill" => {
            let world_id = args.get(1).ok_or_else(|| "usage: task-fabric-svn-work learn drill <world-id> [--json]".to_string())?;
            let (config, _) = load_config().map_err(|e| format!("failed to load config: {e}"))?;
            let project = find_project(&config, world_id).ok_or_else(|| format!("unknown world '{world_id}'"))?;
            let events = worldstate::read_events(200).unwrap_or_default();

            // Fetch memory recall context from sqlite if exocortex.db exists
            let mut memories = Vec::new();
            if let Some(home) = std::env::var_os("HOME") {
                let db_path = PathBuf::from(home).join(".local/state/task-fabric-svn/exocortex.db");
                if db_path.is_file() {
                    let query = format!("SELECT title || ': ' || content_text FROM memories WHERE world_id = '{}' ORDER BY created_at DESC LIMIT 3;", project.id);
                    if let Ok(out) = Command::new("sqlite3").arg(&db_path).arg(&query).output() {
                        if out.status.success() {
                            let text = String::from_utf8_lossy(&out.stdout);
                            for line in text.lines() {
                                if !line.trim().is_empty() {
                                    memories.push(line.trim().to_string());
                                }
                            }
                        }
                    }
                }
            }

            let drill = worldstate::generate_socratic_drill(project, &events, &memories);

            if as_json {
                println!("{}", serde_json::to_string_pretty(&drill).map_err(|e| e.to_string())?);
            } else {
                print_socratic_drill(&drill);
            }
            Ok(())
        }
        "list" => {
            let (config, _) = load_config().map_err(|e| format!("failed to load config: {e}"))?;
            println!("🎓 The Tutor · Available World Socratic Drills:");
            for p in &config.projects {
                println!("  • {:<26} (Phase: {:<12}) · {}", p.id, p.phase, p.label);
            }
            println!("\nRun `task-fabric-svn-work learn drill <world-id>` to begin active recall.");
            Ok(())
        }
        "help" | "--help" | "-h" => {
            println!("Usage:");
            println!("  task-fabric-svn-work learn drill <world-id> [--json] (generate 3 Socratic active-recall questions)");
            println!("  task-fabric-svn-work learn list                      (list all worlds available for drilling)");
            Ok(())
        }
        other => Err(format!("unknown learn subcommand '{other}'; try 'task-fabric-svn-work learn drill <world-id>' or 'list'")),
    }
}

fn print_socratic_drill(drill: &worldstate::SocraticDrill) {
    println!("🎓 The Tutor · Socratic Recall Drill: '{}' ({})", drill.world_label, drill.world_id);
    println!("Phase: {} · Objective: {}", drill.phase, drill.objective);
    println!("─────────────────────────────────────────────────────────────────────────────");

    for (idx, q) in drill.questions.iter().enumerate() {
        let category_label = match q.category {
            worldstate::SocraticCategory::MachineInvariant => "Machine Invariants & Substrate",
            worldstate::SocraticCategory::CausalLineage => "Causal Lineage & Recent Events",
            worldstate::SocraticCategory::StrategicAction => "Strategic Action & Objective",
        };
        println!("[Q{}: {}]", idx + 1, category_label);
        println!("❓ {}", q.question);
        println!("💡 Hint: {}", q.hint);
        println!("🎯 Ground Truth: {}", q.ground_truth);
        println!();
    }
    println!("─────────────────────────────────────────────────────────────────────────────");
}

fn status_command(args: &[String]) -> Result<(), String> {
    let json = args.iter().any(|arg| arg == "--json");
    let config_path = option_path(args, "--config").unwrap_or_else(default_config_path);
    let socket_path = option_path(args, "--socket").unwrap_or_else(default_socket_path);
    let (config, using_example) =
        ProjectConfig::from_path_or_example(&config_path).map_err(|error| error.to_string())?;
    let mut status = request_daemon(&socket_path)
        .unwrap_or_else(|| StatusSnapshot::local(&config, &config_path, using_example));
    if status.source == task_fabric_svn_work_core::StatusSource::LocalFallback {
        let state = worldstate::state_dir();
        let selected = read_state_world_id(&state.join("selected-world"));
        let requested = read_state_world_id(&state.join("requested-world"));
        let active = read_state_world_id(&state.join("active-project"));
        let last = worldstate::read_last_transition_reference();
        let snapshot = worldstate::snapshot_for_with_state(
            &config,
            selected.as_deref(),
            requested.as_deref(),
            active.as_deref(),
            None,
            last,
        );
        status.selected_world_id = selected;
        status.requested_world_id = requested;
        status.active_world_id = active;
        status.world_activation_status = snapshot.world_activation_status;
        status.active_trace_id = snapshot.active_trace_id;
        status.last_transition_id = snapshot.last_transition.as_ref().map(|reference| reference.id.clone());
        status.degraded.extend(snapshot.degraded_reasons);
    }
    if status.config_path.is_empty() {
        status.config_path = config_path.display().to_string();
    }
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&status).map_err(|error| error.to_string())?
        );
    } else {
        print_human_status(&status);
    }
    Ok(())
}

fn read_state_world_id(path: &Path) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn request_daemon(socket: &Path) -> Option<StatusSnapshot> {
    let mut stream = UnixStream::connect(socket).ok()?;
    stream.write_all(b"status\n").ok()?;
    let mut response = String::new();
    BufReader::new(stream).read_line(&mut response).ok()?;
    serde_json::from_str::<StatusSnapshot>(&response).ok()
}

fn request_value(socket: &Path, request: &str) -> Result<serde_json::Value, String> {
    let mut stream = UnixStream::connect(socket)
        .map_err(|error| format!("could not connect to task-fabric-svn-workd: {error}"))?;
    stream
        .write_all(format!("{request}\n").as_bytes())
        .map_err(|error| format!("could not write daemon request: {error}"))?;
    // The runtime socket reads a bounded request until the client half-closes
    // its write side. Without this, transition commands block forever while
    // waiting for the runtime's semantic projection response.
    if request.starts_with('{') {
        stream
            .shutdown(std::net::Shutdown::Write)
            .map_err(|error| format!("could not finish daemon request: {error}"))?;
    }
    let mut response = String::new();
    BufReader::new(stream)
        .read_line(&mut response)
        .map_err(|error| format!("could not read daemon response: {error}"))?;
    serde_json::from_str(&response).map_err(|error| format!("invalid daemon response: {error}"))
}

fn request_runtime_projection(world_id: &str) -> Result<serde_json::Value, String> {
    let socket = std::env::var_os("TASK_FABRIC_SVN_RUNTIME_SOCKET")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("XDG_RUNTIME_DIR").map(|path| PathBuf::from(path).join("task_fabric_svn/runtime.sock")))
        .ok_or_else(|| "runtime socket path unavailable".to_string())?;
    let request = serde_json::json!({"kind":"semantic_projection","world_id":world_id});
    let response = request_value(&socket, &request.to_string())?;
    response.get("semantic_projection").cloned().ok_or_else(|| "runtime returned no semantic projection".into())
}

fn menu_provider_command() -> Result<(), String> {
    let config_path = default_config_path();
    let (config, _) = ProjectConfig::from_path_or_example(&config_path)
        .map_err(|e| format!("config error: {e}"))?;
    let status = StatusSnapshot::local(&config, &config_path, false);

    if let Some(project) = &status.project {
        println!(
            "Resume: {} ({})\ttask-fabric-svn-work resume {}",
            project.label, project.next_action, project.id
        );
        for action in &status.actions {
            println!(
                "{}\ttask-fabric-svn-work action run {} {}",
                action.label, project.id, action.id
            );
        }
        println!("Toggle World Scratchpad ({})\ttask-fabric-svn-work scratchpad toggle", project.id);
    } else {
        println!("Toggle Scratchpad\ttask-fabric-svn-work scratchpad toggle");
    }
    println!("Open Task Fabric SVN Horizon\ttask-fabric-svn-work horizon toggle");
    println!("Run Workstation Doctor\ttask-fabric-svn-work doctor");
    println!("Review Improvements\ttask-fabric-svn-work improve review");
    Ok(())
}

fn station_sync_command(args: &[String]) -> Result<(), String> {
    let dry_run = args.iter().any(|a| a == "--dry-run" || a == "-n");
    println!("Task Fabric SVN station sync (dry_run: {})", dry_run);

    let state_dir = worldstate::state_dir();
    if state_dir.exists() {
        println!("  [ok] state directory exists: {}", state_dir.display());
        if !dry_run {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(&state_dir, std::fs::Permissions::from_mode(0o700));
            }
        }
    }

    let units = [
        "task-fabric-svn-workd.service",
        "task-fabric-svn-soundd.service",
        "task-fabric-svn-approval-relay.service",
    ];
    for unit in units {
        let is_active = Command::new("systemctl")
            .args(["--user", "is-active", "--quiet", unit])
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if is_active {
            println!("  [ok] unit {unit} is active");
        } else {
            println!("  [warn] unit {unit} is not active");
        }
    }

    let home = std::env::var("HOME").unwrap_or_default();
    let bindings_path = std::path::PathBuf::from(format!("{home}/.config/hypr/bindings.lua"));
    if bindings_path.exists() {
        let content = std::fs::read_to_string(&bindings_path).unwrap_or_default();
        if content.contains("TASK-FABRIC-SVN") {
            println!("  [ok] hyprland bindings block present");
        } else {
            println!("  [warn] hyprland bindings missing TASK-FABRIC-SVN block");
        }
    }

    println!("Station sync complete.");
    Ok(())
}

fn doctor_command(args: &[String]) -> Result<(), String> {
    let audit_mode = args.iter().any(|a| a == "--audit");
    let json_mode = args.iter().any(|a| a == "--json");
    let config_path = default_config_path();
    let socket_path = default_socket_path();
    let mut failures = 0u32;
    println!("Task Fabric SVN doctor (audit: {})", audit_mode);
    println!("  config: {}", config_path.display());
    match ProjectConfig::from_path_or_example(&config_path) {
        Ok((config, using_example)) => {
            if using_example {
                println!("  [warn] config is absent; pilot defaults are active");
            } else {
                println!("  [ok] config parses");
            }
            for issue in config.validate() {
                println!("  [fail] config: {issue}");
                failures += 1;
            }
            for project in &config.projects {
                if project.repo.is_dir() {
                    println!("  [ok] {} repository exists", project.id);
                } else {
                    println!(
                        "  [warn] {} repository missing: {}",
                        project.id,
                        project.repo.display()
                    );
                }
            }
        }
        Err(error) => {
            println!("  [fail] {error}");
            failures += 1;
        }
    }
    if command_available("git") {
        println!("  [ok] git available");
    } else {
        println!("  [fail] git not available");
        failures += 1;
    }
    if command_available("herdr") {
        println!("  [ok] herdr available");
    } else {
        println!("  [warn] herdr not available; session status will be unknown");
    }
    if request_daemon(&socket_path).is_some() {
        println!("  [ok] daemon responds at {}", socket_path.display());
    } else {
        println!("  [info] daemon unavailable; CLI will use local read-only fallback");
    }

    let state_dir = worldstate::state_dir();
    let selected = read_state_world_id(&state_dir.join("selected-world"));
    let requested = read_state_world_id(&state_dir.join("requested-world"));
    let active = read_state_world_id(&state_dir.join("active-project"));
    let last = worldstate::read_last_transition_reference();
    println!(
        "  [info] world state: selected={} requested={} active={} status={}",
        selected.as_deref().unwrap_or("none"),
        requested.as_deref().unwrap_or("none"),
        active.as_deref().unwrap_or("none"),
        if requested == active && active.is_some() { "active" } else if requested.is_some() { "degraded" } else { "unavailable" }
    );
    if let Some(reference) = last {
        println!("  [info] last transition: {} outcome={} trace={}", reference.id, reference.outcome, reference.trace_id.as_deref().unwrap_or("none"));
    } else {
        println!("  [info] last transition: none");
    }

    if audit_mode {
        let units = [
            "task-fabric-svn-workd.service",
            "task-fabric-svn-soundd.service",
            "task-fabric-svn-approval-relay.service",
            "task-fabric-svn-voice-gateway.service",
        ];
        for unit in units {
            let active = Command::new("systemctl")
                .args(["--user", "is-active", "--quiet", unit])
                .status()
                .map(|s| s.success())
                .unwrap_or(false);
            if active {
                println!("  [ok] service {unit} active");
            } else {
                println!("  [fail] service {unit} inactive");
                failures += 1;
            }
        }
        if state_dir.exists() {
            println!("  [ok] state dir present at {}", state_dir.display());
        } else {
            println!("  [warn] state dir missing at {}", state_dir.display());
        }

        let inhibitor_file = state_dir.join("idle-inhibitor.json");
        if inhibitor_file.exists() {
            if let Ok(text) = std::fs::read_to_string(&inhibitor_file) {
                if let Ok(val) = serde_json::from_str::<serde_json::Value>(&text) {
                    if val.get("active").and_then(|v| v.as_bool()).unwrap_or(false) {
                        println!("  [ok] idle inhibitor: active (power lease held for running agent)");
                    } else {
                        println!("  [ok] idle inhibitor: disarmed (normal power policies active)");
                    }
                } else {
                    println!("  [ok] idle inhibitor: disarmed");
                }
            } else {
                println!("  [ok] idle inhibitor: disarmed");
            }
        } else {
            println!("  [ok] idle inhibitor: ready (systemd-inhibit capable)");
        }

        let exocortex_db = state_dir.join("exocortex.db");
        if exocortex_db.exists() {
            println!("  [ok] exocortex memory db present at {}", exocortex_db.display());
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                if let Ok(meta) = std::fs::metadata(&exocortex_db) {
                    let mode = meta.permissions().mode() & 0o777;
                    if mode == 0o600 {
                        println!("  [ok] exocortex db permissions secure (0600)");
                    } else {
                        println!("  [warn] exocortex db permissions {:o} (expected 0600)", mode);
                    }
                }
            }
        } else {
            println!("  [warn] exocortex memory db missing (run 'task-fabric-svn-work memory ingest')");
        }

        let llama_active = Command::new("systemctl")
            .args(["--user", "is-active", "--quiet", "llama-server.service"])
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if llama_active {
            println!("  [ok] local llama-server active (qwen3-4b)");
        } else {
            println!("  [info] local llama-server inactive (offline LLM synthesis disabled)");
        }
    }

    if failures == 0 {
        if json_mode {
            let value = serde_json::json!({
                "schema_version": 1,
                "audit": audit_mode,
                "authority_reachable": request_daemon(&socket_path).is_some(),
                "selected_world_id": selected,
                "requested_world_id": requested,
                "active_world_id": active,
                "last_transition": worldstate::read_last_transition_reference(),
                "state_dir": state_dir.display().to_string(),
                "status": "ok"
            });
            println!("{}", serde_json::to_string_pretty(&value).unwrap());
        }
        Ok(())
    } else {
        Err(format!("doctor found {failures} blocking issue(s)"))
    }
}

fn command_available(command: &str) -> bool {
    Command::new(command)
        .arg("--version")
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

fn print_example_config() -> Result<(), String> {
    let text = toml::to_string_pretty(&ProjectConfig::example_pilot())
        .map_err(|error| error.to_string())?;
    print!("{text}");
    Ok(())
}

fn audio_command(args: &[String]) -> Result<(), String> {
    let home = std::env::var("HOME").map_err(|_| "HOME is not set".to_string())?;
    let path = std::env::var("TASK_FABRIC_SVN_AUDIO_CONFIG")
        .unwrap_or_else(|_| format!("{home}/.config/task-fabric-svn/audio.toml"));
    let action = args.first().map(String::as_str).unwrap_or("status");
    let _config_lock = if matches!(
        action,
        "mode" | "volume" | "boost" | "speech" | "humor" | "ducking" | "tts" | "voice"
    ) {
        Some(ConfigLock::acquire(Path::new(&path))?)
    } else {
        None
    };
    let mut text = std::fs::read_to_string(&path).unwrap_or_else(|_| "mode = \"full\"\nspeech = true\nmaster_volume = 70\nspeech_volume = 80\nboost_approved = false\nspeech_profile = \"kokoro\"\nhumor = \"dry\"\nsound_dir = \"%h/.local/share/task-fabric-svn/sounds\"\ntemplate_dir = \"%h/.local/share/task-fabric-svn/templates\"\nvoice = \"%h/.local/share/task-fabric-svn/voices/en_US-lessac-medium.onnx\"\nmodel = \"%h/.local/share/task-fabric-svn/models/qwen2.5-1.5b-instruct-q4_k_m.gguf\"\nkokoro_model = \"%h/.local/share/task-fabric-svn/kokoro/kokoro-v1.0.onnx\"\nkokoro_voices = \"%h/.local/share/task-fabric-svn/kokoro/voices-v1.0.bin\"\nkokoro_voice = \"af_heart\"\nmax_speech_per_minute = 3\nmax_speech_per_10_minutes = 8\nmedia_ducking = true\nmedia_duck_percent = 35\n".into());
    match action {
        "status" => {
            if args.iter().any(|arg| arg == "--json") {
                let parsed: toml::Value = toml::from_str(&text)
                    .unwrap_or_else(|_| toml::Value::Table(Default::default()));
                let get = |key: &str| {
                    parsed
                        .get(key)
                        .cloned()
                        .unwrap_or(toml::Value::String(String::new()))
                };
                let value = serde_json::json!({
                    "mode": get("mode").as_str().unwrap_or("full"),
                    "speech": get("speech").as_bool().unwrap_or(true),
                    "master_volume": get("master_volume").as_integer().unwrap_or(70),
                    "speech_volume": get("speech_volume").as_integer().unwrap_or(80),
                    "boost_approved": get("boost_approved").as_bool().unwrap_or(false),
                    "speech_profile": get("speech_profile").as_str().unwrap_or("kokoro"),
                    "humor": get("humor").as_str().unwrap_or("dry"),
                    "humor_kind": get("humor_kind").as_str().unwrap_or(""),
                    "humor_failure": get("humor_failure").as_bool().unwrap_or(false),
                    "humor_failure_rate": get("humor_failure_rate").as_integer().unwrap_or(15),
                    "humor_max_per_10_minutes": get("humor_max_per_10_minutes").as_integer().unwrap_or(0),
                    "phrase_word_cap": get("phrase_word_cap").as_integer().unwrap_or(20),
                    "speech_read_min_words": get("speech_read_min_words").as_integer().unwrap_or(0),
                    "speech_read_cap": get("speech_read_cap").as_integer().unwrap_or(40),
                    "phrases_available": std::path::Path::new(&format!("{home}/.config/task-fabric-svn/phrases.toml")).exists(),
                    "max_speech_per_minute": get("max_speech_per_minute").as_integer().unwrap_or(3),
                    "max_speech_per_10_minutes": get("max_speech_per_10_minutes").as_integer().unwrap_or(8),
                    "media_ducking": get("media_ducking").as_bool().unwrap_or(true),
                    "media_duck_percent": get("media_duck_percent").as_integer().unwrap_or(35),
                    "dnd": std::fs::read_to_string(format!("{home}/.local/state/omarchy/notifications.json")).ok().and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok()).and_then(|v| v.get("dnd").and_then(|d| d.as_bool())).unwrap_or(false),
                    "piper": Command::new("piper")
                        .arg("--help")
                        .output()
                        .map(|output| output.status.success())
                        .unwrap_or(false),
                    "llama": command_available("llama-cli"),
                    "voice_configured": get("voice").as_str().map(|v| std::path::Path::new(&v.replace("%h", &home)).exists()).unwrap_or(false),
                    "model_configured": get("model").as_str().map(|v| std::path::Path::new(&v.replace("%h", &home)).exists()).unwrap_or(false),
                    "kokoro": Command::new("kokoro-tts").arg("--help").output().map(|output| output.status.success()).unwrap_or(false),
                    "kokoro_model_configured": get("kokoro_model").as_str().map(|v| std::path::Path::new(&v.replace("%h", &home)).exists()).unwrap_or(false),
                    "kokoro_voice": get("kokoro_voice").as_str().unwrap_or("af_heart"),
                    "kokoro_voice_override": get("kokoro_voice_override").as_str().unwrap_or(""),
                    "espeak_fallback": get("espeak_fallback").as_bool().unwrap_or(false),
                    "kokoro_voices_by_app": string_table_json(&parsed, "kokoro_voices_by_app"),
                    "kokoro_voices_by_category": string_table_json(&parsed, "kokoro_voices_by_category"),
                    "templates_available": get("template_dir").as_str().map(|v| {
                        std::fs::read_dir(std::path::Path::new(&v.replace("%h", &home)))
                            .ok()
                            .map(|entries| entries.flatten().any(|entry| entry.path().extension().and_then(|ext| ext.to_str()) == Some("oga")))
                            .unwrap_or(false)
                    }).unwrap_or(false)
                });
                println!("{}", value);
                Ok(())
            } else {
                println!("{}", text.trim());
                Ok(())
            }
        }
        "mode" => {
            let mode = args
                .get(1)
                .ok_or_else(|| "usage: task-fabric-svn-work audio mode full|sounds-only|muted".to_string())?;
            if !matches!(mode.as_str(), "full" | "sounds-only" | "muted") {
                return Err("mode must be full, sounds-only, or muted".into());
            }
            text = replace_toml_value(&text, "mode", &format!("\"{mode}\""));
            std::fs::create_dir_all(std::path::Path::new(&path).parent().unwrap())
                .map_err(|e| e.to_string())?;
            write_atomic(&path, &text)?;
            println!("audio mode: {mode}");
            Ok(())
        }
        // Mute/unmute the TTS read-aloud only. Earcons and toasts still play:
        // mode "sounds-only" and "muted" control the whole audio channel, this
        // toggle controls just the speech line (soundd's `speech` key).
        "tts" => {
            let on = args
                .get(1)
                .ok_or_else(|| "usage: task-fabric-svn-work audio tts on|off".to_string())?;
            let enabled = match on.as_str() {
                "on" => true,
                "off" => false,
                _ => return Err("tts must be on or off".into()),
            };
            text = replace_toml_value(&text, "speech", if enabled { "true" } else { "false" });
            std::fs::create_dir_all(std::path::Path::new(&path).parent().unwrap())
                .map_err(|e| e.to_string())?;
            write_atomic(&path, &text)?;
            println!(
                "notification TTS: {}",
                if enabled { "on" } else { "muted, earcons still play" }
            );
            Ok(())
        }
        "volume" => {
            let kind = args.get(1).ok_or_else(|| {
                "usage: task-fabric-svn-work audio volume master|speech VALUE|--delta N".to_string()
            })?;
            if !matches!(kind.as_str(), "master" | "speech") {
                return Err("volume target must be master or speech".into());
            }
            let key = if kind == "master" {
                "master_volume"
            } else {
                "speech_volume"
            };
            let current = text
                .lines()
                .find_map(|line| {
                    line.trim()
                        .strip_prefix(&format!("{key} = "))
                        .and_then(|v| v.parse::<i32>().ok())
                })
                .unwrap_or(70);
            let approved = text
                .lines()
                .find_map(|line| {
                    line.trim()
                        .strip_prefix("boost_approved = ")
                        .and_then(|v| v.parse::<bool>().ok())
                })
                .unwrap_or(false);
            let value = if args.get(2).map(String::as_str) == Some("--delta") {
                current + args.get(3).and_then(|v| v.parse::<i32>().ok()).unwrap_or(0)
            } else {
                args.get(2)
                    .and_then(|v| v.parse::<i32>().ok())
                    .ok_or_else(|| "volume value must be 0..150".to_string())?
            };
            let value = value.clamp(0, if approved { 150 } else { 100 });
            text = replace_toml_value(&text, key, &value.to_string());
            std::fs::create_dir_all(std::path::Path::new(&path).parent().unwrap())
                .map_err(|e| e.to_string())?;
            write_atomic(&path, &text)?;
            println!("{kind} volume: {value}");
            Ok(())
        }
        "boost" => {
            let value = args
                .get(1)
                .ok_or_else(|| "usage: task-fabric-svn-work audio boost approve|revoke".to_string())?;
            let approved = match value.as_str() {
                "approve" => true,
                "revoke" => false,
                _ => return Err("boost must be approve or revoke".into()),
            };
            text = replace_toml_value(
                &text,
                "boost_approved",
                if approved { "true" } else { "false" },
            );
            if !approved {
                for key in ["master_volume", "speech_volume"] {
                    let current = text
                        .lines()
                        .find_map(|line| {
                            line.trim()
                                .strip_prefix(&format!("{key} = "))
                                .and_then(|v| v.parse::<i32>().ok())
                        })
                        .unwrap_or(70)
                        .min(100);
                    text = replace_toml_value(&text, key, &current.to_string());
                }
            }
            std::fs::create_dir_all(std::path::Path::new(&path).parent().unwrap())
                .map_err(|e| e.to_string())?;
            write_atomic(&path, &text)?;
            println!(
                "boosted notification audio: {}",
                if approved {
                    "approved up to 150%"
                } else {
                    "limited to 100%"
                }
            );
            Ok(())
        }
        "speech" => {
            let profile = args.get(1).ok_or_else(|| {
                "usage: task-fabric-svn-work audio speech auto|kokoro|piper|templates|espeak".to_string()
            })?;
            if !matches!(
                profile.as_str(),
                "auto" | "kokoro" | "espeak" | "piper" | "templates"
            ) {
                return Err(
                    "speech profile must be auto, kokoro, espeak, piper, or templates".into(),
                );
            }
            text = replace_toml_value(&text, "speech_profile", &format!("\"{profile}\""));
            std::fs::create_dir_all(std::path::Path::new(&path).parent().unwrap())
                .map_err(|e| e.to_string())?;
            write_atomic(&path, &text)?;
            println!("speech profile: {profile}");
            Ok(())
        }
        "humor" => {
            let value = args
                .get(1)
                .ok_or_else(|| "usage: task-fabric-svn-work audio humor off|dry|playful|blunt|noir|haiku".to_string())?;
            if !matches!(value.as_str(), "off" | "dry" | "playful" | "blunt" | "noir" | "haiku") {
                return Err("humor must be off, dry, playful, blunt, noir, or haiku".into());
            }
            text = replace_toml_value(&text, "humor", &format!("\"{value}\""));
            text = replace_toml_value(&text, "humor_kind", &format!("\"{value}\""));
            write_atomic(&path, &text)?;
            println!("notification humor: {value}");
            Ok(())
        }
        "ducking" => {
            let value = args
                .get(1)
                .ok_or_else(|| "usage: task-fabric-svn-work audio ducking on|off|PERCENT".to_string())?;
            if value == "on" || value == "off" {
                text = replace_toml_value(
                    &text,
                    "media_ducking",
                    if value == "on" { "true" } else { "false" },
                );
            } else {
                let percent = value
                    .parse::<i32>()
                    .map_err(|_| "ducking must be on, off, or 0..80".to_string())?
                    .clamp(0, 80);
                text = replace_toml_value(&text, "media_ducking", "true");
                text = replace_toml_value(&text, "media_duck_percent", &percent.to_string());
            }
            std::fs::create_dir_all(std::path::Path::new(&path).parent().unwrap())
                .map_err(|e| e.to_string())?;
            write_atomic(&path, &text)?;
            println!("media ducking: {value}");
            Ok(())
        }
        "studio" => {
            let slug = args
                .get(1)
                .ok_or_else(|| "usage: task-fabric-svn-work audio studio NAME PHRASE...".to_string())?;
            let phrase = args
                .get(2..)
                .ok_or_else(|| "usage: task-fabric-svn-work audio studio NAME PHRASE...".to_string())?
                .join(" ");
            if phrase.trim().is_empty() || phrase.len() > 160 {
                return Err("studio phrase must be 1..160 characters".into());
            }
            let slug = slugify_audio_name(slug);
            if slug.is_empty() {
                return Err("studio name must contain letters or numbers".into());
            }
            let parsed: toml::Value = toml::from_str(&text).map_err(|e| e.to_string())?;
            let get_string = |key: &str, fallback: &str| {
                parsed
                    .get(key)
                    .and_then(|value| value.as_str())
                    .unwrap_or(fallback)
                    .replace("%h", &home)
            };
            let model = get_string(
                "kokoro_model",
                &format!("{home}/.local/share/task-fabric-svn/kokoro/kokoro-v1.0.onnx"),
            );
            let voices = get_string(
                "kokoro_voices",
                &format!("{home}/.local/share/task-fabric-svn/kokoro/voices-v1.0.bin"),
            );
            let voice = parsed
                .get("kokoro_voice")
                .and_then(|value| value.as_str())
                .unwrap_or("af_heart");
            if !Path::new(&model).exists() || !Path::new(&voices).exists() {
                return Err("Kokoro model or voices are not installed".into());
            }
            let template_dir = get_string(
                "template_dir",
                &format!("{home}/.local/share/task-fabric-svn/templates"),
            );
            std::fs::create_dir_all(&template_dir).map_err(|e| e.to_string())?;
            let wav = std::env::temp_dir().join(format!("task-fabric-svn-studio-{}.wav", std::process::id()));
            let mut tts = Command::new("kokoro-tts")
                .args([
                    "--model", &model, "--voices", &voices, "--voice", voice, "--output",
                ])
                .arg(&wav)
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .map_err(|e| format!("Kokoro launch failed: {e}"))?;
            if let Some(mut stdin) = tts.stdin.take() {
                stdin
                    .write_all(phrase.as_bytes())
                    .map_err(|e| e.to_string())?;
            }
            let status = tts.wait().map_err(|e| e.to_string())?;
            if !status.success() {
                let _ = std::fs::remove_file(&wav);
                return Err("Kokoro could not render the studio phrase".into());
            }
            let target = Path::new(&template_dir).join(format!("{slug}.oga"));
            let encoded =
                Path::new(&template_dir).join(format!(".{slug}-{}.tmp.oga", std::process::id()));
            let status = Command::new("ffmpeg")
                .args(["-hide_banner", "-loglevel", "error", "-y", "-i"])
                .arg(&wav)
                .args([
                    "-af",
                    "loudnorm=I=-18:LRA=7:TP=-2",
                    "-ar",
                    "48000",
                    "-ac",
                    "1",
                    "-c:a",
                    "libopus",
                    "-b:a",
                    "96k",
                ])
                .arg(&encoded)
                .status()
                .map_err(|e| e.to_string())?;
            let _ = std::fs::remove_file(&wav);
            if !status.success() {
                let _ = std::fs::remove_file(&encoded);
                return Err("ffmpeg could not encode the studio template".into());
            }
            if target.exists() {
                let stamp = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|value| value.as_secs())
                    .unwrap_or(0);
                std::fs::copy(&target, target.with_extension(format!("oga.bak.{stamp}")))
                    .map_err(|e| format!("could not back up existing template: {e}"))?;
            }
            std::fs::rename(&encoded, &target).map_err(|e| e.to_string())?;
            println!("saved studio template: {}", target.display());
            Ok(())
        }
        "preview" => {
            if args.get(1).map(String::as_str) == Some("speech") {
                let kind = args.get(2).map(String::as_str).unwrap_or("approval");
                let (app, urgency, summary, task_fabric_svn) = match kind {
                    "approval" => (
                        "Codex",
                        "normal",
                        "Approval required",
                        serde_json::json!({"category":"approval","need":"your approval","context":"speech preview"}),
                    ),
                    "authentication" => (
                        "GitHub",
                        "normal",
                        "Authentication required",
                        serde_json::json!({"category":"approval","need":"authentication","context":"speech preview"}),
                    ),
                    "warning" | "attention" => (
                        "Task Fabric SVN Pulse",
                        "normal",
                        "Service unavailable",
                        serde_json::json!({"category":"warning","need":"a project service needs attention","context":"speech preview","outcome":"degraded"}),
                    ),
                    "blocked" => (
                        "Task Fabric SVN",
                        "normal",
                        "Work is blocked",
                        serde_json::json!({"category":"blocked","need":"needs attention","context":"speech preview","outcome":"degraded"}),
                    ),
                    "critical" => (
                        "notify-send",
                        "critical",
                        "Critical system alert",
                        serde_json::json!({"category":"critical","need":"immediate attention","context":"speech preview","outcome":"failed"}),
                    ),
                    "humor" => (
                        "Task Fabric SVN",
                        "normal",
                        "Humor preview complete",
                        serde_json::json!({"category":"success","context":"speech preview","outcome":"completed"}),
                    ),
                    _ => return Err("speech preview must be approval, authentication, blocked, warning, critical, or humor".into()),
                };
                let id = format!(
                    "{}-{}-{}",
                    if kind == "humor" {
                        "humor-preview"
                    } else {
                        "speech-preview"
                    },
                    kind,
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|value| value.as_micros())
                        .unwrap_or(0)
                );
                let envelope = serde_json::json!({"schema_version":1,"id":id,"timestamp_epoch_ms":0,"app":app,"urgency":urgency,"summary":summary,"body":"","dnd":false,"task_fabric_svn":task_fabric_svn});
                print!("{}", send_audio_envelope(&home, &envelope)?);
                Ok(())
            } else {
                let earcon = args.get(1).ok_or_else(|| "usage: task-fabric-svn-work audio preview <arrival|message|complete|approval|blocked|critical>".to_string())?;
                let dir = std::env::var("TASK_FABRIC_SVN_AUDIO_SOUND_DIR")
                    .unwrap_or_else(|_| format!("{home}/.local/share/task-fabric-svn/sounds"));
                let path = std::path::Path::new(&dir).join(format!("{earcon}.oga"));
                if !path.exists() {
                    return Err(format!("earcon not found: {}", path.display()));
                }
                let status = Command::new("pw-play")
                    .args([
                        "--media-role",
                        "Notification",
                        "--properties",
                        "application.name=Task Fabric SVN Notification Audio",
                    ])
                    .arg(path)
                    .status()
                    .map_err(|e| e.to_string())?;
                if status.success() {
                    Ok(())
                } else {
                    Err("audio preview failed".into())
                }
            }
        }
        "test" => {
            let socket = std::env::var("TASK_FABRIC_SVN_AUDIO_SOCKET").unwrap_or_else(|_| {
                format!(
                    "{}/task-fabric-svn/soundd.sock",
                    std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| format!("{home}/.cache"))
                )
            });
            let envelope = serde_json::json!({"schema_version":1,"id":"manual-test","timestamp_epoch_ms":0,"app":"Task Fabric SVN Audio","urgency":"normal","summary":"Notification audio test","body":"","dnd":false});
            let mut stream = UnixStream::connect(socket)
                .map_err(|e| format!("task-fabric-svn-soundd unavailable: {e}"))?;
            writeln!(stream, "{}", envelope).map_err(|e| e.to_string())?;
            let mut response = String::new();
            BufReader::new(stream)
                .read_to_string(&mut response)
                .map_err(|e| e.to_string())?;
            print!("{response}");
            Ok(())
        }
        "notify" => {
            let socket = std::env::var("TASK_FABRIC_SVN_AUDIO_SOCKET").unwrap_or_else(|_| {
                format!(
                    "{}/task-fabric-svn/soundd.sock",
                    std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| format!("{home}/.cache"))
                )
            });
            let mut payload = String::new();
            // The Omarchy notification plugin keeps this client alive as a
            // one-shot queue worker and writes one newline-delimited envelope.
            // Read exactly one frame; waiting for stdin EOF leaves the QML
            // Process running forever, so its queue never advances past the
            // first notification.
            BufReader::new(std::io::stdin().lock())
                .read_line(&mut payload)
                .map_err(|e| e.to_string())?;
            if payload.len() > 32 * 1024 {
                return Err("notification envelope is too large".into());
            }
            let mut stream = UnixStream::connect(socket)
                .map_err(|e| format!("task-fabric-svn-soundd unavailable: {e}"))?;
            stream
                .write_all(payload.trim().as_bytes())
                .map_err(|e| e.to_string())?;
            stream.write_all(b"\n").map_err(|e| e.to_string())?;
            Ok(())
        }
        "simulate" => {
            let mut app = "Claude Code".to_string();
            let mut kind = "approval".to_string();
            let mut caution = false;
            let mut extra = String::new();
            let mut duration = String::new();
            let mut workspace = "task-fabric-svn".to_string();

            let mut i = 1;
            while i < args.len() {
                match args[i].as_str() {
                    "--app" if i + 1 < args.len() => {
                        app = match args[i + 1].to_lowercase().as_str() {
                            "claude" => "Claude Code".into(),
                            "codex" => "Codex".into(),
                            "gemini" => "Gemini".into(),
                            "opencode" => "OpenCode".into(),
                            _ => args[i + 1].clone(),
                        };
                        i += 2;
                    }
                    "--kind" if i + 1 < args.len() => {
                        kind = args[i + 1].clone();
                        i += 2;
                    }
                    "--caution" => {
                        caution = true;
                        i += 1;
                    }
                    "--extra" if i + 1 < args.len() => {
                        extra = args[i + 1].clone();
                        i += 2;
                    }
                    "--duration" if i + 1 < args.len() => {
                        duration = args[i + 1].clone();
                        i += 2;
                    }
                    "--workspace" if i + 1 < args.len() => {
                        workspace = args[i + 1].clone();
                        i += 2;
                    }
                    _ => { i += 1; }
                }
            }

            let (cat, need, out, risk) = match kind.as_str() {
                "approval" => ("approval", "tool_permission", "", if caution { "high" } else { "low" }),
                "done" => ("success", "none", "completed", "low"),
                "error" => ("blocked", "investigate", "failed", "high"),
                "blocked" => ("blocked", "intervention", "failed", "high"),
                _ => ("routine", "none", "", "low"),
            };

            let summary = if caution {
                format!("Caution: {app} needs approval")
            } else if kind == "approval" {
                format!("{app} needs approval")
            } else if kind == "done" {
                format!("{app} finished turn")
            } else {
                format!("{app} {kind}")
            };

            let extra_final = if !extra.is_empty() {
                extra
            } else if caution {
                "Caution: wants to delete files".into()
            } else if kind == "approval" {
                "wants to run the test suite".into()
            } else if kind == "done" {
                "Completed work on authentication".into()
            } else {
                "An unexpected issue occurred".into()
            };

            let now = now_epoch_ms();
            let envelope = serde_json::json!({
                "schema_version": 1,
                "id": format!("sim-{}", now),
                "timestamp_epoch_ms": now,
                "app": app,
                "urgency": if caution { "critical" } else { "normal" },
                "summary": summary,
                "body": format!("Simulation of {app} {kind}"),
                "dnd": false,
                "extra": extra_final,
                "workspace": workspace,
                "duration": if duration.is_empty() { None } else { Some(duration) },
                "risk": risk,
                "task_fabric_svn": {
                    "category": cat,
                    "need": need,
                    "context": format!("[{workspace}] {app}"),
                    "outcome": out
                }
            });

            println!("Simulating bridge notification for {app} ({kind})...");
            let response = send_audio_envelope(&home, &envelope)?;
            print!("{response}");
            Ok(())
        }
        "voice" => voice_command(&args[1..], &text, &path, &home),
        other => Err(format!("unknown audio command {other}")),
    }
}

fn replace_toml_value(text: &str, key: &str, value: &str) -> String {
    let mut found = false;
    let mut result = text
        .lines()
        .map(|line| {
            if line.trim_start().starts_with(&format!("{key} "))
                || line.trim_start().starts_with(&format!("{key}="))
            {
                found = true;
                format!("{key} = {value}")
            } else {
                line.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    if !found {
        if !result.is_empty() {
            result.push('\n');
        }
        result.push_str(&format!("{key} = {value}"));
    }
    result.push('\n');
    result
}

fn write_atomic(path: &str, text: &str) -> Result<(), String> {
    let target = Path::new(path);
    let parent = target
        .parent()
        .ok_or_else(|| "audio configuration path has no parent".to_string())?;
    std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    let temporary = parent.join(format!(".audio.toml.{}.tmp", std::process::id()));
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(&temporary)
        .map_err(|e| e.to_string())?;
    file.write_all(text.as_bytes()).map_err(|e| e.to_string())?;
    file.sync_all().map_err(|e| e.to_string())?;
    std::fs::rename(&temporary, target).map_err(|e| e.to_string())
}

fn string_table(parsed: &toml::Value, key: &str) -> HashMap<String, String> {
    let mut result = HashMap::new();
    if let Some(table) = parsed.get(key).and_then(toml::Value::as_table) {
        for (k, v) in table.iter() {
            if let Some(s) = v.as_str() {
                result.insert(k.clone(), s.into());
            }
        }
    }
    result
}

fn string_table_json(parsed: &toml::Value, key: &str) -> serde_json::Value {
    let mut pairs = Vec::new();
    if let Some(table) = parsed.get(key).and_then(toml::Value::as_table) {
        for (k, v) in table.iter() {
            if let Some(s) = v.as_str() {
                pairs.push(format!("\"{k}\":\"{s}\""));
            }
        }
    }
    serde_json::from_str::<serde_json::Value>(&format!("{{{}}}", pairs.join(",")))
        .unwrap_or_else(|_| serde_json::json!({}))
}

fn voice_resolve(parsed: &toml::Value, app: &str, category: &str) -> String {
    let global_override = parsed
        .get("kokoro_voice_override")
        .and_then(toml::Value::as_str)
        .unwrap_or("");
    if !global_override.is_empty() {
        return global_override.to_string();
    }
    if !app.is_empty() {
        let by_app = string_table(parsed, "kokoro_voices_by_app");
        let key = app.to_lowercase();
        if let Some(voice) = by_app.get(&key) {
            return voice.clone();
        }
    }
    if !category.is_empty() {
        let by_category = string_table(parsed, "kokoro_voices_by_category");
        let key = category.to_lowercase();
        if let Some(voice) = by_category.get(&key) {
            return voice.clone();
        }
    }
    parsed
        .get("kokoro_voice")
        .and_then(toml::Value::as_str)
        .unwrap_or("af_heart")
        .to_string()
}

fn list_kokoro_voices(path: &str) -> Result<Vec<String>, String> {
    if !Path::new(path).exists() {
        return Err(format!("kokoro voices archive not found: {path}"));
    }
    let output = Command::new("unzip")
        .args(["-Z1", path])
        .output()
        .map_err(|e| e.to_string())?;
    if !output.status.success() {
        return Err(format!("could not read kokoro voices archive: {path}"));
    }
    let mut voices = Vec::new();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let trimmed = line.trim();
        if trimmed.ends_with(".npy") && trimmed.len() > 4 {
            voices.push(trimmed[..trimmed.len() - 4].to_string());
        }
    }
    Ok(voices)
}

fn set_table_entry(text: &str, table: &str, key: &str, value: &str) -> String {
    let header = format!("[{table}]");
    let mut output = Vec::new();
    let mut inside = false;
    let mut saw_header = false;
    let mut wrote_entry = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            if inside && !wrote_entry {
                output.push(format!("{key} = \"{value}\""));
                wrote_entry = true;
            }
            inside = trimmed == header;
            if inside {
                saw_header = true;
            }
            output.push(line.to_string());
            continue;
        }
        if inside {
            let Some(eq) = trimmed.find('=') else {
                output.push(line.to_string());
                continue;
            };
            let k = trimmed[..eq].trim();
            if k == key && !wrote_entry {
                output.push(format!("{key} = \"{value}\""));
                wrote_entry = true;
                continue;
            }
        }
        output.push(line.to_string());
    }
    if inside && !wrote_entry {
        output.push(format!("{key} = \"{value}\""));
    }
    if !saw_header {
        output.push(header);
        output.push(format!("{key} = \"{value}\""));
    }
    output.join("\n") + "\n"
}

fn remove_table_entry(text: &str, table: &str, key: &str) -> Result<String, String> {
    let header = format!("[{table}]");
    let lines = text.lines().collect::<Vec<_>>();
    let mut section_start = None;
    let mut entry_line = None;
    let mut entry_count = 0;
    let mut inside = false;
    for i in 0..lines.len() {
        let trimmed = lines[i].trim();
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            if inside {
                break;
            }
            inside = trimmed == header;
            if inside {
                section_start = Some(i);
            }
            continue;
        }
        if inside {
            let Some(eq) = trimmed.find('=') else {
                continue;
            };
            let k = trimmed[..eq].trim();
            if k == key {
                entry_line = Some(i);
            } else if !k.is_empty() {
                entry_count += 1;
            }
        }
    }
    if entry_line.is_none() {
        return Err(format!("no voice override set for {key} in [{table}]"));
    }
    let mut output = Vec::new();
    for i in 0..lines.len() {
        if i == entry_line.unwrap() {
            continue;
        }
        if section_start.is_some() && i == section_start.unwrap() && entry_count == 0 {
            continue;
        }
        output.push(lines[i].to_string());
    }
    Ok(output.join("\n"))
}

fn set_top_level_key(text: &str, key: &str, value: &str) -> String {
    let mut inside_table = false;
    let mut replaced = false;
    let mut inserted = false;
    let mut output = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            inside_table = true;
            if !inserted && !replaced {
                output.push(format!("{key} = {value}"));
                inserted = true;
            }
            output.push(line.to_string());
            continue;
        }
        if !inside_table
            && !replaced
            && (trimmed.starts_with(&format!("{key} "))
                || trimmed.starts_with(&format!("{key}=")))
        {
            output.push(format!("{key} = {value}"));
            replaced = true;
            continue;
        }
        output.push(line.to_string());
    }
    if !replaced && !inserted {
        output.push(format!("{key} = {value}"));
    }
    output.join("\n") + "\n"
}

fn remove_toml_key(text: &str, key: &str) -> String {
    let mut inside_table = false;
    let mut output = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            inside_table = true;
        }
        if !inside_table
            && (trimmed.starts_with(&format!("{key} "))
                || trimmed.starts_with(&format!("{key}=")))
        {
            continue;
        }
        output.push(line.to_string());
    }
    output.join("\n") + "\n"
}

fn preview_kokoro_voice(
    voices_path: &str,
    model_path: &str,
    voice: &str,
    text: &str,
    volume: u16,
) -> Result<(), String> {
    let home = std::env::var("HOME").map_err(|_| "HOME is not set".to_string())?;
    let cache = format!("{home}/.cache/task-fabric-svn/speech-preview");
    std::fs::create_dir_all(Path::new(&cache)).map_err(|e| e.to_string())?;
    let key = format!("{:x}", fnv1a(format!("{voice}\n{text}\n{volume}").as_bytes()));
    let wav = format!("{cache}/{key}.wav");
    if !Path::new(&wav).exists() {
        let mut child = match Command::new("kokoro-tts")
            .args([
                "--model", model_path, "--voices", voices_path, "--voice", voice, "--output",
            ])
            .arg(&wav)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(value) => value,
            Err(_) => return Err("kokoro-tts unavailable".into()),
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
            if start.elapsed() > Duration::from_secs(20) {
                let _ = child.kill();
                let _ = child.wait();
                return Err("kokoro-tts timed out synthesizing preview".into());
            }
            thread::sleep(Duration::from_millis(25));
        }
        if !succeeded {
            return Err("kokoro-tts failed to synthesize preview".into());
        }
    }
    let status = Command::new("pw-play")
        .args([
            "--media-role", "Notification", "--properties", "application.name=Task Fabric SVN Notification Preview",
            "--volume", &volume.to_string(),
        ])
        .arg(&wav)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|e| e.to_string())?;
    if status.success() {
        Ok(())
    } else {
        Err("pw-play failed to play preview".into())
    }
}

fn voice_command(args: &[String], text: &str, path: &str, home: &str) -> Result<(), String> {
    let sub = args.first().map(String::as_str).unwrap_or("list");
    let parsed: toml::Value = toml::from_str(text)
        .unwrap_or_else(|_| toml::Value::Table(Default::default()));
    let get = |key: &str| {
        parsed.get(key).cloned().unwrap_or(toml::Value::String(String::new()))
    };
    let default_voices =
        format!("{home}/.local/share/task-fabric-svn/kokoro/voices-v1.0.bin");
    let voices_path = get("kokoro_voices")
        .as_str()
        .unwrap_or(&default_voices)
        .replace("%h", home);
    let default_model =
        format!("{home}/.local/share/task-fabric-svn/kokoro/kokoro-v1.0.onnx");
    let model_path = get("kokoro_model")
        .as_str()
        .unwrap_or(&default_model)
        .replace("%h", home);

    match sub {
        "list" => {
            let voices = list_kokoro_voices(&voices_path)?;
            let global_override = get("kokoro_voice_override").as_str().unwrap_or("").to_string();
            let fallback = get("kokoro_voice").as_str().unwrap_or("af_heart").to_string();
            let by_app = string_table(&parsed, "kokoro_voices_by_app");
            let by_category = string_table(&parsed, "kokoro_voices_by_category");
            if !global_override.is_empty() {
                println!("global override: {global_override}");
            }
            println!("default fallback: {fallback}");
            println!("per-app:");
            for (app, voice) in by_app.iter() {
                println!("  {app} -> {voice}");
            }
            println!("per-category:");
            for (category, voice) in by_category.iter() {
                println!("  {category} -> {voice}");
            }
            println!("\navailable voices ({}):", voices.len());
            for voice in voices {
                println!("  {voice}");
            }
            Ok(())
        }
        "preview" => {
            let mut voice: Option<String> = None;
            let mut app = String::new();
            let mut category = String::new();
            let mut i = 1;
            while i < args.len() {
                match args[i].as_str() {
                    "--voice" if i + 1 < args.len() => {
                        voice = Some(args[i + 1].clone());
                        i += 2;
                    }
                    "--app" if i + 1 < args.len() => {
                        app = args[i + 1].clone();
                        i += 2;
                    }
                    "--category" if i + 1 < args.len() => {
                        category = args[i + 1].clone();
                        i += 2;
                    }
                    _ => {
                        i += 1;
                    }
                }
            }
            let selected = voice.unwrap_or(voice_resolve(&parsed, &app, &category));
            let voices = list_kokoro_voices(&voices_path)?;
            if !voices.iter().any(|v| v == &selected) {
                return Err(format!(
                    "unknown voice {selected}; see `task-fabric-svn-work audio voice list`"
                ));
            }
            let volume = get("speech_volume").as_integer().unwrap_or(80).clamp(1, 150);
            preview_kokoro_voice(
                &voices_path,
                &model_path,
                &selected,
                "Task finished in task-fabric-svn-notify-bridges. No dragons detected.",
                volume as u16,
            )?;
            Ok(())
        }
        "set" => {
            let id = args
                .get(1)
                .map(String::as_str)
                .unwrap_or("")
                .to_string();
            if id.is_empty() {
                return Err(
                    "usage: task-fabric-svn-work audio voice set ID [--app APP | --category CAT | --global]"
                        .to_string(),
                );
            }
            let mut app: Option<String> = None;
            let mut category: Option<String> = None;
            let mut global = false;
            let mut i = 2;
            while i < args.len() {
                match args[i].as_str() {
                    "--app" if i + 1 < args.len() => {
                        app = Some(args[i + 1].to_lowercase());
                        i += 2;
                    }
                    "--category" if i + 1 < args.len() => {
                        category = Some(args[i + 1].to_lowercase());
                        i += 2;
                    }
                    "--global" => {
                        global = true;
                        i += 1;
                    }
                    _ => {
                        i += 1;
                    }
                }
            }
            let scopes = (if app.is_some() { 1 } else { 0 })
                + (if category.is_some() { 1 } else { 0 })
                + (if global { 1 } else { 0 });
            if scopes != 1 {
                return Err(
                    "usage: task-fabric-svn-work audio voice set ID --app APP | --category CAT | --global (pick one scope)"
                        .to_string(),
                );
            }
            let voices = list_kokoro_voices(&voices_path)?;
            if !voices.iter().any(|v| v == &id) {
                return Err(format!("unknown voice {id}; see `task-fabric-svn-work audio voice list`"));
            }
            let (updated, scope) = if global {
                (
                    set_top_level_key(text, "kokoro_voice_override", &format!("\"{id}\"")),
                    "global".into(),
                )
            } else if let Some(app) = app {
                (
                    set_table_entry(text, "kokoro_voices_by_app", app.as_str(), id.as_str()),
                    format!("app {app}"),
                )
            } else if let Some(category) = category {
                (
                    set_table_entry(
                        text, "kokoro_voices_by_category", category.as_str(), id.as_str(),
                    ),
                    format!("category {category}"),
                )
            } else {
                (text.to_string(), "none".into())
            };
            std::fs::create_dir_all(std::path::Path::new(path).parent().unwrap())
                .map_err(|e| e.to_string())?;
            write_atomic(path, &updated)?;
            println!("voice {id} set for {scope}");
            Ok(())
        }
        "unset" => {
            let mut app: Option<String> = None;
            let mut category: Option<String> = None;
            let mut global = false;
            let mut i = 1;
            while i < args.len() {
                match args[i].as_str() {
                    "--app" if i + 1 < args.len() => {
                        app = Some(args[i + 1].to_lowercase());
                        i += 2;
                    }
                    "--category" if i + 1 < args.len() => {
                        category = Some(args[i + 1].to_lowercase());
                        i += 2;
                    }
                    "--global" => {
                        global = true;
                        i += 1;
                    }
                    _ => {
                        i += 1;
                    }
                }
            }
            let scopes = (if app.is_some() { 1 } else { 0 })
                + (if category.is_some() { 1 } else { 0 })
                + (if global { 1 } else { 0 });
            if scopes != 1 {
                return Err(
                    "usage: task-fabric-svn-work audio voice unset --app APP | --category CAT | --global"
                        .to_string(),
                );
            }
            let updated = if global {
                remove_toml_key(text, "kokoro_voice_override")
            } else if let Some(app) = app {
                remove_table_entry(text, "kokoro_voices_by_app", app.as_str())?
            } else if let Some(category) = category {
                remove_table_entry(text, "kokoro_voices_by_category", category.as_str())?
            } else {
                text.to_string()
            };
            std::fs::create_dir_all(std::path::Path::new(path).parent().unwrap())
                .map_err(|e| e.to_string())?;
            write_atomic(path, &updated)?;
            println!("voice unset");
            Ok(())
        }
        other => Err(format!("unknown voice command {other}")),
    }
}

struct ConfigLock(PathBuf);

impl ConfigLock {
    fn acquire(config: &Path) -> Result<Self, String> {
        let lock = config.with_extension("toml.lock");
        if let Some(parent) = lock.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        for _ in 0..50 {
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&lock)
            {
                Ok(_) => return Ok(Self(lock)),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
                Err(error) => return Err(error.to_string()),
            }
        }
        Err("audio configuration is busy; try again".into())
    }
}

impl Drop for ConfigLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn slugify_audio_name(value: &str) -> String {
    let mut result = String::new();
    for character in value.to_lowercase().chars() {
        if character.is_ascii_alphanumeric() {
            result.push(character);
        } else if !result.ends_with('-') {
            result.push('-');
        }
    }
    result.trim_matches('-').to_string()
}

fn send_audio_envelope(home: &str, envelope: &serde_json::Value) -> Result<String, String> {
    let socket = std::env::var("TASK_FABRIC_SVN_AUDIO_SOCKET").unwrap_or_else(|_| {
        format!(
            "{}/task-fabric-svn/soundd.sock",
            std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| format!("{home}/.cache"))
        )
    });
    let mut stream =
        UnixStream::connect(socket).map_err(|e| format!("task-fabric-svn-soundd unavailable: {e}"))?;
    writeln!(stream, "{}", envelope).map_err(|e| e.to_string())?;
    let mut response = String::new();
    BufReader::new(stream)
        .read_to_string(&mut response)
        .map_err(|e| e.to_string())?;
    Ok(response)
}

fn option_path(args: &[String], flag: &str) -> Option<PathBuf> {
    args.windows(2)
        .find(|pair| pair[0] == flag)
        .map(|pair| pair[1].clone().into())
}

fn option_value(args: &[String], flag: &str) -> Option<String> {
    args.windows(2)
        .find(|pair| pair[0] == flag)
        .map(|pair| pair[1].clone())
}

fn now_epoch_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn print_human_status(status: &StatusSnapshot) {
    println!("TASK-FABRIC-SVN WORKSTATION · {:?}", status.source);
    println!(
        "WORLD     selected={} · requested={} · active={} · {}",
        status.selected_world_id.as_deref().unwrap_or("none"),
        status.requested_world_id.as_deref().unwrap_or("none"),
        status.active_world_id.as_deref().unwrap_or("none"),
        status.world_activation_status
    );
    if let Some(trace_id) = status.active_trace_id.as_deref() {
        println!("TRACE     {trace_id}");
    }
    match &status.project {
        Some(project) => {
            println!(
                "NOW       {} · {} · {}",
                project.label, project.phase, project.branch
            );
            println!("OBJECTIVE {}", project.objective);
            println!("NEXT      {}", project.next_action);
            println!(
                "GIT       {}{}",
                if project.dirty { "dirty" } else { "clean" },
                project
                    .herdr_session
                    .as_deref()
                    .map(|session| format!(" · Herdr {session}"))
                    .unwrap_or_default()
            );
        }
        None => println!("NOW       no project configured"),
    }
    for agent in &status.agents {
        let dur = agent.duration.as_deref().filter(|d| !d.trim().is_empty()).map(|d| format!(" ({d})")).unwrap_or_default();
        let detail = agent.detail.as_deref().map(|d| format!(" · {d}")).unwrap_or_default();
        println!("AGENT     {} · {}{dur}{detail}", agent.label, agent.state);
    }
    for service in &status.services {
        println!("SERVICE   {} · {}", service.id, service.health);
    }
    for item in &status.attention {
        println!("ATTENTION {} · {}", item.title, item.detail);
    }
    if let Some(durable) = &status.durable_value {
        println!("TASK FABRIC SVN TEST {} · {}", durable.level, durable.summary);
        for reason in &durable.reasons {
            println!("  · {reason}");
        }
    }
    if !status.degraded.is_empty() {
        println!("DEGRADED  {}", status.degraded.join("; "));
    }
}

fn why_command(args: &[String]) -> Result<(), String> {
    let json_output = args.iter().any(|arg| arg == "--json");
    let target_ref = args.iter().find(|arg| !arg.starts_with("--")).map(String::as_str);

    let state = worldstate::state_dir();
    let transitions_dir = state.join("world-transitions");

    // 1. Locate receipt
    let receipt: Option<worldstate::WorldTransitionReceipt> = if let Some(r) = target_ref {
        // Try reading directly as a receipt ID
        if let Ok(Some(rcpt)) = worldstate::read_receipt(r) {
            Some(rcpt)
        } else {
            // Search world-transitions/*.json
            let mut found: Option<worldstate::WorldTransitionReceipt> = None;
            if let Ok(entries) = std::fs::read_dir(&transitions_dir) {
                let mut receipts = Vec::new();
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.extension().and_then(|e| e.to_str()) == Some("json") {
                        if let Ok(bytes) = std::fs::read(&path) {
                            if let Ok(rcpt) = serde_json::from_slice::<worldstate::WorldTransitionReceipt>(&bytes) {
                                receipts.push(rcpt);
                            }
                        }
                    }
                }
                // Sort by started_at descending
                receipts.sort_by(|a, b| b.started_at_epoch_seconds.cmp(&a.started_at_epoch_seconds));
                for rcpt in receipts {
                    if rcpt.id == r
                        || rcpt.transition_id == r
                        || rcpt.world_id.eq_ignore_ascii_case(r)
                        || rcpt.trace_id.as_deref() == Some(r)
                    {
                        found = Some(rcpt);
                        break;
                    }
                }
            }
            found
        }
    } else {
        // Find newest receipt across world-transitions
        let mut newest: Option<worldstate::WorldTransitionReceipt> = None;
        if let Ok(entries) = std::fs::read_dir(&transitions_dir) {
            let mut receipts = Vec::new();
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) == Some("json") {
                    if let Ok(bytes) = std::fs::read(&path) {
                        if let Ok(rcpt) = serde_json::from_slice::<worldstate::WorldTransitionReceipt>(&bytes) {
                            receipts.push(rcpt);
                        }
                    }
                }
            }
            receipts.sort_by(|a, b| b.started_at_epoch_seconds.cmp(&a.started_at_epoch_seconds));
            newest = receipts.into_iter().next();
        }
        newest
    };

    let receipt = receipt.ok_or_else(|| {
        format!(
            "no matching transition receipt found{}",
            target_ref.map(|r| format!(" for '{r}'")).unwrap_or_default()
        )
    })?;

    // 2. Fetch related events from world-events.jsonl
    let all_events = worldstate::read_events(200).unwrap_or_default();
    let related_events: Vec<worldstate::WorldEvent> = all_events
        .into_iter()
        .filter(|e| {
            receipt.event_ids.contains(&e.id)
                || e.transition_id.as_deref() == Some(&receipt.transition_id)
                || (receipt.trace_id.is_some() && e.trace_id.as_deref() == receipt.trace_id.as_deref())
        })
        .collect();

    // 3. Format output
    if json_output {
        let output = json!({
            "trace_id": receipt.trace_id,
            "transition_id": receipt.transition_id,
            "world_id": receipt.world_id,
            "outcome": receipt.outcome,
            "duration_ms": receipt.duration_ms,
            "started_at": receipt.started_at_epoch_seconds,
            "finished_at": receipt.finished_at_epoch_seconds,
            "intent": receipt.intent,
            "effects": receipt.effects,
            "events": related_events,
            "before_snapshot": receipt.before_snapshot.snapshot_id,
            "after_snapshot": receipt.after_snapshot.snapshot_id,
        });
        println!("{}", serde_json::to_string_pretty(&output).map_err(|e| e.to_string())?);
        return Ok(());
    }

    let trace_str = receipt.trace_id.as_deref().unwrap_or(&receipt.transition_id);
    println!("🧵 The Loom · Causal Lineage for [{}]", trace_str);
    println!(
        "World: {} · Outcome: {} · Duration: {}ms",
        receipt.world_id, receipt.outcome, receipt.duration_ms
    );
    println!("\nCausal Timeline:");

    let mut step = 1;
    if let Some(intent) = &receipt.intent {
        let caller_str = match &intent.caller {
            worldstate::IntentCaller::HumanCli => "human_cli".to_string(),
            worldstate::IntentCaller::VoiceGateway => "voice_gateway".to_string(),
            worldstate::IntentCaller::AgentSession { agent_name, session_id } => {
                format!("agent:{agent_name} ({session_id})")
            }
            worldstate::IntentCaller::SystemTimer => "system_timer".to_string(),
        };
        println!("  [{step}] INTENT    Caller: {caller_str} · Summary: {}", intent.summary);
        if let Some(prompt) = &intent.raw_prompt {
            println!("               Prompt: \"{prompt}\"");
        }
        if let Some(auth) = &intent.authorization_ref {
            println!("               Auth: {auth}");
        }
        step += 1;
    }

    println!(
        "  [{step}] PLAN      Base Snapshot: {} -> Target: {}",
        receipt.before_snapshot.snapshot_id, receipt.world_id
    );
    step += 1;

    for effect in &receipt.effects {
        println!(
            "  [{step}] EFFECT    {} · outcome: {:?} · detail: {}",
            effect.effect_id, effect.outcome, effect.detail
        );
        step += 1;
    }

    for event in &related_events {
        println!(
            "  [{step}] EVENT     {} · {}",
            event.kind, event.summary
        );
        step += 1;
    }

    println!(
        "  [{step}] RECEIPT   {} · outcome: {} (finished in {}ms)",
        receipt.id, receipt.outcome, receipt.duration_ms
    );

    Ok(())
}

fn print_help() {
    println!("Task Fabric SVN");
    println!("  task-fabric-svn-work status [--json] [--config PATH] [--socket PATH]");
    println!("  task-fabric-svn-work audio simulate [--app <name>] [--kind <kind>] [--caution]");
    println!("  task-fabric-svn-work select <project-id>");
    println!("  task-fabric-svn-work enter <world-id> [--dry-run] [--plan] [--json]");
    println!("  task-fabric-svn-work simulate <world-id> [--json] (flight simulator)");
    println!("  task-fabric-svn-work why [ref] [--json]    (causal replay & lineage)");
    println!("  task-fabric-svn-work resume plan <world-id> [--json]");
    println!("  task-fabric-svn-work resume <world-id> [--json]");
    println!("  task-fabric-svn-work worldstate|world-events|transition-receipt [ID]");
    println!("  task-fabric-svn-work study-context --json");
    println!("  task-fabric-svn-work semantic-context <world-id>");
    println!("  task-fabric-svn-work doctor [--audit] [--json]");
    println!("  task-fabric-svn-work agent-attach <world-id>");
    println!("  task-fabric-svn-work world list|scan|add (autonomous discovery & registration)");
    println!("  task-fabric-svn-work work <world-id>   (compatibility alias for enter)");
    println!("  task-fabric-svn-work action run <project-id> <action-id> [--confirm]");
    println!("  task-fabric-svn-work action dry-run <project-id> <action-id> [--json]");
    println!("  task-fabric-svn-work signal record <project> <kind> [--note TEXT]");
    println!("  task-fabric-svn-work signal list <project> [--json]");
    println!("  task-fabric-svn-work improve [propose|review <proposal-id>|queue-trial <proposal-id>] [world-id] [--diff] [--json]");
    println!("  task-fabric-svn-work experiment queue|baseline|create|evaluate|show|request-approval|recheck|approve|deny|stage|prove-loaded|canary-start|canary-observe|fork|promote|rollback|inspect-router|inspect-opencode|attach-router-cost|collect-result|attach-artifact ...");
    println!("  task-fabric-svn-work experiment-status <experiment-id>    (daemon projection)");
    println!("  task-fabric-svn-work learn drill <world-id> [--json]");
    println!("  task-fabric-svn-work learn list");
    println!("  task-fabric-svn-work audio status|mode|preview|test");
    println!("  task-fabric-svn-work memory ingest|search|query|recall|stats");
    println!("  task-fabric-svn-work attention spend|report [--hours <N>] [--json]");
    println!("  task-fabric-svn-work attention status [--json]");
    println!("  task-fabric-svn-work station sync");
    println!("  task-fabric-svn-work config example");
    println!("\nStatus is read-only. Start task-fabric-svn-workd for a local socket-backed status source.");
}

#[cfg(test)]
mod tests {
    use super::*;
    use task_fabric_svn_work_core::{ActionConfig, Project};
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    struct TestDir(PathBuf);

    impl TestDir {
        fn new(label: &str) -> Self {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("system clock")
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "task-fabric-svn-work-action-{label}-{}-{nonce}",
                std::process::id()
            ));
            fs::create_dir_all(&path).expect("create test dir");
            Self(path)
        }

        fn config(&self, action: ActionConfig) -> PathBuf {
            let config_path = self.0.join("projects.toml");
            let mut project = Project::default();
            project.id = "test-project".into();
            project.repo = self.0.clone();
            project.actions = vec![action];
            let mut config = ProjectConfig::default();
            config.projects = vec![project];
            let content = toml::to_string(&config).expect("serialize config");
            fs::write(&config_path, content).expect("write test config");
            config_path
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn action(id: &str, command: &[String], risk: &str) -> ActionConfig {
        ActionConfig {
            id: id.into(),
            label: id.into(),
            command: command.to_vec(),
            risk: risk.into(),
            cwd: None,
        }
    }

    #[test]
    fn action_runs_fixed_argv_in_explicit_cwd() {
        let dir = TestDir::new("runs");
        let marker = dir.0.join("ran.txt");
        let command = vec![
            "/bin/sh".into(),
            "-c".into(),
            format!("touch {}", marker.display()),
        ];
        let config = dir.config(action("touch", &command, "low"));
        action_command_with_config(&["test-project".into(), "touch".into()], &config)
            .expect("action must succeed");
        assert!(marker.exists());
    }

    #[test]
    fn review_action_requires_confirmation_then_runs() {
        let dir = TestDir::new("review");
        let marker = dir.0.join("reviewed.txt");
        let command = vec![
            "/bin/sh".into(),
            "-c".into(),
            format!("touch {}", marker.display()),
        ];
        let config = dir.config(action("touch", &command, "review"));

        let unconfirmed =
            action_command_with_config(&["test-project".into(), "touch".into()], &config)
                .expect_err("unconfirmed review action must fail");
        assert!(unconfirmed.contains("explicit confirmation"));
        assert!(!marker.exists());

        action_command_with_config(
            &["test-project".into(), "touch".into(), "--confirm".into()],
            &config,
        )
        .expect("confirmed action must run");
        assert!(marker.exists());
    }

    #[test]
    fn unknown_command_is_error() {
        assert!(run(vec!["nope".into()]).is_err());
    }

    #[test]
    fn option_path_reads_adjacent_value() {
        let dir = TestDir::new("option");
        let config = dir.config(action("opt", &["/bin/true".into()], "low"));
        let error = action_command_with_config(
            &[
                "test-project".into(),
                "opt".into(),
                "--unexpected".into(),
            ],
            &config,
        )
        .expect_err("unknown action options must fail closed");
        assert!(error.contains("unknown action option"));
    }

    #[test]
    fn unknown_action_is_rejected() {
        let dir = TestDir::new("unknown");
        let config = dir.config(action("known", &["/bin/true".into()], "low"));
        let error = action_command_with_config(&["test-project".into(), "missing".into()], &config)
            .expect_err("unknown action must fail");
        assert!(error.contains("unknown action missing"));
    }

    #[test]
    fn action_rejects_nul_arguments() {
        let dir = TestDir::new("nul");
        let command = vec!["/bin/true".into(), "bad\0arg".into()];
        let config = dir.config(action("nul", &command, "low"));
        let error = action_command_with_config(&["test-project".into(), "nul".into()], &config)
            .expect_err("NUL argument must fail");
        assert!(error.contains("NUL"));
    }

    #[test]
    fn action_rejects_missing_cwd_and_unknown_risk() {
        let dir = TestDir::new("validation");
        let mut missing_cwd = action("cwd", &["/bin/true".into()], "low");
        missing_cwd.cwd = Some(dir.0.join("missing"));
        let config = dir.config(missing_cwd);
        let error = action_command_with_config(&["test-project".into(), "cwd".into()], &config)
            .expect_err("missing cwd must fail");
        assert!(error.contains("working directory does not exist"));

        let config = dir.config(action("risk", &["/bin/true".into()], "dangerous"));
        let error = action_command_with_config(&["test-project".into(), "risk".into()], &config)
            .expect_err("unknown risk must fail");
        assert!(error.contains("unknown risk dangerous"));
    }

    #[test]
    fn action_failure_is_reported() {
        let dir = TestDir::new("failure");
        let command = vec!["/bin/sh".into(), "-c".into(), "exit 7".into()];
        let config = dir.config(action("failure", &command, "low"));
        let error = action_command_with_config(&["test-project".into(), "failure".into()], &config)
            .expect_err("failed command must fail");
        assert!(error.contains("action failure exited"));
    }

    #[test]
    fn action_dry_run_command_verifies_without_executing() {
        let dir = TestDir::new("dryrun");
        let command = vec!["/bin/sh".into(), "-c".into(), "echo simulated".into()];
        let config_path = dir.config(action("sim-action", &command, "low"));
        let (config, _) = ProjectConfig::from_path_or_example(&config_path).unwrap();
        let project = config.first_project().unwrap();
        let report = worldstate::simulate_action_dry_run(project, "sim-action").unwrap();
        assert!(report.ready);
        assert!(report.binary_exists);
        assert_eq!(report.action_id, "sim-action");
    }

    #[test]
    fn attention_command_runs_status_and_spend() {
        assert!(run(vec!["attention".into(), "status".into(), "--json".into()]).is_ok());
        assert!(run(vec!["attention".into(), "spend".into(), "--json".into()]).is_ok());
    }

    #[test]
    fn improve_command_runs_friction_analysis() {
        assert!(run(vec!["improve".into(), "--json".into()]).is_ok());
        assert!(run(vec!["improve".into(), "--diff".into()]).is_ok());
    }

    #[test]
    fn learn_command_runs_drill_and_list() {
        assert!(run(vec!["learn".into(), "list".into()]).is_ok());
        assert!(run(vec!["learn".into(), "drill".into(), "example-project".into(), "--json".into()]).is_ok());
    }
}
