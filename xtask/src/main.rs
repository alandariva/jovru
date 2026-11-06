// xtask is a command-line tool: printing its progress is its job (ADR-0011).
#![allow(clippy::print_stdout, clippy::print_stderr)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]
mod catalogs;

use serde_json::{Map, Value};
use std::{
    collections::BTreeSet,
    env,
    ffi::OsStr,
    fs,
    path::Path,
    process::{Command, ExitCode},
    time::{SystemTime, UNIX_EPOCH},
};

#[derive(Debug, PartialEq)]
enum Task {
    Check { full: bool },
    Catalog(String),
    Catalogs(catalogs::Job),
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
        Ok(Task::Catalogs(job)) => match catalogs::run(root, job) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("SQL catalogs not updated: {error}");
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

/// Runs the gate with a run id and an application data folder of its own, so
/// concurrent runs (from separate worktrees) never share Docker resources or
/// the user's real data folder.
fn run(root: &Path, full: bool) -> Result<(), String> {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or_default();
    let id = run_id(std::process::id(), nanos);
    // Handed to the tests as JYKRE_CONFIG_DIR: a test that reaches the
    // application data folder by mistake writes here, and the gate fails.
    let data = env::temp_dir().join(format!("{id}-data"));
    fs::create_dir_all(&data).map_err(|e| format!("{}: {e}", data.display()))?;
    let result = gate(root, full, &id, &data);
    let cleanup = fs::remove_dir_all(&data).map_err(|e| format!("{}: {e}", data.display()));
    combine(result, cleanup)
}

fn gate(root: &Path, full: bool, id: &str, data: &Path) -> Result<(), String> {
    let isolated = [("JYKRE_CONFIG_DIR", data.as_os_str())];
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
    library_without_language_features(root)?;
    assistant_privacy(root)?;
    println!("[OK] Architecture (direct dependencies, including dev and build)");
    catalogs::check(&root.join("catalog"))?;
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
    command_env(
        root,
        "cargo",
        &["test", "--workspace", "--locked"],
        &isolated,
    )?;
    untouched(data)?;
    // The editor's language features also build, lint and pass without their
    // optional engines, which is what makes those engines replaceable.
    command(
        root,
        "cargo",
        &[
            "clippy",
            "-p",
            "jykre-sql-language",
            "--no-default-features",
            "--all-targets",
            "--locked",
            "--",
            "-D",
            "warnings",
        ],
    )?;
    command_env(
        root,
        "cargo",
        &[
            "test",
            "-p",
            "jykre-sql-language",
            "--no-default-features",
            "--locked",
        ],
        &isolated,
    )?;
    untouched(data)?;
    if full {
        live(root, id, data)?;
        untouched(data)?;
        // Quick mode only proves the benchmarks still run; comparing numbers
        // is done by hand against a saved baseline.
        command_env(
            root,
            "cargo",
            &["bench", "--locked", "--bench", "hot_paths", "--", "--quick"],
            &isolated,
        )?;
        untouched(data)?;
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
        ["catalogs", rest @ ..] => catalogs::options(rest).map(Task::Catalogs),
        _ => Err(format!(
            "Usage: cargo xtask check [--full] | cargo xtask i18n <locale> | {}. Offline: set CARGO_NET_OFFLINE=true.",
            catalogs::USAGE
        )),
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
            // The assistant crate talks to AI providers (ADR-0028) the way a
            // driver talks to a database: it knows the core contracts only,
            // never the UI, the cache or the vault.
            let adapter = name.starts_with("jykre-driver-")
                || name == "jykre-storage"
                || name == "jykre-secrets"
                || name == "jykre-assistant";
            let backend = name == "jykre-core" || adapter;
            let internal = target == "jykre" || target == "xtask" || target.starts_with("jykre-");
            let ui = target.starts_with("gpui") || target == "tauri";
            // The SQL crates are pure text handling (ADR-0020): `jykre-sql`
            // serves query execution and must stay free of the editor's
            // engines; `jykre-sql-language` holds the editor's features and
            // knows neither the UI nor the editor's LSP types.
            let sql = name == "jykre-sql";
            let language = name == "jykre-sql-language";
            let editor_engine = target == "sqruff-lib" || target.starts_with("tree-sitter");
            let forbidden = (backend && ui)
                || (name == "jykre-core"
                    && (internal || matches!(target, "sqlx" | "reqwest" | "genai")))
                || (adapter && internal && target != "jykre-core")
                || ((sql || language) && (ui || target == "lsp-types"))
                || (sql && ((internal && target != "jykre-core") || editor_engine))
                || (language && internal && !matches!(target, "jykre-core" | "jykre-sql"));
            if forbidden {
                return Err(format!(
                    "Dependency forbidden by the architecture: {name} -> {target}"
                ));
            }
        }
    }
    Ok(())
}

/// The `jykre` library (everything in `src/` but the application in
/// `src/main.rs` and `src/app/`) runs queries and reads metadata; it never
/// uses the editor's language features, so they can be removed without
/// touching it (ADR-0020).
fn library_without_language_features(root: &Path) -> Result<(), String> {
    let src = root.join("src");
    let mut pending = vec![src.clone()];
    while let Some(dir) = pending.pop() {
        for entry in fs::read_dir(&dir).map_err(|e| format!("{}: {e}", dir.display()))? {
            let path = entry.map_err(|e| e.to_string())?.path();
            if path == src.join("app") || path == src.join("main.rs") {
                continue;
            }
            if path.is_dir() {
                pending.push(path);
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                let source =
                    fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
                if source.contains("jykre_sql_language") {
                    return Err(format!(
                        "{} uses the editor's language features (jykre_sql_language); only src/app may",
                        path.display()
                    ));
                }
            }
        }
    }
    Ok(())
}

/// What the AI assistant's code may not touch (ADR-0028): result values,
/// bound parameter values, sessions, credentials and server reads. Its
/// requests are built from the allow-list in `src/assistant/facts.rs` and
/// the metadata cache only. Test files may build fixtures with them.
const ASSISTANT_FORBIDDEN: [&str; 9] = [
    "ResultData",
    ".parameters",
    "inspect_object",
    "object_definition",
    "ResolvedConnection",
    "Credentials",
    "backend::query",
    "backend::fetch",
    "held_pages",
];

fn assistant_privacy(root: &Path) -> Result<(), String> {
    let mut pending = vec![root.join("src").join("assistant")];
    let mut files = Vec::new();
    for single in ["assistant_panel.rs", "assistant_settings.rs"] {
        let path = root.join("src").join("app").join(single);
        if path.exists() {
            files.push(path);
        }
    }
    while let Some(dir) = pending.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries {
            let path = entry.map_err(|e| e.to_string())?.path();
            if path.is_dir() {
                pending.push(path);
            } else if path.extension().is_some_and(|ext| ext == "rs")
                && path.file_name().is_some_and(|name| name != "tests.rs")
            {
                files.push(path);
            }
        }
    }
    for path in files {
        let source = fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        if let Some(name) = ASSISTANT_FORBIDDEN
            .iter()
            .find(|name| source.contains(**name))
        {
            return Err(format!(
                "{} uses {name}; the assistant may read only the facts allow-list and the metadata cache",
                path.display()
            ));
        }
    }
    Ok(())
}

fn command(root: &Path, program: &str, args: &[&str]) -> Result<(), String> {
    command_env(root, program, args, &[])
}

fn command_env(
    root: &Path,
    program: &str,
    args: &[&str],
    vars: &[(&str, &OsStr)],
) -> Result<(), String> {
    println!("> {program} {}", args.join(" "));
    let status = Command::new(program)
        .current_dir(root)
        .args(args)
        .envs(vars.iter().copied())
        .status()
        .map_err(|e| format!("Could not run {program}: {e}"))?;
    if !status.success() {
        return Err(format!("{program} {} ({status})", args.join(" ")));
    }
    println!("[OK] {program} {}", args.join(" "));
    Ok(())
}

fn compose_args<'a>(project: &'a str, args: &[&'a str]) -> Vec<&'a str> {
    let mut all = vec!["compose", "-p", project, "-f", "docker-compose.test.yml"];
    all.extend_from_slice(args);
    all
}

fn compose(root: &Path, project: &str, args: &[&str]) -> Result<(), String> {
    command(root, "docker", &compose_args(project, args))
}

/// Runs a Compose command and returns its standard output.
fn compose_output(root: &Path, project: &str, args: &[&str]) -> Result<String, String> {
    let all = compose_args(project, args);
    println!("> docker {}", all.join(" "));
    let output = Command::new("docker")
        .current_dir(root)
        .args(&all)
        .output()
        .map_err(|e| format!("Could not run docker: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "docker {} ({}): {}",
            all.join(" "),
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    String::from_utf8(output.stdout).map_err(|e| format!("docker {}: {e}", all.join(" ")))
}

/// A name no other run uses at the same time, valid as a Compose project.
/// The time suffix keeps it unique where PIDs can repeat, such as Windows and
/// WSL sharing one Docker daemon, or a PID reused after an interrupted run.
fn run_id(pid: u32, nanos: u128) -> String {
    format!("jykre-gate-{pid}-{nanos:x}")
}

/// The host port in the output of `docker compose port`, such as
/// `127.0.0.1:49153`.
fn published_port(output: &str) -> Result<u16, String> {
    let address = output.lines().next().unwrap_or_default().trim();
    address
        .rsplit_once(':')
        .and_then(|(_, port)| port.parse::<u16>().ok())
        .filter(|port| *port != 0)
        .ok_or_else(|| format!("Unexpected published address {address:?}"))
}

/// Fails when anything was written to the data folder handed to the tests.
fn untouched(dir: &Path) -> Result<(), String> {
    let mut written = fs::read_dir(dir)
        .map_err(|e| format!("{}: {e}", dir.display()))?
        .map(|entry| {
            entry
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .map_err(|e| format!("{}: {e}", dir.display()))
        })
        .collect::<Result<Vec<_>, _>>()?;
    if written.is_empty() {
        return Ok(());
    }
    written.sort();
    Err(format!(
        "A test wrote to the application data folder ({}: {}); tests must use their own temporary folder",
        dir.display(),
        written.join(", ")
    ))
}

/// Reports the main error, and the cleanup error when both failed.
fn combine(result: Result<(), String>, cleanup: Result<(), String>) -> Result<(), String> {
    match (result, cleanup) {
        (Err(error), Err(cleanup)) => Err(format!("{error}; cleanup also failed: {cleanup}")),
        (Err(error), _) | (_, Err(error)) => Err(error),
        _ => Ok(()),
    }
}

/// Compose services in `docker-compose.test.yml`, their driver and the port
/// they listen on inside the container.
const FIXTURES: [(&str, &str, &str); 4] = [
    ("postgres-min", "postgres", "5432"),
    ("postgres-max", "postgres", "5432"),
    ("mysql-lts", "mysql", "3306"),
    ("mysql-current", "mysql", "3306"),
];

fn live(root: &Path, project: &str, data: &Path) -> Result<(), String> {
    // Check access before creating any resources. Each run owns its own
    // project, and Docker picks a free host port for every service.
    command(root, "docker", &["info", "--format", "{{.ServerVersion}}"])?;
    let result = (|| {
        compose(
            root,
            project,
            &["up", "-d", "--wait", "--wait-timeout", "180"],
        )?;
        for (service, driver, container_port) in FIXTURES {
            let output = compose_output(root, project, &["port", service, container_port])?;
            let port = published_port(&output)?.to_string();
            let package = format!("jykre-driver-{driver}");
            println!("> Integration {package} ({service}) on 127.0.0.1:{port}");
            command_env(
                root,
                "cargo",
                &[
                    "test",
                    "-p",
                    &package,
                    "--locked",
                    "--test",
                    "live",
                    "--",
                    "--ignored",
                ],
                &[
                    ("JYKRE_TEST_PORT", OsStr::new(&port)),
                    ("JYKRE_CONFIG_DIR", data.as_os_str()),
                ],
            )
            .map_err(|_| format!("Integration {service} failed"))?;
            // The export reads real sessions through the application's
            // backend, which no driver crate may depend on.
            println!("> Integration jykre export ({service}) on 127.0.0.1:{port}");
            command_env(
                root,
                "cargo",
                &[
                    "test",
                    "-p",
                    "jykre",
                    "--locked",
                    "--test",
                    "live",
                    "--",
                    "--ignored",
                ],
                &[
                    ("JYKRE_TEST_PORT", OsStr::new(&port)),
                    ("JYKRE_TEST_DRIVER", OsStr::new(driver)),
                    ("JYKRE_CONFIG_DIR", data.as_os_str()),
                ],
            )
            .map_err(|_| format!("Integration jykre export ({service}) failed"))?;
            println!("[OK] Integration {service}");
        }
        Ok(())
    })();
    // Cleanup also runs when startup or a test fails; never remove another project's volumes.
    let cleanup = compose(root, project, &["down", "--volumes"]);
    combine(result, cleanup)
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
            ("jykre-sql", "jykre"),
            ("jykre-sql", "jykre-sql-language"),
            ("jykre-sql", "jykre-storage"),
            ("jykre-sql", "sqruff-lib"),
            ("jykre-sql", "tree-sitter"),
            ("jykre-sql", "gpui-kit"),
            ("jykre-sql-language", "jykre"),
            ("jykre-sql-language", "jykre-storage"),
            ("jykre-sql-language", "jykre-driver-postgres"),
            ("jykre-sql-language", "gpui-kit"),
            ("jykre-sql-language", "lsp-types"),
            ("jykre-core", "genai"),
            ("jykre-assistant", "gpui-kit"),
            ("jykre-assistant", "jykre"),
            ("jykre-assistant", "jykre-storage"),
            ("jykre-assistant", "jykre-secrets"),
            ("jykre-assistant", "jykre-sql-language"),
            ("jykre-assistant", "jykre-driver-postgres"),
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
            {"name":"jykre-secrets","dependencies":[{"name":"jykre-core"},{"name":"keyring"}]},
            {"name":"jykre-sql","dependencies":[{"name":"jykre-core"}]},
            {"name":"jykre-sql-language","dependencies":[{"name":"jykre-core"},{"name":"jykre-sql"},{"name":"sqruff-lib"}]},
            {"name":"jykre-assistant","dependencies":[{"name":"jykre-core"},{"name":"genai"},{"name":"reqwest"}]},
            {"name":"jykre","dependencies":[{"name":"jykre-assistant"}]}
        ]})).is_ok());
    }

    #[test]
    fn the_library_never_uses_the_editor_language_features() {
        let root = env::temp_dir().join(format!("xtask-library-{}", std::process::id()));
        let write = |path: &str, text: &str| {
            let path = root.join(path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, text).unwrap();
        };
        write("src/lib.rs", "pub mod query;");
        write("src/query.rs", "pub fn run() {}");
        // The application may use them.
        write(
            "src/app/sql_editor/mod.rs",
            "use jykre_sql_language::format;",
        );
        write("src/main.rs", "use jykre_sql_language as _;");
        assert!(library_without_language_features(&root).is_ok());
        write(
            "src/backend/names.rs",
            "use jykre_sql_language::completion;",
        );
        let refused = library_without_language_features(&root);
        fs::remove_dir_all(&root).unwrap();
        assert!(refused.is_err_and(|error| error.contains("names.rs")));
    }

    #[test]
    fn the_assistant_never_reads_values_sessions_or_credentials() {
        let root = env::temp_dir().join(format!("xtask-assistant-{}", std::process::id()));
        let write = |path: &str, text: &str| {
            let path = root.join(path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, text).unwrap();
        };
        write("src/assistant/mod.rs", "pub mod facts;");
        write(
            "src/assistant/facts.rs",
            "pub fn rows(tab: &ResultTab) -> usize { tab.rows_read() }",
        );
        // Fixtures in tests may build results.
        write(
            "src/assistant/tests.rs",
            "let data = ResultData::from_rows(columns, rows);",
        );
        assert!(assistant_privacy(&root).is_ok());
        write(
            "src/app/assistant_panel.rs",
            "let values = tab.parameters.clone();",
        );
        let refused = assistant_privacy(&root);
        fs::remove_dir_all(&root).unwrap();
        assert!(refused.is_err_and(|error| error.contains("assistant_panel.rs")));
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
    fn reads_the_host_port_docker_published() {
        assert_eq!(published_port("127.0.0.1:49153\n"), Ok(49153));
        assert_eq!(published_port("[::1]:5\r\n0.0.0.0:6\n"), Ok(5));
        for output in [
            "",
            "\n",
            "abc",
            "127.0.0.1:",
            "127.0.0.1:0",
            "127.0.0.1:70000",
        ] {
            assert!(published_port(output).is_err(), "{output:?}");
        }
    }

    #[test]
    fn run_ids_are_valid_compose_projects_and_differ_between_runs() {
        let id = run_id(4242, 0x1a2b);
        assert_eq!(id, "jykre-gate-4242-1a2b");
        assert!(
            id.chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        );
        // The same PID in another namespace or reused later gets another id.
        assert_ne!(run_id(4242, 1), run_id(4242, 2));
    }

    #[test]
    fn fails_when_a_test_writes_to_the_application_data_folder() {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = env::temp_dir().join(format!("{}-xtask-test", run_id(std::process::id(), nanos)));
        fs::create_dir_all(&dir).unwrap();
        assert_eq!(untouched(&dir), Ok(()));
        fs::write(dir.join("config.db"), b"").unwrap();
        let error = untouched(&dir).unwrap_err();
        fs::remove_dir_all(&dir).unwrap();
        assert!(error.contains("config.db"), "{error}");
        assert!(
            untouched(&dir).is_err(),
            "a missing folder is not untouched"
        );
    }

    #[test]
    fn keeps_the_main_error_and_reports_a_failed_cleanup() {
        assert_eq!(combine(Ok(()), Ok(())), Ok(()));
        assert_eq!(combine(Err("a".into()), Ok(())), Err("a".into()));
        assert_eq!(combine(Ok(()), Err("b".into())), Err("b".into()));
        assert_eq!(
            combine(Err("a".into()), Err("b".into())),
            Err("a; cleanup also failed: b".into())
        );
    }

    #[test]
    fn rejects_invalid_or_source_locales() {
        let root = Path::new(".");
        for tag in ["", "en", "EN", "../x", "pt_BR", "a--b"] {
            assert!(catalog(root, tag).is_err(), "{tag}");
        }
    }
}
