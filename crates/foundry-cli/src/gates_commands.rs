use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use comfy_table::{ContentArrangement, Table};
use foundry_core::gates::{GateDefinition, read_gates_file, write_gates_file};

/// Display current gates for a project in table format.
pub fn show(project_dir: &Path) -> Result<()> {
    let gates = read_gates_file(project_dir)?;

    if gates.is_empty() {
        println!("No .hone-gates.json found in {}", project_dir.display());
        println!();
        println!("Run `foundry gates --init <project>` to derive gates for this project.");
        return Ok(());
    }

    let mut table = Table::new();
    table.set_content_arrangement(ContentArrangement::Dynamic);
    table.set_header(vec!["Name", "Command", "Required"]);

    for gate in &gates {
        let required = if gate.required { "yes" } else { "no" };
        table.add_row(vec![gate.name.as_str(), gate.command.as_str(), required]);
    }

    println!("{table}");
    Ok(())
}

/// Derive gates by inspecting the project and invoking Claude, then write .hone-gates.json.
pub fn init(project_dir: &Path) -> Result<()> {
    let gate_path = project_dir.join(".hone-gates.json");
    if gate_path.exists() {
        println!("Warning: {} already exists and will be overwritten.", gate_path.display());
    }

    println!("Inspecting project at {}...", project_dir.display());
    let context = gather_context(project_dir);

    println!("Invoking Claude to derive quality gates...");
    let json_output = invoke_claude(project_dir, &context)?;

    let gates = parse_gates_response(&json_output)?;

    if gates.is_empty() {
        bail!(
            "Claude returned no gates — check that the project has recognizable build/test tooling"
        );
    }

    write_gates_file(project_dir, &gates)?;

    println!("Wrote {} gates to {}", gates.len(), gate_path.display());

    // Show the result
    let mut table = Table::new();
    table.set_content_arrangement(ContentArrangement::Dynamic);
    table.set_header(vec!["Name", "Command", "Required"]);

    for gate in &gates {
        let required = if gate.required { "yes" } else { "no" };
        table.add_row(vec![gate.name.as_str(), gate.command.as_str(), required]);
    }

    println!("{table}");
    Ok(())
}

// -- Context gathering --

/// Files whose presence indicates a language/stack.
const PACKAGE_FILES: &[&str] = &[
    "package.json",
    "Cargo.toml",
    "pyproject.toml",
    "mix.exs",
    "CMakeLists.txt",
    "go.mod",
    "Gemfile",
    "build.gradle",
    "build.gradle.kts",
    "pom.xml",
    "Makefile",
];

/// CI configuration paths to check.
const CI_FILES: &[&str] = &[".gitlab-ci.yml", ".circleci/config.yml", "Jenkinsfile"];

/// Tool config file patterns (exact names).
const TOOL_CONFIG_FILES: &[&str] = &[
    ".eslintrc",
    ".eslintrc.js",
    ".eslintrc.json",
    ".eslintrc.yml",
    "eslint.config.js",
    "eslint.config.mjs",
    "biome.json",
    "ruff.toml",
    ".ruff.toml",
    "rustfmt.toml",
    "clippy.toml",
    ".credo.exs",
    ".formatter.exs",
    "tsconfig.json",
    "bunfig.toml",
    "deno.json",
    ".prettierrc",
    ".prettierrc.json",
    ".prettierrc.yml",
    "prettier.config.js",
    "prettier.config.mjs",
];

/// Lockfile → package manager mapping.
const LOCKFILE_MAP: &[(&str, &str)] = &[
    ("bun.lockb", "bun"),
    ("pnpm-lock.yaml", "pnpm"),
    ("yarn.lock", "yarn"),
    ("package-lock.json", "npm"),
    ("uv.lock", "uv"),
    ("poetry.lock", "poetry"),
    ("Pipfile.lock", "pipenv"),
    ("Cargo.lock", "cargo"),
    ("go.sum", "go"),
    ("Gemfile.lock", "bundler"),
    ("mix.lock", "mix"),
];

/// Directories to exclude from the tree listing.
const TREE_EXCLUDE: &[&str] = &[
    "node_modules",
    "_build",
    "deps",
    "__pycache__",
    "target",
    "dist",
    "build",
    ".git",
    ".elixir_ls",
    ".next",
    "vendor",
];

/// Gather project context by inspecting the filesystem.
fn gather_context(project_dir: &Path) -> String {
    let mut sections: Vec<String> = Vec::new();

    // Package/build files
    let mut found_packages = Vec::new();
    for name in PACKAGE_FILES {
        if project_dir.join(name).exists() {
            found_packages.push((*name).to_string());
        }
    }
    if !found_packages.is_empty() {
        sections.push(format!("## Package/Build Files\n{}", found_packages.join(", ")));
    }

    // CI configurations
    let mut found_ci = Vec::new();
    let workflows_dir = project_dir.join(".github/workflows");
    if workflows_dir.is_dir() {
        if let Ok(entries) = std::fs::read_dir(&workflows_dir) {
            for entry in entries.flatten() {
                let file_name = entry.file_name();
                let path = Path::new(&file_name);
                let is_yaml = path.extension().is_some_and(|ext| {
                    ext.eq_ignore_ascii_case("yml") || ext.eq_ignore_ascii_case("yaml")
                });
                if is_yaml {
                    let name = file_name.to_string_lossy();
                    found_ci.push(format!(".github/workflows/{name}"));
                }
            }
        }
    }
    for name in CI_FILES {
        if project_dir.join(name).exists() {
            found_ci.push((*name).to_string());
        }
    }
    if !found_ci.is_empty() {
        sections.push(format!("## CI Configurations\n{}", found_ci.join(", ")));
    }

    // Tool config files
    let mut found_tools = Vec::new();
    for name in TOOL_CONFIG_FILES {
        if project_dir.join(name).exists() {
            found_tools.push((*name).to_string());
        }
    }
    if !found_tools.is_empty() {
        sections.push(format!("## Tool Config Files\n{}", found_tools.join(", ")));
    }

    // Lockfiles / package manager detection
    let mut found_managers = Vec::new();
    for (lockfile, manager) in LOCKFILE_MAP {
        if project_dir.join(lockfile).exists() {
            found_managers.push(format!("{lockfile} → {manager}"));
        }
    }
    if !found_managers.is_empty() {
        sections
            .push(format!("## Package Managers (from lockfiles)\n{}", found_managers.join(", ")));
    }

    // Directory tree (3 levels deep)
    let tree = build_tree(project_dir, 3);
    if !tree.is_empty() {
        sections.push(format!("## Directory Tree (3 levels)\n{tree}"));
    }

    sections.join("\n\n")
}

/// Build a simple directory tree listing, excluding hidden dirs and common build artifacts.
fn build_tree(root: &Path, max_depth: usize) -> String {
    let mut lines = Vec::new();
    collect_tree(root, 0, max_depth, &mut lines);
    lines.join("\n")
}

fn collect_tree(dir: &Path, depth: usize, max_depth: usize, lines: &mut Vec<String>) {
    if depth >= max_depth {
        return;
    }

    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };

    let mut items: Vec<_> = entries.filter_map(Result::ok).collect();
    items.sort_by_key(std::fs::DirEntry::file_name);

    for entry in items {
        let name = entry.file_name().to_string_lossy().to_string();

        // Skip hidden entries and excluded directories
        if name.starts_with('.') || TREE_EXCLUDE.contains(&name.as_str()) {
            continue;
        }

        let is_dir = entry.file_type().is_ok_and(|t| t.is_dir());
        let indent = "  ".repeat(depth);
        let suffix = if is_dir { "/" } else { "" };
        lines.push(format!("{indent}{name}{suffix}"));

        if is_dir {
            collect_tree(&entry.path(), depth + 1, max_depth, lines);
        }
    }
}

// -- Claude invocation --

/// Invoke Claude CLI to derive gates from the gathered context.
fn invoke_claude(project_dir: &Path, context: &str) -> Result<String> {
    let prompt = build_prompt(context);

    let output = Command::new("claude")
        .arg("--print")
        .arg("--model")
        .arg("claude-sonnet-4-6")
        .arg("--dangerously-skip-permissions")
        .arg("--allowedTools")
        .arg("Read Glob Grep")
        .arg("-p")
        .arg(&prompt)
        .current_dir(project_dir)
        .env("CLAUDE_CODE", "")
        .output()
        .context("failed to run `claude` CLI — is it installed and on PATH?")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("claude CLI exited with {}: {stderr}", output.status);
    }

    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

/// Build the prompt sent to Claude for gate derivation.
fn build_prompt(context: &str) -> String {
    format!(
        r#"You are inspecting a software project to discover its quality gates (test, lint, format, typecheck, build, security audit commands).

Here is what was found by scanning the filesystem:

{context}

Your task:
1. Read actual project files (package.json scripts, CI configs, Makefiles, Cargo.toml, pyproject.toml, etc.) using the tools available to you.
2. Discover what quality commands exist: test, lint, format, typecheck, build, security audit.
3. Return ONLY a JSON array of gate objects. No other text, no markdown fences, no explanation.
4. Each gate object must have exactly these fields:
   - "name": short identifier (e.g. "test", "fmt", "lint", "typecheck", "build", "security-audit")
   - "command": the exact shell command to run
   - "required": boolean — true for most gates, false for security/audit gates
5. NEVER invent commands — every gate must come from a file you actually read.
6. Mark security/audit gates as required: false. Everything else should be required: true.
7. Combine related commands with && where appropriate (e.g. "cargo fmt --all -- --check && cargo clippy --workspace -- -D warnings").
8. Do NOT include commands that only work in CI environments (e.g. commands requiring CI-specific env vars).
9. Prefer check/dry-run variants for formatting (e.g. "cargo fmt --check" not "cargo fmt").

Return ONLY the JSON array, for example:
[{{"name": "test", "command": "cargo test", "required": true}}]"#
    )
}

// -- Response parsing --

/// Intermediate struct for parsing Claude's JSON response.
#[derive(serde::Deserialize)]
struct RawGateResponse {
    name: String,
    command: String,
    required: bool,
}

/// Parse Claude's JSON response into `GateDefinition` values.
fn parse_gates_response(raw: &str) -> Result<Vec<GateDefinition>> {
    // Claude might wrap JSON in markdown fences — strip them.
    let trimmed = raw.trim();
    let json_str = if trimmed.starts_with("```") {
        // Strip opening fence (```json or ```)
        let after_open = trimmed.find('\n').map_or(trimmed, |i| &trimmed[i + 1..]);
        // Strip closing fence
        after_open.rfind("```").map_or(after_open, |i| &after_open[..i]).trim()
    } else {
        trimmed
    };

    // Try to find the JSON array within the output (Claude might prefix/suffix text).
    let start = json_str.find('[').context("no JSON array found in Claude response")?;
    let end = json_str.rfind(']').context("no closing bracket found in Claude response")?;
    let array_str = &json_str[start..=end];

    let raw_gates: Vec<RawGateResponse> = serde_json::from_str(array_str)
        .with_context(|| format!("failed to parse gates JSON: {array_str}"))?;

    Ok(raw_gates
        .into_iter()
        .map(|raw| GateDefinition {
            name: raw.name,
            command: raw.command,
            required: raw.required,
            timeout: None,
        })
        .collect())
}

/// Resolve the project directory from either a project name (via registry) or a --dir path.
pub fn resolve_project_dir(
    project: Option<&str>,
    dir: Option<&str>,
    registry_path: &Path,
) -> Result<PathBuf> {
    match (project, dir) {
        (Some(_), Some(_)) => bail!("specify either a project name or --dir, not both"),
        (None, None) => bail!("specify a project name or use --dir <path>"),
        (None, Some(d)) => {
            let path = PathBuf::from(d);
            if !path.is_dir() {
                bail!("{d} is not a directory");
            }
            Ok(path)
        }
        (Some(name), None) => {
            let registry =
                foundry_core::registry::Registry::load(registry_path).with_context(|| {
                    format!("failed to load registry from {}", registry_path.display())
                })?;
            let entry = registry
                .projects
                .iter()
                .find(|p| p.name == name)
                .with_context(|| format!("project {name:?} not found in registry"))?;
            let path = PathBuf::from(&entry.path);
            if !path.is_dir() {
                bail!("registry path for {name:?} does not exist: {}", path.display());
            }
            Ok(path)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_gates_response_plain_json() {
        let input = r#"[{"name":"test","command":"cargo test","required":true},{"name":"fmt","command":"cargo fmt --check","required":true}]"#;
        let gates = parse_gates_response(input).unwrap();
        assert_eq!(gates.len(), 2);
        assert_eq!(gates[0].name, "test");
        assert_eq!(gates[0].command, "cargo test");
        assert!(gates[0].required);
        assert_eq!(gates[1].name, "fmt");
    }

    #[test]
    fn parse_gates_response_with_markdown_fences() {
        let input =
            "```json\n[{\"name\":\"lint\",\"command\":\"npm run lint\",\"required\":true}]\n```";
        let gates = parse_gates_response(input).unwrap();
        assert_eq!(gates.len(), 1);
        assert_eq!(gates[0].name, "lint");
    }

    #[test]
    fn parse_gates_response_with_surrounding_text() {
        let input = "Here are the gates:\n[{\"name\":\"build\",\"command\":\"npm run build\",\"required\":true}]\nDone!";
        let gates = parse_gates_response(input).unwrap();
        assert_eq!(gates.len(), 1);
        assert_eq!(gates[0].name, "build");
    }

    #[test]
    fn parse_gates_response_no_array() {
        let input = "No gates found.";
        let err = parse_gates_response(input).unwrap_err();
        assert!(format!("{err:#}").contains("no JSON array"));
    }

    #[test]
    fn gather_context_on_empty_dir() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = gather_context(dir.path());
        // No package files, CI, etc. — context should be minimal (just empty tree)
        assert!(!ctx.contains("## Package/Build Files"));
    }

    #[test]
    fn gather_context_detects_cargo_project() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Cargo.toml"), "[package]\nname = \"test\"").unwrap();
        std::fs::write(dir.path().join("Cargo.lock"), "").unwrap();

        let ctx = gather_context(dir.path());
        assert!(ctx.contains("Cargo.toml"));
        assert!(ctx.contains("Cargo.lock → cargo"));
    }

    #[test]
    fn gather_context_detects_node_project() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("package.json"), "{}").unwrap();
        std::fs::write(dir.path().join("package-lock.json"), "{}").unwrap();
        std::fs::write(dir.path().join("tsconfig.json"), "{}").unwrap();

        let ctx = gather_context(dir.path());
        assert!(ctx.contains("package.json"));
        assert!(ctx.contains("package-lock.json → npm"));
        assert!(ctx.contains("tsconfig.json"));
    }

    #[test]
    fn build_tree_excludes_hidden_and_artifacts() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join(".git")).unwrap();
        std::fs::create_dir(dir.path().join("node_modules")).unwrap();
        std::fs::create_dir(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/main.rs"), "").unwrap();

        let tree = build_tree(dir.path(), 3);
        assert!(!tree.contains(".git"));
        assert!(!tree.contains("node_modules"));
        assert!(tree.contains("src/"));
        assert!(tree.contains("main.rs"));
    }

    #[test]
    fn resolve_project_dir_with_dir_flag() {
        let dir = tempfile::tempdir().unwrap();
        let result = resolve_project_dir(
            None,
            Some(dir.path().to_str().unwrap()),
            Path::new("/nonexistent"),
        )
        .unwrap();
        assert_eq!(result, dir.path());
    }

    #[test]
    fn resolve_project_dir_both_specified() {
        let err =
            resolve_project_dir(Some("foo"), Some("/tmp"), Path::new("/nonexistent")).unwrap_err();
        assert!(format!("{err:#}").contains("not both"));
    }

    #[test]
    fn resolve_project_dir_neither_specified() {
        let err = resolve_project_dir(None, None, Path::new("/nonexistent")).unwrap_err();
        assert!(format!("{err:#}").contains("specify a project name"));
    }
}
