fn main() {
    let commit = std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "unknown".into());
    println!("cargo:rustc-env=TASK_FABRIC_SVN_SOURCE_COMMIT={commit}");
    println!("cargo:rerun-if-changed=../../.git/index");
}
