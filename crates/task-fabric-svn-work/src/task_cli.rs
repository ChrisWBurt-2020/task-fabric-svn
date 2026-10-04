//! `task-fabric-svn-work task ...` — CLI surface for the Task Fabric ledger.
//!
//! Every subcommand prints one JSON object to stdout on success and exits
//! non-zero with a message on stderr on failure, so script and node callers
//! (ExampleField's WorkstationTaskClient) can rely on the contract:
//!
//! ```text
//! task-fabric-svn-work task submit --conversation-id C --target-space-id S \
//!     --intent-summary "..." --risk-class read [--param k=v ...]
//! task-fabric-svn-work task inspect <task-id>
//! task-fabric-svn-work task list [--state working] [--conversation-id C] [--limit N]
//! task-fabric-svn-work task history <task-id> [--limit N]
//! task-fabric-svn-work task start <task-id> --harness-session-id S
//! task-fabric-svn-work task begin-work <task-id>
//! task-fabric-svn-work task note-progress <task-id> --note "..."
//! task-fabric-svn-work task request-input <task-id> --question "..." --resume-token T
//! task-fabric-svn-work task respond <task-id> --resume-token T --answer "..."
//! task-fabric-svn-work task request-approval <task-id> --action "..."
//! task-fabric-svn-work task approve <task-id> --approved true|false [--reason R]
//! task-fabric-svn-work task complete <task-id> --result-summary "..." \
//!     [--artifact-ref A ...] [--receipt-ref R ...]
//! task-fabric-svn-work task fail <task-id> --reason R
//! task-fabric-svn-work task cancel <task-id> --reason R
//! ```
//!
//! This is step 3 of the Task Fabric build sequence: the durable ledger from
//! step 2, now reachable from outside the Rust process. No model or harness
//! runs here — workers attach in step 4.

use task_fabric_svn_work_core::task_ledger::{
    RiskClass, TaskError, TaskLedger, TaskListFilter, TaskState, TaskSubmitRequest,
};
use std::collections::HashMap;

pub fn task_command(args: &[String]) -> Result<(), String> {
    let ledger = TaskLedger::new();
    let sub = args.first().map(String::as_str).unwrap_or("help");
    let value = dispatch(&ledger, sub, &args[1..])?;
    println!(
        "{}",
        serde_json::to_string(&value).map_err(|e| e.to_string())?
    );
    Ok(())
}

fn dispatch(
    ledger: &TaskLedger,
    sub: &str,
    args: &[String],
) -> Result<serde_json::Value, String> {
    match sub {
        "submit" => submit_cmd(ledger, args),
        "inspect" => inspect_cmd(ledger, args),
        "list" => list_cmd(ledger, args),
        "history" => history_cmd(ledger, args),
        "start" => start_cmd(ledger, args),
        "begin-work" => begin_work_cmd(ledger, args),
        "note-progress" => note_progress_cmd(ledger, args),
        "request-input" => request_input_cmd(ledger, args),
        "respond" => respond_cmd(ledger, args),
        "request-approval" => request_approval_cmd(ledger, args),
        "approve" => approve_cmd(ledger, args),
        "complete" => complete_cmd(ledger, args),
        "fail" => fail_cmd(ledger, args),
        "cancel" => cancel_cmd(ledger, args),
        "help" | "--help" | "-h" => {
            print_task_help();
            Ok(serde_json::json!({ "ok": true }))
        }
        other => Err(format!(
            "unknown task subcommand '{other}' (try 'task-fabric-svn-work task help')"
        )),
    }
}

fn print_task_help() {
    println!(
        "Usage: task-fabric-svn-work task <subcommand> [flags]\n\
         \n\
         Subcommands: submit inspect list history start begin-work note-progress\n\
         request-input respond request-approval approve complete fail cancel\n\
         \n\
         Every subcommand prints one JSON object to stdout. See task_cli.rs for flags."
    );
}

fn task_result<T>(value: Result<T, TaskError>) -> Result<T, String> {
    value.map_err(|e| format!("task: {e}"))
}

fn ok_task(task: &task_fabric_svn_work_core::task_ledger::TaskRecord) -> serde_json::Value {
    serde_json::json!({ "ok": true, "task": task })
}

// ---- flag parsing ----

/// Parsed flags: single values plus repeated values (for --artifact-ref etc.).
struct Flags {
    single: HashMap<String, String>,
    repeated: HashMap<String, Vec<String>>,
}

fn parse_flags(args: &[String]) -> Result<Flags, String> {
    let mut single = HashMap::new();
    let mut repeated: HashMap<String, Vec<String>> = HashMap::new();
    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        let Some(name) = arg.strip_prefix("--") else {
            return Err(format!("expected --flag, got '{arg}'"));
        };
        let (name, inline) = match name.split_once('=') {
            Some((name, value)) => (name.to_string(), Some(value.to_string())),
            None => (name.to_string(), None),
        };
        let value = match inline {
            Some(value) => value,
            None => {
                i += 1;
                args.get(i)
                    .cloned()
                    .ok_or_else(|| format!("--{name} needs a value"))?
            }
        };
        if arg.starts_with("--no-") {
            single.insert(name, "false".to_string());
        } else {
            single.insert(name.clone(), value.clone());
            repeated.entry(name).or_default().push(value);
        }
        i += 1;
    }
    Ok(Flags { single, repeated })
}

fn required<'a>(flags: &'a Flags, key: &str) -> Result<&'a str, String> {
    flags
        .single
        .get(key)
        .map(String::as_str)
        .filter(|v| !v.is_empty())
        .ok_or_else(|| format!("--{key} is required"))
}

fn optional<'a>(flags: &'a Flags, key: &str) -> Option<&'a str> {
    flags
        .single
        .get(key)
        .map(String::as_str)
        .filter(|v| !v.is_empty())
}

fn positional<'a>(args: &'a [String], index: usize, what: &str) -> Result<&'a str, String> {
    args.get(index)
        .map(String::as_str)
        .ok_or_else(|| format!("{what} is required"))
}

fn parse_risk_class(raw: &str) -> Result<RiskClass, String> {
    match raw {
        "read" => Ok(RiskClass::Read),
        "write-sandbox" => Ok(RiskClass::WriteSandbox),
        "write-repo" => Ok(RiskClass::WriteRepo),
        "exec" => Ok(RiskClass::Exec),
        "external" => Ok(RiskClass::External),
        other => Err(format!(
            "unknown --risk-class '{other}' (expected read | write-sandbox | write-repo | exec | external)"
        )),
    }
}

fn parse_task_state(raw: &str) -> Result<TaskState, String> {
    match raw {
        "queued" => Ok(TaskState::Queued),
        "starting" => Ok(TaskState::Starting),
        "working" => Ok(TaskState::Working),
        "input_required" => Ok(TaskState::InputRequired),
        "approval_required" => Ok(TaskState::ApprovalRequired),
        "completed" => Ok(TaskState::Completed),
        "failed" => Ok(TaskState::Failed),
        "cancelled" => Ok(TaskState::Cancelled),
        other => Err(format!("unknown task state '{other}'")),
    }
}

// ---- subcommands ----

fn submit_cmd(ledger: &TaskLedger, args: &[String]) -> Result<serde_json::Value, String> {
    let flags = parse_flags(args)?;
    let mut parameters = serde_json::Map::new();
    for pair in flags.repeated.get("param").cloned().unwrap_or_default() {
        let (key, value) = pair
            .split_once('=')
            .ok_or_else(|| format!("--param must be key=value, got '{pair}'"))?;
        parameters.insert(key.to_string(), serde_json::Value::String(value.to_string()));
    }
    let record = task_result(ledger.submit(TaskSubmitRequest {
        conversation_id: required(&flags, "conversation-id")?.to_string(),
        parent_task_id: optional(&flags, "parent-task-id").map(str::to_string),
        target_space_id: required(&flags, "target-space-id")?.to_string(),
        target_repo_identity: optional(&flags, "target-repo-identity").map(str::to_string),
        target_revision: optional(&flags, "target-revision").map(str::to_string),
        harness: optional(&flags, "harness").map(str::to_string),
        intent_summary: required(&flags, "intent-summary")?.to_string(),
        risk_class: parse_risk_class(required(&flags, "risk-class")?)?,
        trace_id: optional(&flags, "trace-id").map(str::to_string),
        correlation_id: optional(&flags, "correlation-id").map(str::to_string),
        parameters: serde_json::Value::Object(parameters),
    }))?;
    Ok(serde_json::json!({ "ok": true, "task_id": record.task_id, "task": record }))
}

fn inspect_cmd(ledger: &TaskLedger, args: &[String]) -> Result<serde_json::Value, String> {
    let task_id = positional(args, 0, "task id")?;
    match task_result(ledger.inspect(task_id))? {
        Some(record) => Ok(ok_task(&record)),
        None => Err(format!("task not found: {task_id}")),
    }
}

fn list_cmd(ledger: &TaskLedger, args: &[String]) -> Result<serde_json::Value, String> {
    let flags = parse_flags(args)?;
    let state = optional(&flags, "state").map(parse_task_state).transpose()?;
    let limit = optional(&flags, "limit")
        .map(str::parse::<usize>)
        .transpose()
        .map_err(|_| "--limit must be a number".to_string())?;
    let tasks = task_result(ledger.list(TaskListFilter {
        state,
        conversation_id: optional(&flags, "conversation-id").map(str::to_string),
        limit,
    }))?;
    Ok(serde_json::json!({ "ok": true, "tasks": tasks }))
}

fn history_cmd(ledger: &TaskLedger, args: &[String]) -> Result<serde_json::Value, String> {
    let task_id = positional(args, 0, "task id")?;
    let flags = parse_flags(&args[1..])?;
    let limit = optional(&flags, "limit")
        .map(str::parse::<usize>)
        .transpose()
        .map_err(|_| "--limit must be a number".to_string())?
        .unwrap_or(100);
    let events = task_result(ledger.history(task_id, limit))?;
    Ok(serde_json::json!({ "ok": true, "task_id": task_id, "events": events }))
}

fn start_cmd(ledger: &TaskLedger, args: &[String]) -> Result<serde_json::Value, String> {
    let task_id = positional(args, 0, "task id")?;
    let flags = parse_flags(&args[1..])?;
    let record = task_result(ledger.start(task_id, required(&flags, "harness-session-id")?))?;
    Ok(ok_task(&record))
}

fn begin_work_cmd(ledger: &TaskLedger, args: &[String]) -> Result<serde_json::Value, String> {
    let task_id = positional(args, 0, "task id")?;
    let record = task_result(ledger.begin_work(task_id))?;
    Ok(ok_task(&record))
}

fn note_progress_cmd(ledger: &TaskLedger, args: &[String]) -> Result<serde_json::Value, String> {
    let task_id = positional(args, 0, "task id")?;
    let flags = parse_flags(&args[1..])?;
    let record = task_result(ledger.note_progress(task_id, required(&flags, "note")?))?;
    Ok(ok_task(&record))
}

fn request_input_cmd(ledger: &TaskLedger, args: &[String]) -> Result<serde_json::Value, String> {
    let task_id = positional(args, 0, "task id")?;
    let flags = parse_flags(&args[1..])?;
    let record = task_result(ledger.request_input(
        task_id,
        required(&flags, "question")?,
        required(&flags, "resume-token")?,
    ))?;
    Ok(ok_task(&record))
}

fn respond_cmd(ledger: &TaskLedger, args: &[String]) -> Result<serde_json::Value, String> {
    let task_id = positional(args, 0, "task id")?;
    let flags = parse_flags(&args[1..])?;
    let answer = task_answer_from_flags(&flags, &mut std::io::stdin().lock())?;
    let result = match optional(&flags, "generation") {
        Some(generation) => ledger.respond_generation(task_id, generation, &answer),
        None => ledger.respond(task_id, required(&flags, "resume-token")?, &answer),
    };
    let record = task_result(result)?;
    Ok(ok_task(&record))
}

fn task_answer_from_flags(flags: &Flags, input: &mut impl std::io::Read) -> Result<String, String> {
    if optional(flags, "answer-stdin") == Some("true") {
        use std::io::Read;
        if optional(flags, "answer").is_some() {
            return Err("--answer and --answer-stdin cannot be used together".into());
        }
        let mut answer = String::new();
        input
            .take((task_fabric_svn_work_core::task_ledger::MAX_TEXT + 1) as u64)
            .read_to_string(&mut answer)
            .map_err(|_| "could not read task answer from stdin".to_string())?;
        if answer.len() > task_fabric_svn_work_core::task_ledger::MAX_TEXT {
            return Err("task answer exceeds the maximum size".to_string());
        }
        Ok(answer)
    } else {
        Ok(required(flags, "answer")?.to_string())
    }
}

fn request_approval_cmd(
    ledger: &TaskLedger,
    args: &[String],
) -> Result<serde_json::Value, String> {
    let task_id = positional(args, 0, "task id")?;
    let flags = parse_flags(&args[1..])?;
    let record = task_result(ledger.request_approval(task_id, required(&flags, "action")?))?;
    Ok(ok_task(&record))
}

fn approve_cmd(ledger: &TaskLedger, args: &[String]) -> Result<serde_json::Value, String> {
    let task_id = positional(args, 0, "task id")?;
    let flags = parse_flags(&args[1..])?;
    let approved = match required(&flags, "approved")? {
        "true" => true,
        "false" => false,
        other => return Err(format!("--approved must be true|false, got '{other}'")),
    };
    let reason = optional(&flags, "reason");
    let result = match optional(&flags, "generation") {
        Some(generation) => ledger.approve_generation(task_id, generation, approved, reason),
        None => ledger.approve(task_id, approved, reason),
    };
    let record = task_result(result)?;
    Ok(ok_task(&record))
}

fn complete_cmd(ledger: &TaskLedger, args: &[String]) -> Result<serde_json::Value, String> {
    let task_id = positional(args, 0, "task id")?;
    let flags = parse_flags(&args[1..])?;
    let artifacts = flags.repeated.get("artifact-ref").cloned().unwrap_or_default();
    let receipts = flags.repeated.get("receipt-ref").cloned().unwrap_or_default();
    let record = task_result(ledger.complete(
        task_id,
        required(&flags, "result-summary")?,
        &artifacts,
        &receipts,
    ))?;
    Ok(ok_task(&record))
}

fn fail_cmd(ledger: &TaskLedger, args: &[String]) -> Result<serde_json::Value, String> {
    let task_id = positional(args, 0, "task id")?;
    let flags = parse_flags(&args[1..])?;
    let record = task_result(ledger.fail(task_id, required(&flags, "reason")?))?;
    Ok(ok_task(&record))
}

fn cancel_cmd(ledger: &TaskLedger, args: &[String]) -> Result<serde_json::Value, String> {
    let task_id = positional(args, 0, "task id")?;
    let flags = parse_flags(&args[1..])?;
    let record = match optional(&flags, "generation") {
        Some(generation) => task_result(ledger.cancel_generation(
            task_id,
            generation,
            required(&flags, "reason")?,
        ))?,
        None => task_result(ledger.cancel(task_id, required(&flags, "reason")?))?,
    };
    Ok(ok_task(&record))
}

#[cfg(test)]
mod tests {
    use super::*;
    use task_fabric_svn_work_core::task_ledger::TaskLedger;
    use std::sync::atomic::{AtomicU64, Ordering};

    static CLI_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn test_ledger() -> TaskLedger {
        let n = CLI_COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("task-fabric-svn-task-cli-test-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        TaskLedger::with_dir(dir)
    }

    fn args(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn submit_and_inspect_roundtrip() {
        let ledger = test_ledger();
        let value = dispatch(
            &ledger,
            "submit",
            &args(&[
                "--conversation-id", "conv-cli",
                "--target-space-id", "voice-default",
                "--intent-summary", "Say hello to the agent.",
                "--risk-class", "read",
                "--trace-id", "t1",
                "--param", "target_agent_identity=agent:opencode",
            ]),
        )
        .expect("submit");
        assert_eq!(value["ok"], true);
        let task_id = value["task_id"].as_str().expect("task_id");
        assert!(task_id.starts_with("task-"));
        assert_eq!(value["task"]["status"], "queued");
        assert_eq!(
            value["task"]["parameters"]["target_agent_identity"],
            "agent:opencode"
        );

        let inspected = dispatch(&ledger, "inspect", &args(&[task_id])).expect("inspect");
        assert_eq!(inspected["task"]["task_id"], task_id);
    }

    #[test]
    fn full_lifecycle_through_cli() {
        let ledger = test_ledger();
        let submitted = dispatch(
            &ledger,
            "submit",
            &args(&[
                "--conversation-id", "conv-cli",
                "--target-space-id", "voice-default",
                "--intent-summary", "Do the thing.",
                "--risk-class", "write-sandbox",
            ]),
        )
        .expect("submit");
        let task_id = submitted["task_id"].as_str().unwrap().to_string();

        let started = dispatch(
            &ledger,
            "start",
            &args(&[&task_id, "--harness-session-id", "sess-cli"]),
        )
        .expect("start");
        assert_eq!(started["task"]["status"], "starting");

        let working = dispatch(&ledger, "begin-work", &args(&[&task_id])).expect("begin-work");
        assert_eq!(working["task"]["status"], "working");

        let asked = dispatch(
            &ledger,
            "request-input",
            &args(&[&task_id, "--question", "Which?", "--resume-token", "tok-cli"]),
        )
        .expect("request-input");
        assert_eq!(asked["task"]["status"], "input_required");

        let resumed = dispatch(
            &ledger,
            "respond",
            &args(&[&task_id, "--resume-token", "tok-cli", "--answer", "This one"]),
        )
        .expect("respond");
        assert_eq!(resumed["task"]["status"], "working");

        let done = dispatch(
            &ledger,
            "complete",
            &args(&[
                &task_id,
                "--result-summary", "Did the thing.",
                "--artifact-ref", "a1",
                "--receipt-ref", "r1",
            ]),
        )
        .expect("complete");
        assert_eq!(done["task"]["status"], "completed");
        assert_eq!(done["task"]["receipt_refs"][0], "r1");
    }

    #[test]
    fn submit_rejects_unknown_risk_class_and_missing_flags() {
        let ledger = test_ledger();
        let err = dispatch(
            &ledger,
            "submit",
            &args(&[
                "--conversation-id", "c",
                "--target-space-id", "s",
                "--intent-summary", "x",
                "--risk-class", "nuke",
            ]),
        )
        .unwrap_err();
        assert!(err.contains("unknown --risk-class"), "{err}");

        let err = dispatch(
            &ledger,
            "submit",
            &args(&["--conversation-id", "c"]),
        )
        .unwrap_err();
        assert!(err.contains("--target-space-id"), "{err}");
    }

    #[test]
    fn unknown_subcommand_errors() {
        let ledger = test_ledger();
        let err = dispatch(&ledger, "launch", &[]).unwrap_err();
        assert!(err.contains("unknown task subcommand"), "{err}");
    }

    #[test]
    fn approve_false_cancels() {
        let ledger = test_ledger();
        let submitted = dispatch(
            &ledger,
            "submit",
            &args(&[
                "--conversation-id", "c",
                "--target-space-id", "s",
                "--intent-summary", "x",
                "--risk-class", "exec",
            ]),
        )
        .expect("submit");
        let task_id = submitted["task_id"].as_str().unwrap().to_string();
        dispatch(&ledger, "start", &args(&[&task_id, "--harness-session-id", "s1"])).expect("start");
        dispatch(&ledger, "begin-work", &args(&[&task_id])).expect("begin-work");
        dispatch(
            &ledger,
            "request-approval",
            &args(&[&task_id, "--action", "rm -rf /tmp/x"]),
        )
        .expect("request-approval");
        let resolved = dispatch(
            &ledger,
            "approve",
            &args(&[&task_id, "--approved", "false", "--reason", "too risky"]),
        )
        .expect("approve");
        assert_eq!(resolved["task"]["status"], "cancelled");
    }

    #[test]
    fn generation_arguments_resolve_input_and_approval() {
        let ledger = test_ledger();
        let submitted = dispatch(&ledger, "submit", &args(&[
            "--conversation-id", "generation-cli", "--target-space-id", "space",
            "--intent-summary", "Complete bounded work.", "--risk-class", "exec",
        ])).unwrap();
        let task_id = submitted["task_id"].as_str().unwrap().to_string();
        dispatch(&ledger, "start", &args(&[&task_id, "--harness-session-id", "s1"])).unwrap();
        dispatch(&ledger, "begin-work", &args(&[&task_id])).unwrap();

        let asked = dispatch(&ledger, "request-input", &args(&[
            &task_id, "--question", "Which?", "--resume-token", "token",
        ])).unwrap();
        let generation = asked["task"]["input_generation"].as_str().unwrap();
        let resumed = dispatch(&ledger, "respond", &args(&[
            &task_id, "--generation", generation, "--answer", "This one",
        ])).unwrap();
        assert_eq!(resumed["task"]["status"], "working");

        let asked = dispatch(&ledger, "request-approval", &args(&[
            &task_id, "--action", "run approved command",
        ])).unwrap();
        let generation = asked["task"]["approval_generation"].as_str().unwrap();
        let approved = dispatch(&ledger, "approve", &args(&[
            &task_id, "--generation", generation, "--approved", "true",
        ])).unwrap();
        assert_eq!(approved["task"]["status"], "working");
    }

    #[test]
    fn cancel_requires_current_interaction_generation_when_provided() {
        let ledger = test_ledger();
        let submitted = dispatch(&ledger, "submit", &args(&[
            "--conversation-id", "cancel-generation", "--target-space-id", "space",
            "--intent-summary", "Complete bounded work.", "--risk-class", "exec",
        ])).unwrap();
        let task_id = submitted["task_id"].as_str().unwrap().to_string();
        dispatch(&ledger, "start", &args(&[&task_id, "--harness-session-id", "s1"])).unwrap();
        dispatch(&ledger, "begin-work", &args(&[&task_id])).unwrap();
        let asked = dispatch(&ledger, "request-input", &args(&[
            &task_id, "--question", "Which?", "--resume-token", "token",
        ])).unwrap();
        let generation = asked["task"]["input_generation"].as_str().unwrap().to_string();
        let cancelled = dispatch(&ledger, "cancel", &args(&[
            &task_id, "--generation", &generation, "--reason", "user cancelled",
        ])).unwrap();
        assert_eq!(cancelled["task"]["status"], "cancelled");
    }

    #[test]
    fn task_answer_can_be_read_from_stdin_without_an_argv_value() {
        let flags = parse_flags(&args(&["--answer-stdin", "true"])).unwrap();
        let mut input = std::io::Cursor::new("  exact spoken answer  ");
        assert_eq!(task_answer_from_flags(&flags, &mut input).unwrap(), "  exact spoken answer  ");

        let both = parse_flags(&args(&["--answer-stdin", "true", "--answer", "argv-value"])).unwrap();
        let mut empty = std::io::Cursor::new("");
        assert!(task_answer_from_flags(&both, &mut empty).is_err());
    }
}
