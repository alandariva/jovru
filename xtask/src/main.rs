// xtask is a command-line tool: printing its progress is its job (ADR-0011).
#![allow(clippy::print_stdout, clippy::print_stderr)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]
use serde_json::{Map, Value};
use std::{
    collections::BTreeSet,
    env, fs,
    path::Path,
    process::{Command, ExitCode},
};

#[derive(Debug, PartialEq)]
enum Task {
    Check { full: bool },
    Catalog(String),
}

fn main() -> ExitCode {
    let args: Vec<_> = env::args().skip(1).collect();
    let Some(root) = Path::new(env!("CARGO_MANIFEST_DIR")).parent() else {
        eprintln!("xtask must live inside the workspace.");
        return ExitCode::FAILURE;
    };
    match options(&args) {
        Ok(Task::Check { full }) => match run(root, full) {
            Ok(()) => {
                println!("Gate passed.");
                ExitCode::SUCCESS
            }
            Err(error) => {
                eprintln!("Gate failed: {error}\nLater steps were not run.");
                ExitCode::FAILURE
            }
        },
        Ok(Task::Catalog(tag)) => match catalog(root, &tag) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("Catalog not updated: {error}");
                ExitCode::FAILURE
            }
        },
        Err(error) => {
            eprintln!("{error}");
            ExitCode::FAILURE
        }
    }
}

fn run(root: &Path, full: bool) -> Result<(), String> {
    println!(
        "{} gate. Live tests are skipped in basic mode; authenticated Databricks tests and visual checks are separate.",
        if full { "Full" } else { "Basic" }
    );
    let output = Command::new("cargo")
        .current_dir(root)
        .args(["metadata", "--no-deps", "--format-version", "1", "--locked"])
        .output()
        .map_err(|e| format!("cargo metadata: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "cargo metadata: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    let metadata: Value = serde_json::from_slice(&output.stdout).map_err(|e| e.to_string())?;
    architecture(&metadata)?;
    println!("[OK] Architecture (direct dependencies, including dev and build)");
    command(root, "cargo", &["fmt", "--all", "--check"])?;
    command(
        root,
        "cargo",
        &[
            "clippy",
            "--workspace",
            "--all-targets",
            "--locked",
            "--",
            "-D",
            "warnings",
        ],
    )?;
    // No target filter: cargo also runs the credential compile_fail doctests.
    command(root, "cargo", &["test", "--workspace", "--locked"])?;
    if full {
        live(root)?;
        // Quick mode only proves the benchmarks still run; comparing numbers
        // is done by hand against a saved baseline.
        command(
            root,
            "cargo",
            &["bench", "--locked", "--bench", "hot_paths", "--", "--quick"],
        )?;
        command(root, "cargo", &["deny", "--locked", "check"])?;
    } else {
        println!("[NOT RUN] Audit, benchmarks and real integration: use --full.");
    }
    Ok(())
}

fn options(args: &[String]) -> Result<Task, String> {
    match args
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        ["check"] => Ok(Task::Check { full: false }),
        ["check", "--full"] => Ok(Task::Check { full: true }),
        ["i18n", tag] => Ok(Task::Catalog(tag.to_string())),
        _ => Err("Usage: cargo xtask check [--full] | cargo xtask i18n <locale>. Offline: set CARGO_NET_OFFLINE=true.".into()),
    }
}

/// Creates or refreshes `locales/<tag>.json` with every `tr!` message in `src/`.
/// Existing translations are kept, new messages get an empty value (shown in
/// English) and messages no longer in the code are dropped.
fn catalog(root: &Path, tag: &str) -> Result<(), String> {
    let valid = !tag.is_empty()
        && tag.split('-').all(|part| {
            !part.is_empty() && part.len() <= 8 && part.chars().all(|c| c.is_ascii_alphanumeric())
        });
    if !valid || tag.eq_ignore_ascii_case("en") {
        return Err(format!(
            "{tag:?} is not a BCP 47 tag such as pt-BR (English is the source language)"
        ));
    }
    let mut found = BTreeSet::new();
    let mut pending = vec![root.join("src")];
    while let Some(dir) = pending.pop() {
        for entry in fs::read_dir(&dir).map_err(|e| format!("{}: {e}", dir.display()))? {
            let path = entry.map_err(|e| e.to_string())?.path();
            if path.is_dir() {
                pending.push(path);
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                let source =
                    fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
                found.extend(messages(&source));
            }
        }
    }
    let file = root.join("locales").join(format!("{tag}.json"));
    let existing: Map<String, Value> = match fs::read_to_string(&file) {
        Ok(json) => serde_json::from_str(&json).map_err(|e| format!("{}: {e}", file.display()))?,
        Err(_) => Map::new(),
    };
    let updated: Map<String, Value> = found
        .iter()
        .map(|message| {
            let translation = existing.get(message).cloned().unwrap_or_else(|| "".into());
            (message.clone(), translation)
        })
        .collect();
    let missing = updated.values().filter(|value| *value == "").count();
    let dropped = existing
        .keys()
        .filter(|key| !updated.contains_key(*key))
        .count();
    fs::create_dir_all(file.parent().unwrap_or(root)).map_err(|e| e.to_string())?;
    let json = serde_json::to_string_pretty(&updated).map_err(|e| e.to_string())?;
    fs::write(&file, json + "\n").map_err(|e| format!("{}: {e}", file.display()))?;
    println!(
        "{}: {} messages, {missing} untranslated, {dropped} obsolete removed.",
        file.display(),
        updated.len()
    );
    println!("Register it in CATALOGS (src/i18n.rs) to ship the language.");
    Ok(())
}

/// Returns the string literal of every `tr!(...)` call, unescaped.
fn messages(source: &str) -> Vec<String> {
    let mut found = Vec::new();
    for (start, _) in source.match_indices("tr!(") {
        // Skip other macros ending in `tr!`, such as `include_str!`.
        let before = source[..start].chars().next_back();
        if before.is_some_and(|c| c.is_alphanumeric() || c == '_') {
            continue;
        }
        let mut chars = source[start + 4..].trim_start().chars();
        if chars.next() != Some('"') {
            continue;
        }
        let mut message = String::new();
        while let Some(c) = chars.next() {
            match c {
                '"' => {
                    found.push(message);
                    break;
                }
                '\\' => match chars.next() {
                    Some('n') => message.push('\n'),
                    Some('t') => message.push('\t'),
                    Some(other) => message.push(other),
                    None => break,
                },
                _ => message.push(c),
            }
        }
    }
    found
}

fn architecture(metadata: &Value) -> Result<(), String> {
    for package in metadata["packages"]
        .as_array()
        .ok_or("Package graph missing")?
    {
        let name = package["name"].as_str().ok_or("Package name missing")?;
        for dependency in package["dependencies"]
            .as_array()
            .ok_or("Dependencies missing")?
        {
            let target = dependency["name"]
                .as_str()
                .ok_or("Dependency name missing")?;
            let adapter = name.starts_with("jykre-driver-")
                || name == "jykre-storage"
                || name == "jykre-secrets";
            let backend = name == "jykre-core" || adapter;
            let internal = target == "jykre" || target == "xtask" || target.starts_with("jykre-");
            let ui = target.starts_with("gpui") || target == "tauri";
            let forbidden = (backend && ui)
                || (name == "jykre-core" && (internal || target == "sqlx" || target == "reqwest"))
                || (adapter && internal && target != "jykre-core");
            if forbidden {
                return Err(format!(
                    "Dependency forbidden by the architecture: {name} -> {target}"
                ));
            }
        }
    }
    Ok(())
}

fn command(root: &Path, program: &str, args: &[&str]) -> Result<(), String> {
    println!("> {program} {}", args.join(" "));
    let status = Command::new(program)
        .current_dir(root)
        .args(args)
        .status()
        .map_err(|e| format!("Could not run {program}: {e}"))?;
    if !status.success() {
        return Err(format!("{program} {} ({status})", args.join(" ")));
    }
    println!("[OK] {program} {}", args.join(" "));
    Ok(())
}

fn compose(root: &Path, project: &str, args: &[&str]) -> Result<(), String> {
    let mut all = vec!["compose", "-p", project, "-f", "docker-compose.test.yml"];
    all.extend_from_slice(args);
    command(root, "docker", &all)
}

fn live(root: &Path) -> Result<(), String> {
    // Check access before creating any resources. Each run owns its own project.
    command(root, "docker", &["info", "--format", "{{.ServerVersion}}"])?;
    let project = format!("jykre-gate-{}", std::process::id());
    let result = (|| {
        compose(
            root,
            &project,
            &["up", "-d", "--wait", "--wait-timeout", "180"],
        )?;
        for (driver, port) in [
            ("postgres", "55424"),
            ("postgres", "55428"),
            ("mysql", "55394"),
            ("mysql", "55399"),
        ] {
            let package = format!("jykre-driver-{driver}");
            println!("> Integration {package} on localhost:{port}");
            let status = Command::new("cargo")
                .current_dir(root)
                .args([
                    "test",
                    "-p",
                    &package,
                    "--locked",
                    "--test",
                    "live",
                    "--",
                    "--ignored",
                ])
                .env("JYKRE_TEST_PORT", port)
                .status()
                .map_err(|e| e.to_string())?;
            if !status.success() {
                return Err(format!("Integration {driver}:{port} failed"));
            }
            println!("[OK] Integration {driver}:{port}");
        }
        Ok(())
    })();
    // Cleanup also runs when startup or a test fails; never remove another project's volumes.
    let cleanup = compose(root, &project, &["down", "--volumes"]);
    match (result, cleanup) {
        (Err(error), Err(cleanup)) => Err(format!("{error}; cleanup also failed: {cleanup}")),
        (Err(error), _) | (_, Err(error)) => Err(error),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn rejects_invalid_dependency_edges_in_every_dependency_kind() {
        for (name, target) in [
            ("jykre-core", "gpui-kit"),
            ("jykre-core", "sqlx"),
            ("jykre-core", "xtask"),
            ("jykre-core", "jykre-driver-mysql"),
            ("jykre-driver-postgres", "jykre"),
            ("jykre-driver-postgres", "jykre-driver-mysql"),
            ("jykre-storage", "jykre"),
            ("jykre-storage", "gpui-kit"),
            ("jykre-storage", "jykre-driver-postgres"),
            ("jykre-secrets", "jykre-storage"),
            ("jykre-core", "jykre-storage"),
        ] {
            for kind in [Value::Null, json!("dev"), json!("build")] {
                assert!(architecture(&json!({"packages":[{"name":name,"dependencies":[{"name":target,"kind":kind}]}]})).is_err());
            }
        }
    }

    #[test]
    fn allows_driver_contracts_and_app_composition() {
        assert!(architecture(&json!({"packages":[
            {"name":"jykre-driver-postgres","dependencies":[{"name":"jykre-core"},{"name":"sqlx"}]},
            {"name":"jykre","dependencies":[{"name":"gpui-kit"},{"name":"jykre-driver-postgres"},{"name":"jykre-storage"},{"name":"jykre-secrets"}]},
            {"name":"jykre-storage","dependencies":[{"name":"jykre-core"},{"name":"sqlx"}]},
            {"name":"jykre-secrets","dependencies":[{"name":"jykre-core"},{"name":"keyring"}]}
        ]})).is_ok());
    }

    #[test]
    fn rejects_flags_that_could_silently_skip_required_checks() {
        assert!(options(&["check".into(), "--ful".into()]).is_err());
        assert_eq!(
            options(&["check".into(), "--full".into()]),
            Ok(Task::Check { full: true })
        );
        assert_eq!(
            options(&["i18n".into(), "pt-BR".into()]),
            Ok(Task::Catalog("pt-BR".into()))
        );
    }

    #[test]
    fn extracts_translatable_messages_only_from_tr_calls() {
        let source = r#"
            label(tr!("Run query"));
            tr!(
                "Query {0}: \"{1}\"\n",
                index, title
            );
            format!("not {}", 1);
            include_str!("x.json");
            tr!(name);
        "#;
        assert_eq!(messages(source), ["Run query", "Query {0}: \"{1}\"\n"]);
    }

    #[test]
    fn rejects_invalid_or_source_locales() {
        let root = Path::new(".");
        for tag in ["", "en", "EN", "../x", "pt_BR", "a--b"] {
            assert!(catalog(root, tag).is_err(), "{tag}");
        }
    }
}
