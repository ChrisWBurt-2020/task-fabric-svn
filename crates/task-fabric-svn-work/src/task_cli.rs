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
//! task-fabric-svn-work task request-approval <task-id> --action "..." [--channel typed|voice|inbox|system]
//! task-fabric-svn-work task approve <task-id> --approved true|false [--reason R] [--channel C] [--diff-viewed true|false]
//! task-fabric-svn-work task complete <task-id> --result-summary "..." \
//!     [--artifact-ref A ...] [--receipt-ref R ...]
//! task-fabric-svn-work task fail <task-id> --reason R
//! task-fabric-svn-work task cancel <task-id> --reason R
//! task-fabric-svn-work task diff --task-id <id> --repo <path> --baseline <rev> [--json]
//! task-fabric-svn-work task record-diff-manifest --task-id <id> --manifest '<json>'
//! task-fabric-svn-work task create-pr --task-id <id> --title <t> --body <b> [--base B] [--head H]
//! task-fabric-svn-work task merge --task-id <id> --pr <number>
//! ```
//!
//! `diff` prints a human-readable summary by default; `--json` emits the
//! machine-readable manifest array. `create-pr`/`merge` mint scoped approval
//! envelopes (`pr-create` / `pr-merge`) and never invoke gh: execution happens
//! supervisor-side after approval, where envelope<->op matching is enforced.
//!
//! This is step 3 of the Task Fabric build sequence: the durable ledger from
//! step 2, now reachable from outside the Rust process. No model or harness
//! runs here — workers attach in step 4.

use task_fabric_svn_work_core::task_ledger::{
    ApprovalChannel, ArtifactRef, ReviewEvidence, RiskClass, TaskError, TaskLedger, TaskListFilter,
    TaskState, TaskSubmitRequest,
};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::Path;
use std::process::Command;

pub fn task_command(args: &[String]) -> Result<(), String> {
    let ledger = TaskLedger::new();
    let sub = args.first().map(String::as_str).unwrap_or("help");
    let rest = &args[1..];
    // `diff` without --json prints a human-readable summary to stdout
    // instead of the usual one-JSON-object contract.
    if sub == "diff" && !diff_wants_json(rest) {
        diff_text_cmd(&ledger, rest)?;
        return Ok(());
    }
    let value = dispatch(&ledger, sub, rest)?;
    println!(
        "{}",
        serde_json::to_string(&value).map_err(|e| e.to_string())?
    );
    Ok(())
}

/// True when the diff flags carry --json (bare or =true; --json=false opts out).
fn diff_wants_json(args: &[String]) -> bool {
    match parse_flags(args) {
        Ok(flags) => flags
            .single
            .get("json")
            .map(|v| v != "false")
            .unwrap_or(false),
        // Let dispatch surface the parse error under the JSON contract.
        Err(_) => false,
    }
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
        "diff" => diff_json_cmd(ledger, args),
        "record-diff-manifest" => record_diff_manifest_cmd(ledger, args),
        "create-pr" => create_pr_cmd(ledger, args),
        "merge" => merge_cmd(ledger, args),
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
         diff record-diff-manifest create-pr merge\n\
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
            None => match args.get(i + 1) {
                // A bare flag (end of args, or followed by another flag) is
                // "true", so `--json` works without a value.
                Some(next) if !next.starts_with("--") => {
                    i += 1;
                    next.clone()
                }
                _ => "true".to_string(),
            },
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
    let channel = parse_channel(optional(&flags, "channel"))?;
    let record = task_result(ledger.request_approval(
        task_id,
        required(&flags, "action")?,
        channel,
    ))?;
    Ok(ok_task(&record))
}

/// Approval channel for request/approve commands. Defaults to `typed`.
fn parse_channel(flag: Option<&str>) -> Result<ApprovalChannel, String> {
    match flag {
        None => Ok(ApprovalChannel::Typed),
        Some(s) => s.parse::<ApprovalChannel>(),
    }
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
    let channel = parse_channel(optional(&flags, "channel"))?;
    let review_evidence = match optional(&flags, "diff-viewed") {
        Some(value) => {
            let diff_viewed = match value {
                "true" => true,
                "false" => false,
                other => return Err(format!("--diff-viewed must be true|false, got [{other}]")),
            };
            Some(ReviewEvidence {
                diff_viewed,
                evidence_channel: channel,
            })
        }
        None => None,
    };
    let result = match optional(&flags, "generation") {
        Some(generation) => {
            ledger.approve_generation(task_id, generation, approved, reason, channel, review_evidence)
        }
        None => ledger.approve(task_id, approved, reason, channel, review_evidence),
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

// ---- diff ----

/// One changed file in a workspace-vs-baseline report.
#[derive(Debug, Clone, PartialEq, Eq)]
struct DiffFile {
    path: String,
    status: String,
    insertions: u64,
    deletions: u64,
    /// sha256 of the current workspace content; None when the file is
    /// absent from the workspace (deleted) or is a symlink (never followed).
    sha256: Option<String>,
}

struct DiffReport {
    task_id: String,
    baseline_rev: String,
    baseline_sha: String,
    files: Vec<DiffFile>,
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// Run git with `-C <repo>`; on failure the stderr text becomes the error.
fn git_stdout(repo: &Path, args: &[&str]) -> Result<String, String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .map_err(|e| format!("could not run git: {e}"))?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_string());
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn parse_count(raw: &str) -> u64 {
    // numstat prints "-" for binary files: unknown counts as zero.
    raw.parse::<u64>().unwrap_or(0)
}

fn count_lines(bytes: &[u8]) -> u64 {
    if bytes.is_empty() {
        return 0;
    }
    let mut n = bytes.iter().filter(|b| **b == b'\n').count() as u64;
    if !bytes.ends_with(b"\n") {
        n += 1;
    }
    n
}

/// Workspace content hash for a diff entry: None for deleted files and for
/// symlinks, which are never followed.
fn workspace_sha256(repo: &Path, path: &str, deleted: bool) -> Option<String> {
    if deleted {
        return None;
    }
    let full = repo.join(path);
    let meta = std::fs::symlink_metadata(&full).ok()?;
    if !meta.is_file() {
        return None;
    }
    std::fs::read(&full).ok().map(|bytes| sha256_hex(&bytes))
}

fn status_word(code: &str) -> &str {
    match code {
        "A" => "added",
        "M" => "modified",
        "D" => "deleted",
        "T" => "typechange",
        "U" => "unmerged",
        _ => code,
    }
}

/// Render workspace-vs-baseline for a repo. Read-only: no ledger mutation,
/// no working-tree changes.
fn render_diff(
    ledger: &TaskLedger,
    task_id: &str,
    repo: &str,
    baseline: &str,
) -> Result<DiffReport, String> {
    if task_result(ledger.inspect(task_id))?.is_none() {
        return Err(format!("task not found: {task_id}"));
    }
    let repo_path = Path::new(repo);
    if !repo_path.exists() {
        return Err(format!("repo path does not exist: {repo}"));
    }
    if git_stdout(repo_path, &["rev-parse", "--git-dir"]).is_err() {
        return Err(format!("not a git repository: {repo}"));
    }
    let baseline_sha = match git_stdout(repo_path, &["rev-parse", "--verify", baseline]) {
        Ok(out) => out.trim().to_string(),
        Err(stderr) => return Err(format!("baseline does not resolve: {baseline} ({stderr})")),
    };

    // numstat: "added\tdeleted\tpath\0" records; renames disabled so every
    // record carries exactly one path.
    let numstat = git_stdout(
        repo_path,
        &["diff", "--numstat", "-z", "--no-renames", &baseline_sha],
    )
    .map_err(|e| format!("git diff --numstat failed: {e}"))?;
    let mut counts: HashMap<String, (u64, u64)> = HashMap::new();
    for record in numstat.split('\0') {
        if record.is_empty() {
            continue;
        }
        let mut parts = record.splitn(3, '\t');
        let insertions = parse_count(parts.next().unwrap_or("0"));
        let deletions = parse_count(parts.next().unwrap_or("0"));
        let path = parts
            .next()
            .ok_or_else(|| "could not parse git numstat output".to_string())?;
        counts.insert(path.to_string(), (insertions, deletions));
    }

    // name-status: "STATUS\0path\0" pairs.
    let name_status = git_stdout(
        repo_path,
        &["diff", "--name-status", "-z", "--no-renames", &baseline_sha],
    )
    .map_err(|e| format!("git diff --name-status failed: {e}"))?;
    let mut files: Vec<DiffFile> = Vec::new();
    let tokens: Vec<&str> = name_status.split('\0').collect();
    let mut i = 0;
    while i + 1 < tokens.len() {
        let code = tokens[i];
        let path = tokens[i + 1];
        i += 2;
        if code.is_empty() || path.is_empty() {
            continue;
        }
        let status = status_word(code).to_string();
        let (insertions, deletions) = counts.get(path).copied().unwrap_or((0, 0));
        let sha256 = workspace_sha256(repo_path, path, status == "deleted");
        files.push(DiffFile {
            path: path.to_string(),
            status,
            insertions,
            deletions,
            sha256,
        });
    }

    // Untracked files are workspace changes too; git diff omits them.
    let untracked = git_stdout(repo_path, &["ls-files", "--others", "--exclude-standard", "-z"])
        .map_err(|e| format!("git ls-files failed: {e}"))?;
    for path in untracked.split('\0') {
        if path.is_empty() {
            continue;
        }
        let bytes = std::fs::read(repo_path.join(path)).unwrap_or_default();
        files.push(DiffFile {
            path: path.to_string(),
            status: "added".to_string(),
            insertions: count_lines(&bytes),
            deletions: 0,
            sha256: Some(sha256_hex(&bytes)),
        });
    }

    files.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(DiffReport {
        task_id: task_id.to_string(),
        baseline_rev: baseline.to_string(),
        baseline_sha,
        files,
    })
}

fn diff_report_json(report: &DiffReport) -> serde_json::Value {
    serde_json::Value::Array(
        report
            .files
            .iter()
            .map(|f| {
                serde_json::json!({
                    "path": f.path,
                    "sha256": f.sha256,
                    "status": f.status,
                })
            })
            .collect(),
    )
}

fn format_diff_human(report: &DiffReport) -> String {
    let mut out = String::new();
    let short_sha: String = report.baseline_sha.chars().take(7).collect();
    out.push_str(&format!(
        "diff {} vs baseline {} ({})\n",
        report.task_id, report.baseline_rev, short_sha
    ));
    let insertions: u64 = report.files.iter().map(|f| f.insertions).sum();
    let deletions: u64 = report.files.iter().map(|f| f.deletions).sum();
    out.push_str(&format!(
        "{} files changed, {insertions} insertions(+), {deletions} deletions(-)\n",
        report.files.len()
    ));
    for f in &report.files {
        let letter = match f.status.as_str() {
            "added" => "A",
            "modified" => "M",
            "deleted" => "D",
            "typechange" => "T",
            "unmerged" => "U",
            _ => "?",
        };
        if f.status == "deleted" {
            out.push_str(&format!("{letter} {} (deleted from workspace)\n", f.path));
        } else {
            out.push_str(&format!(
                "{letter} {} (+{}, -{})\n",
                f.path, f.insertions, f.deletions
            ));
        }
    }
    out
}

fn diff_json_cmd(ledger: &TaskLedger, args: &[String]) -> Result<serde_json::Value, String> {
    let flags = parse_flags(args)?;
    let report = render_diff(
        ledger,
        required(&flags, "task-id")?,
        required(&flags, "repo")?,
        required(&flags, "baseline")?,
    )?;
    Ok(diff_report_json(&report))
}

fn diff_text_cmd(ledger: &TaskLedger, args: &[String]) -> Result<(), String> {
    let flags = parse_flags(args)?;
    let report = render_diff(
        ledger,
        required(&flags, "task-id")?,
        required(&flags, "repo")?,
        required(&flags, "baseline")?,
    )?;
    print!("{}", format_diff_human(&report));
    Ok(())
}

// ---- record-diff-manifest ----

/// Parse the supervisor-provided manifest JSON and pin it on the task
/// record. Contract: task_cli record-diff-manifest --task-id <id>
/// --manifest '<json>'; <json> is an array of {ref_id, path, sha256}.
fn record_diff_manifest_cmd(
    ledger: &TaskLedger,
    args: &[String],
) -> Result<serde_json::Value, String> {
    let flags = parse_flags(args)?;
    let task_id = required(&flags, "task-id")?;
    let manifest_raw = required(&flags, "manifest")?;
    let manifest: Vec<ArtifactRef> = serde_json::from_str(manifest_raw).map_err(|e| {
        format!("invalid --manifest JSON (expected array of ref_id/path/sha256 objects): {e}")
    })?;
    let record = task_result(ledger.record_diff_manifest(task_id, manifest))?;
    Ok(serde_json::json!({
        "ok": true,
        "task_id": record.task_id,
        "diff_manifest": record.diff_manifest,
    }))
}

// ---- create-pr / merge ----

/// Scoped approval actions for the PR ops. Each op gets its OWN envelope;
/// the supervisor matches envelope<->op after approval before running gh.
const PR_CREATE_ACTION: &str = "pr-create";
const PR_MERGE_ACTION: &str = "pr-merge";

/// Mint the pending envelope for a scoped PR op. Never invokes gh.
fn request_pr_envelope(
    ledger: &TaskLedger,
    task_id: &str,
    action: &str,
) -> Result<serde_json::Value, String> {
    if task_result(ledger.inspect(task_id))?.is_none() {
        return Err(format!("task not found: {task_id}"));
    }
    let record = task_result(ledger.request_approval(task_id, action, ApprovalChannel::Typed))?;
    let envelope = record
        .pending_envelope
        .as_ref()
        .ok_or_else(|| "request_approval minted no envelope".to_string())?;
    Ok(serde_json::json!({
        "ok": true,
        "task_id": record.task_id,
        "action": envelope.action,
        "envelope_id": envelope.envelope_id,
        "generation": envelope.generation,
        "task": record,
    }))
}

fn create_pr_cmd(ledger: &TaskLedger, args: &[String]) -> Result<serde_json::Value, String> {
    let flags = parse_flags(args)?;
    let task_id = required(&flags, "task-id")?;
    let title = required(&flags, "title")?;
    // base/head/body travel with the supervisor's op; the envelope binds the
    // op type. Lengths are capped so a pasted body cannot bloat the CLI call.
    if title.len() > 256 {
        return Err("--title exceeds 256 chars".to_string());
    }
    let max_text = task_fabric_svn_work_core::task_ledger::MAX_TEXT;
    if optional(&flags, "body").map(str::len).unwrap_or(0) > max_text {
        return Err(format!("--body exceeds {max_text} chars"));
    }
    request_pr_envelope(ledger, task_id, PR_CREATE_ACTION)
}

fn merge_cmd(ledger: &TaskLedger, args: &[String]) -> Result<serde_json::Value, String> {
    let flags = parse_flags(args)?;
    let task_id = required(&flags, "task-id")?;
    let pr: u64 = required(&flags, "pr")?
        .parse()
        .ok()
        .filter(|n| *n > 0)
        .ok_or_else(|| "--pr must be a positive PR number".to_string())?;
    let mut value = request_pr_envelope(ledger, task_id, PR_MERGE_ACTION)?;
    value["pr"] = serde_json::json!(pr);
    Ok(value)
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

    // ---- diff / record-diff-manifest / create-pr / merge (Phase 1, Track B) ----

    use task_fabric_svn_work_core::task_ledger::TaskEventKind;

    fn make_git_repo(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "task-fabric-svn-diff-test-{}-{tag}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let run = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(&dir)
                .output()
                .expect("spawn git");
            assert!(
                out.status.success(),
                "git {args:?} failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        };
        run(&["init", "-q"]);
        run(&["config", "user.email", "test@example.com"]);
        run(&["config", "user.name", "test"]);
        std::fs::write(dir.join("a.txt"), "one\ntwo\n").unwrap();
        std::fs::write(dir.join("gone.txt"), "bye\n").unwrap();
        run(&["add", "."]);
        run(&["commit", "-qm", "base"]);
        // Workspace changes vs the HEAD baseline: one modified, one
        // deleted, one new (untracked).
        std::fs::write(dir.join("a.txt"), "one\nTWO\nthree\n").unwrap();
        std::fs::remove_file(dir.join("gone.txt")).unwrap();
        std::fs::write(dir.join("new.txt"), "hello\n").unwrap();
        dir
    }

    fn submit_working_task(ledger: &TaskLedger, conv: &str) -> String {
        let submitted = dispatch(
            ledger,
            "submit",
            &args(&[
                "--conversation-id",
                conv,
                "--target-space-id",
                "space",
                "--intent-summary",
                "Phase 1 slice work.",
                "--risk-class",
                "write-sandbox",
            ]),
        )
        .expect("submit");
        let task_id = submitted["task_id"].as_str().unwrap().to_string();
        dispatch(
            ledger,
            "start",
            &args(&[&task_id, "--harness-session-id", "s1"]),
        )
        .expect("start");
        dispatch(&ledger, "begin-work", &args(&[task_id.as_str()])).expect("begin-work");
        task_id
    }

    fn diff_flag_args(task_id: &str, repo: &str, baseline: &str, json: bool) -> Vec<String> {
        let mut v = vec![
            "--task-id".to_string(),
            task_id.to_string(),
            "--repo".to_string(),
            repo.to_string(),
            "--baseline".to_string(),
            baseline.to_string(),
        ];
        if json {
            // Bare --json (no value): exercises the bare-flag parse path.
            v.push("--json".to_string());
        }
        v
    }

    #[test]
    fn diff_json_lists_changed_files_with_hashes() {
        let ledger = test_ledger();
        let task_id = submit_working_task(&ledger, "conv-diff");
        let repo = make_git_repo("json");
        let value = dispatch(
            &ledger,
            "diff",
            &diff_flag_args(&task_id, repo.to_str().unwrap(), "HEAD", true),
        )
        .expect("diff --json");
        assert!(value.is_array(), "expected bare array, got {value}");
        let files = value.as_array().unwrap();
        assert_eq!(files.len(), 3, "{files:?}");
        let by_path = |p: &str| {
            files
                .iter()
                .find(|f| f["path"] == p)
                .unwrap_or_else(|| panic!("missing {p}"))
        };
        let modified = by_path("a.txt");
        assert_eq!(modified["status"], "modified");
        assert_eq!(
            modified["sha256"],
            sha256_hex(b"one\nTWO\nthree\n"),
            "sha256 must cover current workspace content"
        );
        let deleted = by_path("gone.txt");
        assert_eq!(deleted["status"], "deleted");
        assert!(
            deleted["sha256"].is_null(),
            "deleted files have no workspace content"
        );
        let added = by_path("new.txt");
        assert_eq!(added["status"], "added");
        // sha256("hello\n") pinned: the digest must be the real file hash.
        assert_eq!(
            added["sha256"],
            "5891b5b522d5df086d0ff0b110fbd9d21bb4fc7163af34d08286a2e846f6be03"
        );
    }

    #[test]
    fn diff_human_summary_lists_files_and_counts() {
        let ledger = test_ledger();
        let task_id = submit_working_task(&ledger, "conv-diff-human");
        let repo = make_git_repo("human");
        let report = render_diff(&ledger, &task_id, repo.to_str().unwrap(), "HEAD")
            .expect("render_diff");
        let text = format_diff_human(&report);
        assert!(text.contains("3 files changed"), "{text}");
        assert!(text.contains("a.txt"), "{text}");
        assert!(text.contains("gone.txt"), "{text}");
        assert!(text.contains("new.txt"), "{text}");
        assert!(text.contains("baseline HEAD"), "{text}");
    }

    #[test]
    fn diff_rejects_unresolvable_baseline_with_clean_error() {
        let ledger = test_ledger();
        let task_id = submit_working_task(&ledger, "conv-diff-badrev");
        let repo = make_git_repo("badrev");
        let err = dispatch(
            &ledger,
            "diff",
            &diff_flag_args(&task_id, repo.to_str().unwrap(), "no-such-rev", true),
        )
        .unwrap_err();
        assert!(err.contains("baseline does not resolve"), "{err}");
    }

    #[test]
    fn diff_rejects_non_git_repo_and_missing_path() {
        let ledger = test_ledger();
        let task_id = submit_working_task(&ledger, "conv-diff-norepo");
        let plain = std::env::temp_dir().join(format!(
            "task-fabric-svn-diff-plain-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&plain);
        std::fs::create_dir_all(&plain).unwrap();
        let err = dispatch(
            &ledger,
            "diff",
            &diff_flag_args(&task_id, plain.to_str().unwrap(), "HEAD", true),
        )
        .unwrap_err();
        assert!(err.contains("not a git repository"), "{err}");

        let err = dispatch(
            &ledger,
            "diff",
            &diff_flag_args(&task_id, "/tmp/task-fabric-svn-no-such-dir-xyz", "HEAD", true),
        )
        .unwrap_err();
        assert!(err.contains("repo path does not exist"), "{err}");
    }

    #[test]
    fn diff_requires_an_existing_task_and_is_read_only() {
        let ledger = test_ledger();
        let repo = make_git_repo("notask");
        let err = dispatch(
            &ledger,
            "diff",
            &diff_flag_args("task-nope", repo.to_str().unwrap(), "HEAD", true),
        )
        .unwrap_err();
        assert!(err.contains("task not found"), "{err}");

        let task_id = submit_working_task(&ledger, "conv-diff-ro");
        let before = ledger.inspect(&task_id).expect("inspect").expect("exists");
        dispatch(
            &ledger,
            "diff",
            &diff_flag_args(&task_id, repo.to_str().unwrap(), "HEAD", true),
        )
        .expect("diff");
        let after = ledger.inspect(&task_id).expect("inspect").expect("exists");
        assert_eq!(before, after, "diff must not mutate the ledger");
    }

    #[test]
    fn record_diff_manifest_cli_roundtrip_and_note_journal() {
        let ledger = test_ledger();
        let task_id = submit_working_task(&ledger, "conv-manifest-cli");
        let manifest = serde_json::json!([
            {"ref_id": "art-1", "path": "src/main.rs", "sha256": "ab".repeat(32)},
            {"ref_id": "art-2", "path": "gone.txt", "sha256": null},
        ]);
        let value = dispatch(
            &ledger,
            "record-diff-manifest",
            &args(&[
                "--task-id",
                &task_id,
                "--manifest",
                &serde_json::to_string(&manifest).unwrap(),
            ]),
        )
        .expect("record-diff-manifest");
        assert_eq!(value["ok"], true);
        assert_eq!(value["diff_manifest"].as_array().unwrap().len(), 2);

        // Snapshot round-trip preserves the manifest.
        let inspected = dispatch(&ledger, "inspect", &args(&[&task_id])).expect("inspect");
        assert_eq!(inspected["task"]["diff_manifest"][0]["ref_id"], "art-1");
        assert_eq!(inspected["task"]["diff_manifest"][1]["path"], "gone.txt");

        // The journal carries the summarizing Note event.
        let events = ledger.history(&task_id, 100).expect("history");
        let note = events
            .iter()
            .rev()
            .find(|e| e.kind == TaskEventKind::Note)
            .expect("Note event");
        let text = note.payload["note"].as_str().expect("note text");
        assert!(text.contains("2 artifact"), "unexpected summary: {text}");
    }

    #[test]
    fn record_diff_manifest_cli_rejects_unsafe_path_and_bad_json() {
        let ledger = test_ledger();
        let task_id = submit_working_task(&ledger, "conv-manifest-bad");
        let err = dispatch(
            &ledger,
            "record-diff-manifest",
            &args(&[
                "--task-id",
                &task_id,
                "--manifest",
                r#"[{"ref_id":"a","path":"/abs/x","sha256":null}]"#,
            ]),
        )
        .unwrap_err();
        assert!(err.contains("relative"), "{err}");

        let err = dispatch(
            &ledger,
            "record-diff-manifest",
            &args(&["--task-id", &task_id, "--manifest", r#"[{"ref_id":"a","path":"../x"}]"#]),
        )
        .unwrap_err();
        assert!(err.contains("unsafe"), "{err}");

        let err = dispatch(
            &ledger,
            "record-diff-manifest",
            &args(&["--task-id", &task_id, "--manifest", "not json"]),
        )
        .unwrap_err();
        assert!(err.contains("invalid --manifest JSON"), "{err}");
    }

    #[test]
    fn create_pr_mints_pr_create_envelope() {
        let ledger = test_ledger();
        let task_id = submit_working_task(&ledger, "conv-create-pr");
        let value = dispatch(
            &ledger,
            "create-pr",
            &args(&[
                "--task-id",
                &task_id,
                "--title",
                "Add the thing",
                "--body",
                "Details here.",
                "--base",
                "main",
                "--head",
                "feat-thing",
            ]),
        )
        .expect("create-pr");
        assert_eq!(value["ok"], true);
        assert_eq!(value["action"], "pr-create");
        let envelope_id = value["envelope_id"].as_str().expect("envelope_id");
        assert!(envelope_id.starts_with("env-"), "{envelope_id}");
        let generation = value["generation"].as_str().expect("generation");
        assert!(!generation.is_empty());

        let record = ledger.inspect(&task_id).expect("inspect").expect("exists");
        let pending = record.pending_envelope.as_ref().expect("pending envelope");
        assert_eq!(pending.action, "pr-create");
        assert_eq!(pending.envelope_id, envelope_id);
        assert_eq!(pending.generation, generation);
        assert_eq!(
            pending.channel,
            task_fabric_svn_work_core::task_ledger::ApprovalChannel::Typed
        );
    }

    #[test]
    fn merge_mints_pr_merge_envelope() {
        let ledger = test_ledger();
        let task_id = submit_working_task(&ledger, "conv-merge");
        let value = dispatch(
            &ledger,
            "merge",
            &args(&["--task-id", &task_id, "--pr", "42"]),
        )
        .expect("merge");
        assert_eq!(value["ok"], true);
        assert_eq!(value["action"], "pr-merge");
        assert_eq!(value["pr"], 42);

        let record = ledger.inspect(&task_id).expect("inspect").expect("exists");
        let pending = record.pending_envelope.as_ref().expect("pending envelope");
        assert_eq!(pending.action, "pr-merge");
    }

    #[test]
    fn create_pr_and_merge_require_existing_task_and_valid_pr() {
        let ledger = test_ledger();
        let err = dispatch(
            &ledger,
            "create-pr",
            &args(&["--task-id", "task-nope", "--title", "t", "--body", "b"]),
        )
        .unwrap_err();
        assert!(err.contains("task not found"), "{err}");

        let task_id = submit_working_task(&ledger, "conv-merge-bad");
        for bad in ["0", "abc"] {
            let err = dispatch(&ledger, "merge", &args(&["--task-id", &task_id, "--pr", bad]))
                .unwrap_err();
            assert!(err.contains("--pr must be a positive PR number"), "{err}");
        }
    }

    #[test]
    fn pr_envelopes_stay_generation_bound() {
        use task_fabric_svn_work_core::task_ledger::ApprovalChannel;
        let ledger = test_ledger();
        let task_id = submit_working_task(&ledger, "conv-pr-gen");

        // pr-create mints its own envelope; resolving it with the printed
        // generation returns the task to working.
        let created = dispatch(
            &ledger,
            "create-pr",
            &args(&["--task-id", &task_id, "--title", "t", "--body", "b"]),
        )
        .expect("create-pr");
        let create_gen = created["generation"].as_str().unwrap().to_string();
        let resolved = ledger
            .approve_generation(&task_id, &create_gen, true, None, ApprovalChannel::Typed, None)
            .expect("approve pr-create");
        assert_eq!(resolved.status, task_fabric_svn_work_core::task_ledger::TaskState::Working);

        // pr-merge mints a NEW envelope: the two ops never share a gate.
        let merged = dispatch(
            &ledger,
            "merge",
            &args(&["--task-id", &task_id, "--pr", "7"]),
        )
        .expect("merge");
        let merge_gen = merged["generation"].as_str().unwrap().to_string();
        assert_ne!(create_gen, merge_gen, "each op gets its own envelope");
        assert_eq!(merged["action"], "pr-merge");

        // The stale pr-create generation no longer resolves.
        let stale = ledger.approve_generation(
            &task_id,
            &create_gen,
            true,
            None,
            ApprovalChannel::Typed,
            None,
        );
        assert!(stale.is_err(), "stale generation must be rejected");
    }
}
