//! Autonomous discovery and mining of workstation projects and cognitive worlds.
//!
//! Scans project root folders (such as `~/Work`), detects programming languages,
//! build tools, test runners, matching systemd user services, and synthesizes rich,
//! typed `Project` specifications for registration in `projects.toml`.

use crate::{
    ActionConfig, AgentConfig, CoreError, Project, ProjectConfig, ResourceConfig,
    ScratchpadConfig, ServiceConfig,
};
use serde::Serialize;
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize)]
struct ProjectTableWrapper<'a> {
    projects: &'a [Project],
}

/// Discovered world report containing mined project definitions and candidate statistics.
#[derive(Debug, Clone, Serialize)]
pub struct MineReport {
    pub scanned_root: PathBuf,
    pub existing_count: usize,
    pub discovered_count: usize,
    pub projects: Vec<Project>,
}

/// Mines a single directory and constructs a rich `Project` definition if eligible.
pub fn mine_project_directory(dir: &Path) -> Option<Project> {
    if !dir.is_dir() {
        return None;
    }
    let file_name = dir.file_name()?.to_str()?;
    if file_name.starts_with('.') || file_name == "target" || file_name == "node_modules" || file_name == "scratch" {
        return None;
    }

    let canonical_repo = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
    let id = sanitize_project_id(file_name);
    let label = derive_project_label(file_name);
    let mut aliases = Vec::new();
    if file_name != id && !file_name.eq_ignore_ascii_case(&id) {
        aliases.push(file_name.to_string());
    }

    let objective = extract_objective(dir).unwrap_or_else(|| format!("Workstation world for {}", label));
    let herdr_session = Some(id.clone());
    let phase = "active".to_string();
    let next_action = "Review workspace or run focused checks".to_string();

    let agents = vec![
        AgentConfig {
            id: "codex".into(),
            label: "Codex".into(),
            kind: "herdr".into(),
            identity: "codex".into(),
            required: false,
            owner_world_id: Some(id.clone()),
        },
        AgentConfig {
            id: "claude".into(),
            label: "Claude".into(),
            kind: "herdr".into(),
            identity: "claude".into(),
            required: false,
            owner_world_id: Some(id.clone()),
        },
    ];

    let services = detect_services(&id, file_name);
    let actions = detect_actions(&id, &label, dir);
    let resources = vec![ResourceConfig {
        id: "repo".into(),
        kind: "repository".into(),
        target: canonical_repo.display().to_string(),
        surface_kind: None,
        required: false,
        owner_world_id: Some(id.clone()),
    }];

    let scratchpad = Some(ScratchpadConfig {
        enabled: true,
        command: None,
        terminal: None,
        floating: true,
    });

    Some(Project {
        id,
        label,
        aliases,
        repo: canonical_repo,
        herdr_session,
        objective,
        phase,
        next_action,
        semantic_profile: None,
        resume_action_id: actions.first().map(|a| a.id.clone()),
        agents,
        services,
        resources,
        study_program_refs: Vec::new(),
        study_domain_refs: Vec::new(),
        research_question_refs: Vec::new(),
        actions,
        scratchpad,
        theme: None,
        resume: Vec::new(),
    })
}

/// Scans a workspace root directory (e.g. `~/Work`) for unconfigured projects.
pub fn scan_unregistered_projects(root: &Path, existing_config: &ProjectConfig) -> Result<MineReport, CoreError> {
    let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let entries = fs::read_dir(&root).map_err(|source| CoreError::Io {
        path: root.clone(),
        source,
    })?;

    let mut existing_ids = HashSet::new();
    let mut existing_repos = HashSet::new();

    for p in &existing_config.projects {
        existing_ids.insert(p.id.to_ascii_lowercase());
        for alias in &p.aliases {
            existing_ids.insert(alias.to_ascii_lowercase());
        }
        if let Ok(c) = p.repo.canonicalize() {
            existing_repos.insert(c);
        } else {
            existing_repos.insert(p.repo.clone());
        }
    }

    let mut discovered = Vec::new();
    let mut seen_ids = existing_ids.clone();

    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }

        let canonical = path.canonicalize().unwrap_or_else(|_| path.clone());
        if existing_repos.contains(&canonical) {
            continue;
        }

        if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
            let id = sanitize_project_id(name);
            if existing_ids.contains(&id.to_ascii_lowercase()) || existing_ids.contains(&name.to_ascii_lowercase()) {
                continue;
            }
        }

        if let Some(mut project) = mine_project_directory(&path) {
            if seen_ids.contains(&project.id.to_ascii_lowercase()) {
                // Disambiguate id if multiple directories resolve to the same sanitized id
                let base_id = project.id.clone();
                let mut suffix = 2;
                while seen_ids.contains(&format!("{}-{}", base_id, suffix)) {
                    suffix += 1;
                }
                project.id = format!("{}-{}", base_id, suffix);
                project.herdr_session = Some(project.id.clone());
            }

            seen_ids.insert(project.id.to_ascii_lowercase());
            discovered.push(project);
        }
    }

    // Sort deterministically by id
    discovered.sort_by(|a, b| a.id.cmp(&b.id));

    Ok(MineReport {
        scanned_root: root,
        existing_count: existing_config.projects.len(),
        discovered_count: discovered.len(),
        projects: discovered,
    })
}

/// Appends new project definitions to an existing `projects.toml` without clobbering
/// existing configuration comments or formatting.
pub fn append_projects_to_config(config_path: &Path, new_projects: &[Project]) -> Result<usize, CoreError> {
    if new_projects.is_empty() {
        return Ok(0);
    }

    let existing_text = if config_path.exists() {
        fs::read_to_string(config_path).map_err(|source| CoreError::Io {
            path: config_path.to_path_buf(),
            source,
        })?
    } else {
        String::from("version = 1\n\n")
    };

    // Serialize new projects as TOML table blocks
    let wrapper = ProjectTableWrapper {
        projects: new_projects,
    };
    let appended_toml = toml::to_string_pretty(&wrapper).map_err(|source| CoreError::Io {
        path: config_path.to_path_buf(),
        source: std::io::Error::new(std::io::ErrorKind::InvalidData, source.to_string()),
    })?;

    let mut combined_text = existing_text.trim_end().to_string();
    combined_text.push_str("\n\n");
    combined_text.push_str(&appended_toml);
    combined_text.push('\n');

    // Test parse and validate to guarantee invariants before writing to disk
    let verified_config = ProjectConfig::from_toml_str(&combined_text, config_path)?;
    let issues = verified_config.validate();
    if !issues.is_empty() {
        return Err(CoreError::Io {
            path: config_path.to_path_buf(),
            source: std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("Validation failed: {}", issues.join("; ")),
            ),
        });
    }

    fs::write(config_path, combined_text).map_err(|source| CoreError::Io {
        path: config_path.to_path_buf(),
        source,
    })?;

    Ok(new_projects.len())
}

fn sanitize_project_id(name: &str) -> String {
    let mut result = String::new();
    for c in name.chars() {
        if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
            result.push(c.to_ascii_lowercase());
        } else if c == ' ' || c == '.' {
            result.push('-');
        }
    }
    let trimmed = result.trim_matches('-');
    if trimmed.is_empty() {
        "project".to_string()
    } else {
        trimmed.to_string()
    }
}

fn derive_project_label(name: &str) -> String {
    // If the name has camelCase boundaries, split them
    let mut spaced = String::new();
    let chars: Vec<char> = name.chars().collect();
    for i in 0..chars.len() {
        let c = chars[i];
        if i > 0 && c.is_ascii_uppercase() {
            let prev = chars[i - 1];
            let next = chars.get(i + 1);
            if prev.is_ascii_lowercase() || (prev.is_ascii_uppercase() && next.is_some_and(|n| n.is_ascii_lowercase())) {
                if !spaced.ends_with(' ') {
                    spaced.push(' ');
                }
            }
        }
        if c == '-' || c == '_' {
            spaced.push(' ');
        } else {
            spaced.push(c);
        }
    }

    spaced
        .split_whitespace()
        .map(|w| {
            if w.chars().all(|c| c.is_ascii_uppercase()) && w.len() <= 4 {
                w.to_string()
            } else {
                let mut chars = w.chars();
                match chars.next() {
                    Some(first) => first.to_ascii_uppercase().to_string() + chars.as_str(),
                    None => String::new(),
                }
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn extract_objective(dir: &Path) -> Option<String> {
    let candidates = ["README.md", "readme.md", "README", "README.org", "README.txt"];
    for name in candidates {
        let path = dir.join(name);
        if let Ok(content) = fs::read_to_string(&path) {
            let mut in_code_block = false;
            let mut in_frontmatter = false;
            let mut frontmatter_seen = 0;

            for line in content.lines().take(50) {
                let trimmed = line.trim();
                if trimmed == "---" {
                    frontmatter_seen += 1;
                    in_frontmatter = frontmatter_seen == 1;
                    continue;
                }
                if in_frontmatter {
                    continue;
                }

                if trimmed.starts_with("```") {
                    in_code_block = !in_code_block;
                    continue;
                }
                if in_code_block {
                    continue;
                }

                if trimmed.is_empty()
                    || trimmed.starts_with('#')
                    || trimmed.starts_with("<!--")
                    || trimmed.starts_with("[![")
                    || trimmed.starts_with("![")
                    || trimmed.starts_with("==")
                    || trimmed.starts_with("<div")
                    || trimmed.starts_with("<p")
                    || trimmed.starts_with("<img")
                    || trimmed.starts_with("</")
                    || trimmed.starts_with("source ")
                    || trimmed.starts_with("npm ")
                    || trimmed.starts_with("cargo ")
                    || trimmed.contains("English |")
                    || trimmed.contains("README.zh.md")
                {
                    continue;
                }

                // Strip blockquote markers and alerts
                let mut cleaned = trimmed;
                if let Some(rest) = cleaned.strip_prefix('>') {
                    cleaned = rest.trim();
                }
                if cleaned.starts_with("[!") {
                    continue;
                }

                // Strip markdown formatting & HTML tags
                let without_md = cleaned
                    .replace("**", "")
                    .replace('*', "")
                    .replace('`', "");
                let stripped_html = strip_html_tags(&without_md);
                let cleaned = strip_markdown_links(&stripped_html).trim().to_string();

                if cleaned.len() >= 15 {
                    if cleaned.len() > 160 {
                        let mut truncated = cleaned[..157].to_string();
                        if let Some(last_space) = truncated.rfind(' ') {
                            truncated.truncate(last_space);
                        }
                        truncated.push_str("...");
                        return Some(truncated);
                    }
                    return Some(cleaned);
                }
            }
        }
    }
    None
}

fn strip_html_tags(s: &str) -> String {
    let mut res = String::new();
    let mut in_tag = false;
    for c in s.chars() {
        if c == '<' {
            in_tag = true;
        } else if c == '>' {
            in_tag = false;
        } else if !in_tag {
            res.push(c);
        }
    }
    res
}

fn strip_markdown_links(s: &str) -> String {
    // Replaces [label](url) with label
    let mut res = String::new();
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '[' {
            let mut label = String::new();
            let mut found_close = false;
            for inner in chars.by_ref() {
                if inner == ']' {
                    found_close = true;
                    break;
                }
                label.push(inner);
            }
            if found_close && chars.peek() == Some(&'(') {
                chars.next(); // consume '('
                for inner in chars.by_ref() {
                    if inner == ')' {
                        break;
                    }
                }
                res.push_str(&label);
            } else {
                res.push('[');
                res.push_str(&label);
                if found_close {
                    res.push(']');
                }
            }
        } else {
            res.push(c);
        }
    }
    res
}

fn detect_services(project_id: &str, folder_name: &str) -> Vec<ServiceConfig> {
    let mut services = Vec::new();
    let home = match std::env::var_os("HOME") {
        Some(h) => PathBuf::from(h),
        None => return services,
    };
    let systemd_dir = home.join(".config/systemd/user");
    let entries = match fs::read_dir(&systemd_dir) {
        Ok(e) => e,
        Err(_) => return services,
    };

    let p_norm = project_id.to_ascii_lowercase().replace('_', "-");
    let f_norm = folder_name.to_ascii_lowercase().replace('_', "-");

    // Generic services that belong to workstation infrastructure, not individual projects
    let infrastructure_services = [
        "task-fabric-svn-workd.service",
        "task-fabric-svn-soundd.service",
        "task-fabric-svn-approval-relay.service",
        "task-fabric-svn-voice-gateway.service",
        "task-fabric-svn-weekly-reflection.service",
        "task-fabric-svn-exocortex-sync.service",
        "voxtype.service",
        "sunshine.service",
    ];

    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("service") {
            continue;
        }
        let file_stem = match path.file_stem().and_then(|s| s.to_str()) {
            Some(s) => s,
            None => continue,
        };
        let file_name = match path.file_name().and_then(|s| s.to_str()) {
            Some(s) => s,
            None => continue,
        };

        if infrastructure_services.contains(&file_name) && p_norm != "task-fabric-svn" && p_norm != "task-fabric-svn-soundd" {
            continue;
        }

        let s_norm = file_stem.to_ascii_lowercase().replace('_', "-");
        let matches = s_norm == p_norm
            || s_norm == f_norm
            || s_norm.starts_with(&format!("{}-", p_norm))
            || s_norm.starts_with(&format!("{}-", f_norm))
            || p_norm.starts_with(&format!("{}-", s_norm))
            || (p_norm.starts_with("example-press") && s_norm.starts_with("example-press"));

        if matches {
            services.push(ServiceConfig::systemd(file_stem, file_name));
        }
    }

    services
}

fn detect_actions(project_id: &str, label: &str, dir: &Path) -> Vec<ActionConfig> {
    let mut actions = Vec::new();
    let app_id = format!("org.task-fabric-svn.work.{}", project_id.replace('_', "-"));

    // 1. Primary Continue action (Herdr session)
    actions.push(ActionConfig::new(
        "continue",
        &format!("Continue {}", label),
        &[
            "omarchy-launch-tui",
            &format!("--app-id={}", app_id),
            "herdr",
            "--session",
            project_id,
        ],
    ));

    // 2. Open repository
    actions.push(ActionConfig::new(
        "open-repo",
        "Open repository",
        &["omarchy-launch-editor", "."],
    ));

    // 3. Tech stack specific actions
    if dir.join("Cargo.toml").exists() {
        actions.push(ActionConfig::new(
            "run-tests",
            "Run workspace tests",
            &[
                "omarchy-launch-tui",
                &format!("--app-id={}.tests", app_id),
                "cargo",
                "test",
                "--workspace",
            ],
        ));
        actions.push(ActionConfig::new(
            "check",
            "Cargo check",
            &[
                "omarchy-launch-tui",
                &format!("--app-id={}.check", app_id),
                "cargo",
                "check",
            ],
        ));
    } else if dir.join("package.json").exists() {
        let pkg_content = fs::read_to_string(dir.join("package.json")).unwrap_or_default();
        let (pm, test_cmd, dev_cmd, build_cmd) = if dir.join("pnpm-lock.yaml").exists() {
            ("pnpm", vec!["pnpm", "test"], vec!["pnpm", "run", "dev"], vec!["pnpm", "run", "build"])
        } else if dir.join("yarn.lock").exists() {
            ("yarn", vec!["yarn", "test"], vec!["yarn", "dev"], vec!["yarn", "build"])
        } else if dir.join("bun.lockb").exists() || dir.join("bun.lock").exists() {
            ("bun", vec!["bun", "test"], vec!["bun", "run", "dev"], vec!["bun", "run", "build"])
        } else {
            ("npm", vec!["npm", "test"], vec!["npm", "run", "dev"], vec!["npm", "run", "build"])
        };

        if pkg_content.contains("\"test\"") {
            let id_arg = format!("{}.tests", app_id);
            let mut full = vec!["omarchy-launch-tui", &id_arg];
            full.extend(test_cmd);
            actions.push(ActionConfig {
                id: "run-tests".into(),
                label: format!("Run tests ({})", pm),
                command: full.into_iter().map(String::from).collect(),
                cwd: None,
                risk: "review".into(),
            });
        }
        if pkg_content.contains("\"dev\"") {
            let id_arg = format!("{}.dev", app_id);
            let mut full = vec!["omarchy-launch-tui", &id_arg];
            full.extend(dev_cmd);
            actions.push(ActionConfig {
                id: "dev".into(),
                label: format!("Start dev server ({})", pm),
                command: full.into_iter().map(String::from).collect(),
                cwd: None,
                risk: "review".into(),
            });
        }
        if pkg_content.contains("\"build\"") {
            let id_arg = format!("{}.build", app_id);
            let mut full = vec!["omarchy-launch-tui", &id_arg];
            full.extend(build_cmd);
            actions.push(ActionConfig {
                id: "build".into(),
                label: format!("Build project ({})", pm),
                command: full.into_iter().map(String::from).collect(),
                cwd: None,
                risk: "review".into(),
            });
        }
    } else if dir.join("gradlew").exists() || dir.join("build.gradle").exists() || dir.join("build.gradle.kts").exists() {
        let gradle_bin = if dir.join("gradlew").exists() { "./gradlew" } else { "gradle" };
        actions.push(ActionConfig::new(
            "run-tests",
            "Run Gradle tests",
            &[
                "omarchy-launch-tui",
                &format!("--app-id={}.tests", app_id),
                gradle_bin,
                "test",
            ],
        ));
    } else if dir.join("pyproject.toml").exists() || dir.join("requirements.txt").exists() || dir.join("setup.py").exists() {
        actions.push(ActionConfig::new(
            "run-tests",
            "Run pytest",
            &[
                "omarchy-launch-tui",
                &format!("--app-id={}.tests", app_id),
                "pytest",
            ],
        ));
    } else if dir.join("go.mod").exists() {
        actions.push(ActionConfig::new(
            "run-tests",
            "Run Go tests",
            &[
                "omarchy-launch-tui",
                &format!("--app-id={}.tests", app_id),
                "go",
                "test",
                "./...",
            ],
        ));
    } else if dir.join("Makefile").exists() {
        actions.push(ActionConfig::new(
            "run-tests",
            "Run Make test",
            &[
                "omarchy-launch-tui",
                &format!("--app-id={}.tests", app_id),
                "make",
                "test",
            ],
        ));
    }

    // 4. Commit checkpoint review
    actions.push(ActionConfig::new(
        "commit-checkpoint",
        "Review commit checkpoint",
        &[
            "omarchy-launch-tui",
            "--app-id=org.task-fabric-svn.work.checkpoint",
            "git",
            "status",
            "--short",
            "--branch",
        ],
    ));

    actions
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitizes_project_ids() {
        assert_eq!(sanitize_project_id("3D-Printing-System"), "3d-printing-system");
        assert_eq!(sanitize_project_id("Example Project"), "example-project");
        assert_eq!(sanitize_project_id("ExamplePress-reading-app"), "examplepress-reading-app");
        assert_eq!(sanitize_project_id("task_fabric_svn"), "task_fabric_svn");
        assert_eq!(sanitize_project_id("gov.demo"), "gov-demo");
    }

    #[test]
    fn derives_project_labels() {
        assert_eq!(derive_project_label("3D-Printing-System"), "3D Printing System");
        assert_eq!(derive_project_label("ExamplePress-reading-app"), "Example Press Reading App");
        assert_eq!(derive_project_label("task-fabric-svn"), "Task Fabric Svn");
    }

    #[test]
    fn detects_cargo_actions() {
        let temp_dir = std::env::temp_dir().join(format!("task-fabric-svn-mine-test-{}", std::process::id()));
        let _ = fs::create_dir_all(&temp_dir);
        fs::write(temp_dir.join("Cargo.toml"), "[package]\nname=\"foo\"\nversion=\"0.1.0\"").unwrap();
        let actions = detect_actions("foo", "Foo", &temp_dir);
        assert!(actions.iter().any(|a| a.id == "run-tests"));
        assert!(actions.iter().any(|a| a.id == "continue"));
        let _ = fs::remove_dir_all(temp_dir);
    }

    #[test]
    fn scans_and_appends_new_projects_reliably() {
        let base_temp = std::env::temp_dir().join(format!("task-fabric-svn-scan-test-{}", std::process::id()));
        let workspace_root = base_temp.join("Work");
        let _ = fs::create_dir_all(&workspace_root);

        // Project A (already configured)
        let proj_a = workspace_root.join("alpha-service");
        fs::create_dir_all(&proj_a).unwrap();
        fs::write(proj_a.join("README.md"), "# Alpha Service\n\nCore alpha orchestration node.").unwrap();

        // Project B (unregistered)
        let proj_b = workspace_root.join("beta-worker");
        fs::create_dir_all(&proj_b).unwrap();
        fs::write(proj_b.join("README.md"), "# Beta Worker\n\nHigh throughput task consumer.").unwrap();
        fs::write(proj_b.join("Cargo.toml"), "[package]\nname=\"beta-worker\"\nversion=\"0.1.0\"").unwrap();

        // Config file with only Project A
        let config_file = base_temp.join("projects.toml");
        let initial_toml = format!(
            "version = 1\n\n[[projects]]\nid = \"alpha-service\"\nlabel = \"Alpha Service\"\nrepo = \"{}\"\nobjective = \"Core alpha\"\nphase = \"active\"\nnext_action = \"Inspect\"\n\n[[projects.actions]]\nid = \"continue\"\nlabel = \"Continue\"\ncommand = [\"echo\", \"alpha\"]\nrisk = \"review\"\n",
            proj_a.display()
        );
        fs::write(&config_file, &initial_toml).unwrap();

        let initial_config = ProjectConfig::from_toml(&config_file).unwrap();
        assert_eq!(initial_config.projects.len(), 1);

        let report = scan_unregistered_projects(&workspace_root, &initial_config).unwrap();
        assert_eq!(report.discovered_count, 1);
        assert_eq!(report.projects[0].id, "beta-worker");
        assert_eq!(report.projects[0].objective, "High throughput task consumer.");

        // Append to config
        let appended_count = append_projects_to_config(&config_file, &report.projects).unwrap();
        assert_eq!(appended_count, 1);

        // Reload and verify
        let updated_config = ProjectConfig::from_toml(&config_file).unwrap();
        assert_eq!(updated_config.projects.len(), 2);
        assert_eq!(updated_config.projects[1].id, "beta-worker");
        assert_eq!(updated_config.validate(), Vec::<String>::new());

        let _ = fs::remove_dir_all(base_temp);
    }
}
