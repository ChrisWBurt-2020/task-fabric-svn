//! Separately supervised, bounded executor for experiment trial jobs.
//!
//! Jobs are owner-local JSON files. A job is moved to `running/` before
//! execution and its result is written to `results/`; a process restart moves
//! unfinished jobs back to `inbox/`. The worker never reads or writes the
//! live Governor directory unless a job explicitly supplies an isolated
//! environment and checkout.

use task_fabric_svn_work_core::{experiment, now_epoch_seconds, worldstate};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

const SCHEMA_VERSION: u32 = 1;
const MAX_OUTPUT_BYTES: usize = 64 * 1024;
const MAX_TIMEOUT_SECONDS: u64 = 24 * 60 * 60;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct TrialJob {
    schema_version: u32,
    job_id: String,
    experiment_id: String,
    trial_id: String,
    run_id: String,
    session_id: String,
    task_revision: String,
    harness_manifest_digest: String,
    cwd: String,
    command: Vec<String>,
    #[serde(default)]
    env: BTreeMap<String, String>,
    timeout_seconds: u64,
    #[serde(default)]
    release_target: Option<String>,
    #[serde(default)]
    expected_manifest_digest: Option<String>,
    #[serde(default)]
    router_receipts_path: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct TrialResult {
    schema_version: u32,
    job_id: String,
    experiment_id: Option<String>,
    trial_id: Option<String>,
    run_id: Option<String>,
    session_id: Option<String>,
    task_revision: Option<String>,
    harness_manifest_digest: Option<String>,
    status: String,
    exit_code: Option<i32>,
    started_at_epoch_seconds: u64,
    finished_at_epoch_seconds: u64,
    stdout_excerpt: String,
    stderr_excerpt: String,
    stdout_truncated: bool,
    stderr_truncated: bool,
    stdout_sha256: String,
    stderr_sha256: String,
    release_manifest_digest: Option<String>,
    router_receipt_source_digest: Option<String>,
    router_prompt_tokens: Option<u64>,
    router_completion_tokens: Option<u64>,
    router_cost_micros: Option<u64>,
    router_unknown_cost_rows: Option<u64>,
    router_receipts_error: Option<String>,
    error: Option<String>,
}

fn state_dir() -> PathBuf {
    std::env::var_os("TASK_FABRIC_SVN_WORK_STATE")
        .map(PathBuf::from)
        .unwrap_or_else(|| worldstate::state_dir())
}

fn dirs(state: &Path) -> (PathBuf, PathBuf, PathBuf) {
    let root = state.join("experiments").join("worker");
    (root.join("inbox"), root.join("running"), root.join("results"))
}

fn safe_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.'))
}

fn validate_job(job: &TrialJob) -> io::Result<()> {
    if job.schema_version != SCHEMA_VERSION
        || !safe_id(&job.job_id)
        || !safe_id(&job.experiment_id)
        || !safe_id(&job.trial_id)
        || !safe_id(&job.run_id)
        || !safe_id(&job.session_id)
        || !safe_id(&job.task_revision)
        || job.harness_manifest_digest.len() != 64
        || !job.harness_manifest_digest.chars().all(|character| character.is_ascii_hexdigit())
    {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "invalid trial job identity"));
    }
    if job.command.is_empty() || job.command.len() > 256 || job.timeout_seconds == 0 || job.timeout_seconds > MAX_TIMEOUT_SECONDS {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "invalid trial command or timeout"));
    }
    if !Path::new(&job.cwd).is_absolute() || !Path::new(&job.cwd).is_dir() {
        return Err(io::Error::new(io::ErrorKind::NotFound, "trial cwd is not an existing absolute directory"));
    }
    if job.release_target.is_some() != job.expected_manifest_digest.is_some() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "release target and expected manifest must be supplied together",
        ));
    }
    if let Some(target) = &job.release_target {
        if target.is_empty()
            || !target
                .chars()
                .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.'))
        {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "invalid release target"));
        }
    }
    if let Some(digest) = &job.expected_manifest_digest {
        if digest.len() != 64 || !digest.chars().all(|character| character.is_ascii_hexdigit()) {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "invalid release manifest digest"));
        }
    }
    if let Some(path) = &job.router_receipts_path {
        if !Path::new(path).is_absolute() || !Path::new(path).is_file() {
            return Err(io::Error::new(io::ErrorKind::NotFound, "router receipt export is not an existing absolute file"));
        }
    }
    for value in job.command.iter().chain(std::iter::once(&job.cwd)) {
        if value.contains('\0') || value.len() > 16 * 1024 {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "trial argument is invalid or too large"));
        }
    }
    for (key, value) in &job.env {
        if key.is_empty() || key.contains('\0') || key.contains('=') || value.contains('\0') {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "trial environment contains an invalid entry"));
        }
        if matches!(key.as_str(), "TASK_FABRIC_SVN_EXPERIMENT_ID" | "TASK_FABRIC_SVN_TRIAL_ID" | "TASK_FABRIC_SVN_WORK_ID" | "TASK_FABRIC_SVN_TRACE_ID") {
            return Err(io::Error::new(io::ErrorKind::PermissionDenied, "trial identity environment is worker-owned"));
        }
    }
    Ok(())
}

fn digest(bytes: &[u8]) -> String {
    Sha256::digest(bytes).iter().map(|byte| format!("{byte:02x}")).collect()
}

fn read_bounded(mut reader: impl Read) -> (Vec<u8>, bool) {
    let mut retained = Vec::new();
    let mut buffer = [0u8; 8192];
    let mut truncated = false;
    loop {
        match reader.read(&mut buffer) {
            Ok(0) => break,
            Ok(count) => {
                if retained.len() < MAX_OUTPUT_BYTES {
                    let keep = (MAX_OUTPUT_BYTES - retained.len()).min(count);
                    retained.extend_from_slice(&buffer[..keep]);
                    if keep < count {
                        truncated = true;
                    }
                } else {
                    truncated = true;
                }
            }
            Err(_) => break,
        }
    }
    (retained, truncated)
}

fn verify_release_pointer(job: &TrialJob, state: &Path) -> io::Result<Option<String>> {
    let (Some(target), Some(expected)) = (&job.release_target, &job.expected_manifest_digest) else {
        return Ok(None);
    };
    let pointer_path = experiment::release_pointer_path(state, target)?;
    let pointer: experiment::ActiveReleasePointer = serde_json::from_slice(&fs::read(pointer_path)?)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    if pointer.active_manifest_digest != *expected {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "active release pointer does not match expected manifest",
        ));
    }
    let marker = state
        .join("experiments")
        .join("releases")
        .join(target)
        .join(expected)
        .join("release.json");
    if !marker.is_file() {
        return Err(io::Error::new(io::ErrorKind::NotFound, "expected staged release marker is missing"));
    }
    Ok(Some(expected.clone()))
}

fn run_job(job: &TrialJob, state: &Path) -> TrialResult {
    let started = now_epoch_seconds();
    let mut base = TrialResult {
        schema_version: SCHEMA_VERSION,
        job_id: job.job_id.clone(),
        experiment_id: Some(job.experiment_id.clone()),
        trial_id: Some(job.trial_id.clone()),
        run_id: Some(job.run_id.clone()),
        session_id: Some(job.session_id.clone()),
        task_revision: Some(job.task_revision.clone()),
        harness_manifest_digest: Some(job.harness_manifest_digest.clone()),
        status: "failed".into(),
        exit_code: None,
        started_at_epoch_seconds: started,
        finished_at_epoch_seconds: started,
        stdout_excerpt: String::new(),
        stderr_excerpt: String::new(),
        stdout_truncated: false,
        stderr_truncated: false,
        stdout_sha256: digest(&[]),
        stderr_sha256: digest(&[]),
        release_manifest_digest: None,
        router_receipt_source_digest: None,
        router_prompt_tokens: None,
        router_completion_tokens: None,
        router_cost_micros: None,
        router_unknown_cost_rows: None,
        router_receipts_error: None,
        error: None,
    };
    if let Err(error) = validate_job(job) {
        base.error = Some(error.to_string());
        base.finished_at_epoch_seconds = now_epoch_seconds();
        return base;
    }
    base.release_manifest_digest = match verify_release_pointer(job, state) {
        Ok(digest) => digest,
        Err(error) => {
            base.error = Some(error.to_string());
            base.finished_at_epoch_seconds = now_epoch_seconds();
            return base;
        }
    };

    let mut command = Command::new(&job.command[0]);
    command
        .args(&job.command[1..])
        .current_dir(&job.cwd)
        .env_clear()
        .env("PATH", job.env.get("PATH").map(String::as_str).unwrap_or("/usr/bin:/bin"))
        .env("TASK_FABRIC_SVN_EXPERIMENT_ID", &job.experiment_id)
        .env("TASK_FABRIC_SVN_TRIAL_ID", &job.trial_id)
        // 9router accepts Task Fabric SVN correlation headers; isolated trial launchers
        // can map these worker-owned values into their request config.
        .env("TASK_FABRIC_SVN_WORK_ID", &job.experiment_id)
        .env("TASK_FABRIC_SVN_TRACE_ID", &job.run_id)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (key, value) in &job.env {
        command.env(key, value);
    }
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            base.error = Some(error.to_string());
            base.finished_at_epoch_seconds = now_epoch_seconds();
            return base;
        }
    };
    let stdout_reader = child.stdout.take().map(|reader| thread::spawn(move || read_bounded(reader)));
    let stderr_reader = child.stderr.take().map(|reader| thread::spawn(move || read_bounded(reader)));
    let deadline = Instant::now() + Duration::from_secs(job.timeout_seconds);
    let mut timed_out = false;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                base.exit_code = status.code();
                base.status = if status.success() { "succeeded" } else { "failed" }.into();
                break;
            }
            Ok(None) if Instant::now() >= deadline => {
                timed_out = true;
                let _ = child.kill();
                base.status = "timed_out".into();
                break;
            }
            Ok(None) => thread::sleep(Duration::from_millis(25)),
            Err(error) => {
                base.error = Some(error.to_string());
                let _ = child.kill();
                break;
            }
        }
    }
    if timed_out {
        let _ = child.wait();
    }
    let (stdout, stdout_truncated) = stdout_reader
        .and_then(|reader| reader.join().ok())
        .unwrap_or_default();
    let (stderr, stderr_truncated) = stderr_reader
        .and_then(|reader| reader.join().ok())
        .unwrap_or_default();
    base.stdout_sha256 = digest(&stdout);
    base.stderr_sha256 = digest(&stderr);
    base.stdout_excerpt = String::from_utf8_lossy(&stdout).into_owned();
    base.stderr_excerpt = String::from_utf8_lossy(&stderr).into_owned();
    base.stdout_truncated = stdout_truncated;
    base.stderr_truncated = stderr_truncated;
    if let Some(path) = &job.router_receipts_path {
        match experiment::summarize_router_receipts(Path::new(path), &job.experiment_id) {
            Ok(summary) => {
                base.router_receipt_source_digest = Some(summary.source_digest);
                base.router_prompt_tokens = Some(summary.prompt_tokens);
                base.router_completion_tokens = Some(summary.completion_tokens);
                base.router_cost_micros = summary.cost_micros;
                base.router_unknown_cost_rows = Some(summary.unknown_cost_rows);
                if summary.attributed_rows == 0 {
                    base.router_receipts_error = Some("no receipt carried the exact experiment identity".into());
                }
            }
            Err(error) => base.router_receipts_error = Some(error.to_string()),
        }
    }
    base.finished_at_epoch_seconds = now_epoch_seconds();
    base
}

fn recover_running(inbox: &Path, running: &Path) -> io::Result<()> {
    fs::create_dir_all(inbox)?;
    fs::create_dir_all(running)?;
    for entry in fs::read_dir(running)? {
        let entry = entry?;
        if entry.path().extension().and_then(|ext| ext.to_str()) == Some("json") {
            let destination = inbox.join(entry.file_name());
            if !destination.exists() {
                fs::rename(entry.path(), destination)?;
            }
        }
    }
    Ok(())
}

fn process_one(inbox: &Path, running: &Path, results: &Path, state: &Path) -> io::Result<bool> {
    let mut jobs = fs::read_dir(inbox)?
        .filter_map(Result::ok)
        .filter(|entry| entry.path().extension().and_then(|ext| ext.to_str()) == Some("json"))
        .collect::<Vec<_>>();
    jobs.sort_by_key(|entry| entry.file_name());
    let Some(entry) = jobs.into_iter().next() else {
        return Ok(false);
    };
    let job_path = entry.path();
    let running_path = running.join(entry.file_name());
    fs::rename(&job_path, &running_path)?;
    let job_bytes = fs::read(&running_path)?;
    let result = match serde_json::from_slice::<TrialJob>(&job_bytes) {
        Ok(job) => run_job(&job, state),
        Err(error) => TrialResult {
            schema_version: SCHEMA_VERSION,
            job_id: entry.path().file_stem().and_then(|stem| stem.to_str()).unwrap_or("invalid").into(),
            experiment_id: None,
            trial_id: None,
            run_id: None,
            session_id: None,
            task_revision: None,
            harness_manifest_digest: None,
            status: "failed".into(),
            exit_code: None,
            started_at_epoch_seconds: now_epoch_seconds(),
            finished_at_epoch_seconds: now_epoch_seconds(),
            stdout_excerpt: String::new(),
            stderr_excerpt: String::new(),
            stdout_truncated: false,
            stderr_truncated: false,
            stdout_sha256: digest(&[]),
            stderr_sha256: digest(&[]),
            release_manifest_digest: None,
            router_receipt_source_digest: None,
            router_prompt_tokens: None,
            router_completion_tokens: None,
            router_cost_micros: None,
            router_unknown_cost_rows: None,
            router_receipts_error: None,
            error: Some(error.to_string()),
        },
    };
    let result_path = results.join(format!("{}.result.json", result.job_id));
    worldstate::atomic_write(&result_path, &result)?;
    fs::remove_file(running_path)?;
    Ok(true)
}

fn main() -> io::Result<()> {
    let once = std::env::args().any(|arg| arg == "--once");
    let state = state_dir();
    let (inbox, running, results) = dirs(&state);
    fs::create_dir_all(&inbox)?;
    fs::create_dir_all(&running)?;
    fs::create_dir_all(&results)?;
    recover_running(&inbox, &running)?;
    loop {
        let processed = process_one(&inbox, &running, &results, &state)?;
        if once || !processed {
            if !once {
                thread::sleep(Duration::from_millis(250));
                continue;
            }
            break;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_root() -> PathBuf {
        std::env::temp_dir().join(format!(
            "task-fabric-svn-experiment-worker-{}",
            SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()
        ))
    }

    fn job(root: &Path, command: &[&str], timeout_seconds: u64) -> TrialJob {
        TrialJob {
            schema_version: SCHEMA_VERSION,
            job_id: "job-1".into(),
            experiment_id: "experiment-1".into(),
            trial_id: "trial-1".into(),
            run_id: "run-1".into(),
            session_id: "session-1".into(),
            task_revision: "task-1".into(),
            harness_manifest_digest: "a".repeat(64),
            cwd: root.display().to_string(),
            command: command.iter().map(|value| (*value).into()).collect(),
            env: BTreeMap::new(),
            timeout_seconds,
            release_target: None,
            expected_manifest_digest: None,
            router_receipts_path: None,
        }
    }

    #[test]
    fn successful_job_is_bounded_and_records_output_digest() {
        let root = temp_root();
        fs::create_dir_all(&root).unwrap();
        let receipts = root.join("receipts.jsonl");
        fs::write(
            &receipts,
            "{\"execution_id\":\"exec-1\",\"experiment_id\":\"experiment-1\",\"prompt_tokens\":12,\"completion_tokens\":3,\"cost_micros\":7}\n",
        )
        .unwrap();
        let mut trial = job(&root, &["/bin/sh", "-c", "printf hello"], 2);
        trial.router_receipts_path = Some(receipts.display().to_string());
        let result = run_job(&trial, &root);
        assert_eq!(result.status, "succeeded");
        assert_eq!(result.exit_code, Some(0));
        assert_eq!(result.stdout_excerpt, "hello");
        assert_eq!(result.stdout_sha256, digest(b"hello"));
        assert_eq!(result.router_prompt_tokens, Some(12));
        assert_eq!(result.router_completion_tokens, Some(3));
        assert_eq!(result.router_cost_micros, Some(7));
        assert_eq!(result.router_unknown_cost_rows, Some(0));
        assert!(result.router_receipts_error.is_none());
        assert!(result.error.is_none());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn worker_owns_router_correlation_identity() {
        let root = temp_root();
        fs::create_dir_all(&root).unwrap();
        let mut trial = job(
            &root,
            &["/bin/sh", "-c", "printf '%s/%s/%s/%s' \"$TASK_FABRIC_SVN_EXPERIMENT_ID\" \"$TASK_FABRIC_SVN_TRIAL_ID\" \"$TASK_FABRIC_SVN_WORK_ID\" \"$TASK_FABRIC_SVN_TRACE_ID\""],
            2,
        );
        let result = run_job(&trial, &root);
        assert_eq!(result.stdout_excerpt, "experiment-1/trial-1/experiment-1/run-1");
        trial.env.insert("TASK_FABRIC_SVN_WORK_ID".into(), "spoof".into());
        assert!(validate_job(&trial).is_err());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn timeout_is_recorded_without_claiming_success() {
        let root = temp_root();
        fs::create_dir_all(&root).unwrap();
        let result = run_job(&job(&root, &["/bin/sh", "-c", "sleep 2"], 1), &root);
        assert_eq!(result.status, "timed_out");
        assert_ne!(result.status, "succeeded");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn interrupted_running_job_is_requeued() {
        let root = temp_root();
        let inbox = root.join("inbox");
        let running = root.join("running");
        fs::create_dir_all(&running).unwrap();
        fs::write(running.join("job-1.json"), b"{}").unwrap();
        recover_running(&inbox, &running).unwrap();
        assert!(inbox.join("job-1.json").is_file());
        assert!(!running.join("job-1.json").exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn release_bound_job_requires_pointer_and_staged_marker() {
        let root = temp_root();
        let state = root.join("state");
        let cwd = root.join("cwd");
        fs::create_dir_all(&cwd).unwrap();
        let pointer = experiment::ActiveReleasePointer {
            schema_version: experiment::EXPERIMENT_SCHEMA_VERSION,
            target: "governor".into(),
            active_manifest_digest: "a".repeat(64),
            predecessor_manifest_digest: None,
            activated_at_epoch_seconds: 1,
        };
        worldstate::atomic_write(&experiment::release_pointer_path(&state, "governor").unwrap(), &pointer).unwrap();
        let marker = state
            .join("experiments")
            .join("releases")
            .join("governor")
            .join("a".repeat(64));
        fs::create_dir_all(&marker).unwrap();
        fs::write(marker.join("release.json"), b"staged").unwrap();
        let mut trial = job(&cwd, &["/bin/echo", "loaded"], 2);
        trial.release_target = Some("governor".into());
        trial.expected_manifest_digest = Some("a".repeat(64));
        let result = run_job(&trial, &state);
        assert_eq!(result.status, "succeeded");
        assert_eq!(result.release_manifest_digest, Some("a".repeat(64)));
        fs::remove_dir_all(root).unwrap();
    }
}
