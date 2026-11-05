//! `cargo xtask catalogs`: generates the SQL dialect catalogs in `catalog/`
//! and checks them against the format of
//! `docs/especificacao-catalogo-sql.md`.
//!
//! The catalogs stand in for an external data repository: the application
//! reads them at run time from a directory, and nothing in it depends on how
//! they were made. Generating needs the network (PostgreSQL and Spark
//! sources) or Docker (MySQL); the check is offline and runs in the gate.
use serde_json::{Map, Value, json};
use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

pub const USAGE: &str = "cargo xtask catalogs check | catalogs postgresql <REL_14_0,REL_15_0,...> [--verified-through 18.6] | catalogs mysql | catalogs sqlserver | catalogs databricks <spark-tag> [--keywords <csv>] [--functions <csv>] [--observed YYYY-MM-DD]";

/// What `cargo xtask catalogs` was asked to do.
#[derive(Debug, PartialEq, Eq)]
pub enum Job {
    Check,
    Postgresql {
        tags: Vec<String>,
        verified_through: Option<String>,
    },
    Mysql,
    Sqlserver,
    Databricks {
        spark_tag: String,
        keywords: Option<PathBuf>,
        functions: Option<PathBuf>,
        observed: Option<String>,
    },
}

pub fn options(args: &[&str]) -> Result<Job, String> {
    let flag = |rest: &[&str], name: &str| -> Result<Option<String>, String> {
        match rest.iter().position(|arg| *arg == name) {
            Some(at) => rest
                .get(at + 1)
                .map(|value| Some((*value).to_owned()))
                .ok_or_else(|| format!("{name} needs a value")),
            None => Ok(None),
        }
    };
    let known = |rest: &[&str], flags: &[&str]| -> Result<(), String> {
        let mut at = 0;
        while at < rest.len() {
            if !flags.contains(&rest[at]) {
                return Err(format!("Unknown argument {:?}. Usage: {USAGE}", rest[at]));
            }
            at += 2;
        }
        Ok(())
    };
    match args {
        ["check"] => Ok(Job::Check),
        ["postgresql", tags, rest @ ..] => {
            known(rest, &["--verified-through"])?;
            Ok(Job::Postgresql {
                tags: tags.split(',').map(str::to_owned).collect(),
                verified_through: flag(rest, "--verified-through")?,
            })
        }
        ["mysql"] => Ok(Job::Mysql),
        ["sqlserver"] => Ok(Job::Sqlserver),
        ["databricks", tag, rest @ ..] => {
            known(rest, &["--keywords", "--functions", "--observed"])?;
            Ok(Job::Databricks {
                spark_tag: (*tag).to_owned(),
                keywords: flag(rest, "--keywords")?.map(PathBuf::from),
                functions: flag(rest, "--functions")?.map(PathBuf::from),
                observed: flag(rest, "--observed")?,
            })
        }
        _ => Err(format!("Usage: {USAGE}")),
    }
}

pub fn run(root: &Path, job: Job) -> Result<(), String> {
    let dir = root.join("catalog");
    let generated = match job {
        Job::Check => return check(&dir),
        Job::Postgresql {
            tags,
            verified_through,
        } => postgresql(root, &dir, &tags, verified_through).map(|()| "postgresql"),
        Job::Mysql => mysql(root, &dir).map(|()| "mysql"),
        Job::Sqlserver => sqlserver(root, &dir).map(|()| "sqlserver"),
        Job::Databricks {
            spark_tag,
            keywords,
            functions,
            observed,
        } => {
            databricks(root, &dir, &spark_tag, keywords, functions, observed).map(|()| "databricks")
        }
    }?;
    check_dialect(&dir.join(generated)).map_err(|problems| {
        problems.join(
            "
  ",
        )
    })?;
    println!("[OK] {generated} catalog checked");
    Ok(())
}

// --- Format ---------------------------------------------------------------

/// The dialect directories and the name of the key of each kind of entry.
const DIALECTS: [&str; 4] = ["postgresql", "mysql", "databricks", "sqlserver"];
const KINDS: [(&str, &str); 3] = [
    ("keywords.jsonl", "word"),
    ("functions.jsonl", "id"),
    ("types.jsonl", "name"),
];

/// `value` with the keys of every object in lexicographic order, so its
/// compact form is canonical (RFC 8785 for the strings, numbers and booleans
/// this format uses) whatever order the JSON library keeps.
fn sorted(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let keys: BTreeSet<&String> = map.keys().collect();
            let mut out = Map::new();
            for key in keys {
                out.insert(key.clone(), sorted(&map[key]));
            }
            Value::Object(out)
        }
        Value::Array(items) => Value::Array(items.iter().map(sorted).collect()),
        other => other.clone(),
    }
}

fn canonical(value: &Value) -> String {
    sorted(value).to_string()
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Scheme {
    Numeric,
    Date,
}

fn scheme(manifest: &Value) -> Result<Scheme, String> {
    match manifest["version_scheme"].as_str() {
        Some("numeric") => Ok(Scheme::Numeric),
        Some("date") => Ok(Scheme::Date),
        other => Err(format!("version_scheme {other:?}")),
    }
}

fn compare(scheme: Scheme, a: &str, b: &str) -> Option<Ordering> {
    match scheme {
        Scheme::Date => Some(a.cmp(b)),
        Scheme::Numeric => {
            let parse = |text: &str| {
                text.split('.')
                    .map(|part| part.parse::<u64>().ok())
                    .collect::<Option<Vec<_>>>()
            };
            let (a, b) = (parse(a)?, parse(b)?);
            let at = |parts: &[u64], index: usize| parts.get(index).copied().unwrap_or(0);
            Some(
                (0..a.len().max(b.len()))
                    .map(|index| at(&a, index).cmp(&at(&b, index)))
                    .find(|order| order.is_ne())
                    .unwrap_or(Ordering::Equal),
            )
        }
    }
}

/// Every generated version's entries, in version order and keyed, folded
/// into lines with `since`/`until` (specification §6.2): a new key opens a
/// line, a missing one closes it and a changed one closes it and opens
/// another. Returns the lines, sorted by key and `since`, and the versions
/// that changed something (the first always counts).
fn fold(key: &str, snapshots: &[(String, BTreeMap<String, Value>)]) -> (Vec<Value>, Vec<String>) {
    let mut open: BTreeMap<String, (Value, usize)> = BTreeMap::new();
    let mut closed: Vec<(String, usize, Value, usize)> = Vec::new();
    let mut changed = BTreeSet::from([0]);
    for (index, (_, entries)) in snapshots.iter().enumerate() {
        let keys: BTreeSet<String> = open.keys().chain(entries.keys()).cloned().collect();
        for name in keys {
            let current = open.get(&name).map(|(fields, _)| canonical(fields));
            let next = entries.get(&name);
            match (current, next) {
                (Some(current), Some(next)) if current == canonical(next) => {}
                (current, next) => {
                    if current.is_some()
                        && let Some((fields, since)) = open.remove(&name)
                    {
                        closed.push((name.clone(), since, fields, index));
                    }
                    if let Some(next) = next {
                        open.insert(name.clone(), (next.clone(), index));
                    }
                    if index > 0 {
                        changed.insert(index);
                    }
                }
            }
        }
    }
    let mut lines: Vec<(String, usize, Value, Option<usize>)> = closed
        .into_iter()
        .map(|(name, since, fields, until)| (name, since, fields, Some(until)))
        .collect();
    lines.extend(
        open.into_iter()
            .map(|(name, (fields, since))| (name, since, fields, None)),
    );
    lines.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
    let versions: Vec<String> = changed
        .iter()
        .map(|index| snapshots[*index].0.clone())
        .collect();
    let lines = lines
        .into_iter()
        .map(|(name, since, mut fields, until)| {
            if let Value::Object(map) = &mut fields {
                map.insert(key.to_owned(), Value::String(name));
                if since > 0 {
                    map.insert("since".into(), Value::String(snapshots[since].0.clone()));
                }
                if let Some(until) = until {
                    map.insert("until".into(), Value::String(snapshots[until].0.clone()));
                }
            }
            sorted(&fields)
        })
        .collect();
    (lines, versions)
}

/// Checks a catalog directory against the specification's rules (§9): JSON
/// per line in canonical form, sorted, required fields, known sources, text
/// only from redistributable sources, versions inside the manifest and no
/// overlapping validity for one key.
pub fn check(dir: &Path) -> Result<(), String> {
    let mut problems = Vec::new();
    for dialect in DIALECTS {
        if let Err(found) = check_dialect(&dir.join(dialect)) {
            problems.extend(
                found
                    .into_iter()
                    .map(|problem| format!("{dialect}: {problem}")),
            );
        }
    }
    if problems.is_empty() {
        println!("[OK] SQL catalogs ({})", dir.display());
        Ok(())
    } else {
        problems.truncate(20);
        Err(format!("SQL catalogs:\n  {}", problems.join("\n  ")))
    }
}

fn check_dialect(dir: &Path) -> Result<(), Vec<String>> {
    let read =
        |file: &str| fs::read_to_string(dir.join(file)).map_err(|e| vec![format!("{file}: {e}")]);
    let manifest: Value = serde_json::from_str(&read("manifest.json")?)
        .map_err(|e| vec![format!("manifest.json: {e}")])?;
    let mut problems = Vec::new();
    if manifest["schema_version"] != json!(1) {
        problems.push("manifest.json: schema_version is not 1".to_owned());
    }
    let scheme = scheme(&manifest).map_err(|e| vec![format!("manifest.json: {e}")])?;
    let versions: Vec<String> = manifest["versions"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|version| version.as_str().map(str::to_owned))
        .collect();
    if versions.is_empty() {
        problems.push("manifest.json: no versions".into());
    }
    if versions
        .windows(2)
        .any(|pair| compare(scheme, &pair[0], &pair[1]) != Some(Ordering::Less))
    {
        problems.push("manifest.json: versions are not ascending".into());
    }
    let verified = manifest["verified_through"].as_str().unwrap_or_default();
    if versions
        .last()
        .is_some_and(|last| compare(scheme, last, verified).is_none_or(Ordering::is_gt))
    {
        problems.push("manifest.json: verified_through is before the last version".into());
    }
    let mut sources = BTreeMap::new();
    for source in manifest["sources"].as_array().into_iter().flatten() {
        let (Some(id), Some(_), Some(redistributable)) = (
            source["id"].as_str(),
            source["license"].as_str(),
            source["redistributable"].as_bool(),
        ) else {
            problems.push(format!("manifest.json: incomplete source {source}"));
            continue;
        };
        if sources.insert(id.to_owned(), redistributable).is_some() {
            problems.push(format!("manifest.json: source {id} twice"));
        }
    }
    let position = |version: &str| versions.iter().position(|known| known == version);
    for (file, key) in KINDS {
        let text = read(file)?;
        let mut previous: Option<(String, usize, Option<usize>)> = None;
        for (index, line) in text.lines().enumerate() {
            let at = format!("{file}:{}", index + 1);
            let Ok(value) = serde_json::from_str::<Value>(line) else {
                problems.push(format!("{at}: not JSON"));
                continue;
            };
            if canonical(&value) != line {
                problems.push(format!("{at}: not in canonical form"));
            }
            let required: &[&str] = match key {
                "word" => &["word", "source"],
                "id" => &["id", "name", "kind", "signature", "source"],
                _ => &["name", "display", "source"],
            };
            for field in required {
                if !value[field].is_string() {
                    problems.push(format!("{at}: {field} missing"));
                }
            }
            if key == "word" && !value["reserved"].is_boolean() {
                problems.push(format!("{at}: reserved missing"));
            }
            if value["source"]
                .as_str()
                .is_some_and(|source| !sources.contains_key(source))
            {
                problems.push(format!("{at}: unknown source"));
            }
            if value.get("summary").is_some() {
                match value["summary_source"].as_str().map(|id| sources.get(id)) {
                    Some(Some(true)) => {}
                    _ => problems.push(format!(
                        "{at}: summary without a redistributable summary_source"
                    )),
                }
            }
            let since = match value.get("since") {
                Some(since) => match since.as_str().and_then(position) {
                    Some(found) => found,
                    None => {
                        problems.push(format!("{at}: since is not a tracked version"));
                        continue;
                    }
                },
                None => 0,
            };
            let until = match value.get("until") {
                Some(until) => match until.as_str().and_then(position) {
                    Some(found) if found > since => Some(found),
                    _ => {
                        problems.push(format!("{at}: until is not a tracked version after since"));
                        continue;
                    }
                },
                None => None,
            };
            let name = value[key].as_str().unwrap_or_default().to_owned();
            if let Some((last, last_since, last_until)) = &previous {
                match name.cmp(last) {
                    Ordering::Less => problems.push(format!("{at}: not sorted by {key}")),
                    Ordering::Equal if since < *last_since => {
                        problems.push(format!("{at}: not sorted by since"))
                    }
                    Ordering::Equal if last_until.is_none_or(|end| end > since) => {
                        problems.push(format!("{at}: validity overlaps the line before"))
                    }
                    _ => {}
                }
            }
            previous = Some((name, since, until));
        }
    }
    if problems.is_empty() {
        Ok(())
    } else {
        Err(problems)
    }
}

/// Writes a dialect's files: the folded lines of each kind and its manifest.
/// The manifest's `updated_at` changes only when the data does.
fn write_dialect(dir: &Path, mut manifest: Value, kinds: [Vec<Value>; 3]) -> Result<(), String> {
    fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let mut changed = false;
    for ((file, _), lines) in KINDS.iter().zip(kinds) {
        let text: String = lines
            .iter()
            .map(|line| format!("{}\n", canonical(line)))
            .collect();
        let path = dir.join(file);
        changed |= fs::read_to_string(&path).ok().as_deref() != Some(text.as_str());
        fs::write(&path, text).map_err(|e| format!("{}: {e}", path.display()))?;
    }
    let path = dir.join("manifest.json");
    let previous: Option<Value> = fs::read_to_string(&path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok());
    let without_date = |value: &Value| {
        let mut value = value.clone();
        if let Value::Object(map) = &mut value {
            map.remove("updated_at");
        }
        canonical(&value)
    };
    let kept = previous
        .as_ref()
        .filter(|previous| !changed && without_date(previous) == without_date(&manifest))
        .and_then(|previous| previous["updated_at"].as_str().map(str::to_owned));
    if let Value::Object(map) = &mut manifest {
        map.insert(
            "updated_at".into(),
            Value::String(kept.unwrap_or_else(today)),
        );
    }
    let text = serde_json::to_string_pretty(&sorted(&manifest)).map_err(|e| e.to_string())? + "\n";
    fs::write(&path, text).map_err(|e| format!("{}: {e}", path.display()))?;
    println!("[OK] {}", dir.display());
    Ok(())
}

/// Today's date (UTC) as `YYYY-MM-DD`.
fn today() -> String {
    let days = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs() / 86_400) as i64;
    civil_date(days)
}

/// The proleptic Gregorian date `days` after 1970-01-01.
fn civil_date(days: i64) -> String {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let mp = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}")
}

/// Downloads `url` to `path` unless it is already there, so a second run
/// works offline.
fn download(url: &str, path: &Path) -> Result<(), String> {
    if path.exists() {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    }
    println!("> curl {url}");
    let status = Command::new("curl")
        .args(["-fsSL", "--retry", "2", "-o"])
        .arg(path)
        .arg(url)
        .status()
        .map_err(|e| format!("Could not run curl: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        let _ = fs::remove_file(path);
        Err(format!("curl {url} ({status})"))
    }
}

fn read(path: &Path) -> Result<String, String> {
    fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))
}

fn identifier(name: &str) -> bool {
    name.chars()
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic() || first == '_')
        && name
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
}

fn source(id: &str, title: &str, url: &str, license: &str, redistributable: bool) -> Value {
    json!({"id": id, "title": title, "url": url, "license": license, "redistributable": redistributable})
}

fn curated() -> Value {
    source(
        "curated",
        "Written by the project",
        "https://github.com/alandariva/jykre-studio",
        "CC0-1.0",
        true,
    )
}

fn files() -> Value {
    json!({"keywords": "keywords.jsonl", "functions": "functions.jsonl", "types": "types.jsonl"})
}

fn generator() -> Value {
    json!({"name": "cargo xtask catalogs", "version": "1"})
}

/// A curated type line: its name, how SQL spells it, aliases and parameters.
fn curated_type(
    name: &str,
    display: &str,
    aliases: &[&str],
    params: Option<&str>,
) -> (String, Value) {
    let mut line = json!({"display": display, "source": "curated"});
    if !aliases.is_empty() {
        line["aliases"] = json!(aliases);
    }
    if let Some(params) = params {
        line["params"] = json!(params);
    }
    (name.to_owned(), line)
}

// --- PostgreSQL ------------------------------------------------------------

/// Reads PostgreSQL's catalog data files (`*.dat`): a Perl-like list of
/// `{ key => 'value', ... }` entries with `#` comments.
fn dat_entries(text: &str) -> Result<Vec<BTreeMap<String, String>>, String> {
    let bytes = text.as_bytes();
    let mut entries = Vec::new();
    let mut entry: Option<BTreeMap<String, String>> = None;
    let mut key: Option<String> = None;
    let mut at = 0;
    while at < bytes.len() {
        match bytes[at] {
            b'#' => {
                while at < bytes.len() && bytes[at] != b'\n' {
                    at += 1;
                }
            }
            b'{' => {
                entry = Some(BTreeMap::new());
                at += 1;
            }
            b'}' => {
                entries.push(entry.take().ok_or("unbalanced }")?);
                at += 1;
            }
            b'\'' => {
                let mut value = String::new();
                at += 1;
                loop {
                    match bytes.get(at) {
                        None => return Err("unterminated string".into()),
                        Some(b'\\') => {
                            if let Some(next) = text[at + 1..].chars().next() {
                                value.push(next);
                                at += 1 + next.len_utf8();
                            } else {
                                at += 1;
                            }
                        }
                        Some(b'\'') => {
                            at += 1;
                            break;
                        }
                        Some(_) => {
                            let ch = text[at..].chars().next().ok_or("bad text")?;
                            value.push(ch);
                            at += ch.len_utf8();
                        }
                    }
                }
                let entry = entry.as_mut().ok_or("value outside an entry")?;
                entry.insert(key.take().ok_or("value without a key")?, value);
            }
            byte if byte.is_ascii_alphabetic() || byte == b'_' => {
                let start = at;
                while at < bytes.len() && (bytes[at].is_ascii_alphanumeric() || bytes[at] == b'_') {
                    at += 1;
                }
                key = Some(text[start..at].to_owned());
            }
            _ => at += 1,
        }
    }
    Ok(entries)
}

/// The keywords of PostgreSQL's `kwlist.h`.
fn kwlist(text: &str) -> BTreeMap<String, Value> {
    let mut found = BTreeMap::new();
    for line in text.lines() {
        let Some(rest) = line.strip_prefix("PG_KEYWORD(\"") else {
            continue;
        };
        let Some((word, rest)) = rest.split_once('"') else {
            continue;
        };
        let parts: Vec<&str> = rest
            .trim_start_matches(',')
            .trim_end_matches(')')
            .split(',')
            .map(str::trim)
            .collect();
        let [_, category, label] = parts.as_slice() else {
            continue;
        };
        let category = match *category {
            "UNRESERVED_KEYWORD" => "unreserved",
            "COL_NAME_KEYWORD" => "col_name",
            "TYPE_FUNC_NAME_KEYWORD" => "type_func_name",
            "RESERVED_KEYWORD" => "reserved",
            _ => continue,
        };
        let mut entry = json!({
            "category": category,
            "reserved": matches!(category, "reserved" | "type_func_name"),
            "source": "kwlist.h",
        });
        if *label == "BARE_LABEL" {
            entry["bare_label"] = json!(true);
        }
        found.insert(word.to_owned(), entry);
    }
    found
}

/// How SQL spells PostgreSQL's built-in type names, with the parameters it
/// takes.
fn pg_display(typname: &str) -> (&str, Option<&'static str>) {
    match typname {
        "int2" => ("smallint", None),
        "int4" => ("integer", None),
        "int8" => ("bigint", None),
        "float4" => ("real", None),
        "float8" => ("double precision", None),
        "bool" => ("boolean", None),
        "varchar" => ("character varying", Some("(n)")),
        "bpchar" => ("character", Some("(n)")),
        "timestamptz" => ("timestamp with time zone", Some("(precision)")),
        "timestamp" => ("timestamp without time zone", Some("(precision)")),
        "timetz" => ("time with time zone", Some("(precision)")),
        "time" => ("time without time zone", Some("(precision)")),
        "interval" => ("interval", Some("(precision)")),
        "numeric" => ("numeric", Some("(precision, scale)")),
        "bit" => ("bit", Some("(n)")),
        "varbit" => ("bit varying", Some("(n)")),
        other => (other, None),
    }
}

fn pg_type_text(typname: &str) -> String {
    match typname.strip_prefix('_') {
        Some(element) => format!("{}[]", pg_display(element).0),
        None => pg_display(typname).0.to_owned(),
    }
}

/// Pseudo-types only the server's internals pass around.
const PG_INTERNAL_TYPES: &[&str] = &[
    "internal",
    "cstring",
    "trigger",
    "event_trigger",
    "language_handler",
    "fdw_handler",
    "index_am_handler",
    "tsm_handler",
    "table_am_handler",
];

/// Descriptions of support functions, which nobody calls in a query.
const PG_SUPPORT: &[&str] = &[
    "I/O",
    "aggregate transition function",
    "aggregate final function",
    "aggregate combine function",
    "aggregate serial function",
    "aggregate deserial function",
    "planner support",
    "less-equal-greater",
    "sort support",
    "hash",
    "restriction selectivity",
    "join selectivity",
];

/// The functions a PostgreSQL user can call, from `pg_proc.dat`. Every entry
/// there has a description except those that implement operators, which are
/// left out with the support functions (type I/O, aggregate steps, planner
/// and index support) and those taking internal pseudo-types.
fn pg_functions(entries: &[BTreeMap<String, String>]) -> BTreeMap<String, Value> {
    let mut found = BTreeMap::new();
    for entry in entries {
        let (Some(name), Some(descr)) = (entry.get("proname"), entry.get("descr")) else {
            continue;
        };
        if PG_SUPPORT.iter().any(|support| descr.starts_with(support)) {
            continue;
        }
        let types: Vec<&str> = entry
            .get("proargtypes")
            .map(|types| types.split_whitespace().collect())
            .unwrap_or_default();
        let returns = entry.get("prorettype").map_or("void", String::as_str);
        if types
            .iter()
            .chain(std::iter::once(&returns))
            .any(|ty| PG_INTERNAL_TYPES.contains(ty))
        {
            continue;
        }
        // Argument names only line up with the types when no OUT arguments
        // are declared (`proallargtypes`).
        let names: Vec<&str> = match (entry.get("proargnames"), entry.get("proallargtypes")) {
            (Some(names), None) => names
                .trim_matches(['{', '}'])
                .split(',')
                .map(str::trim)
                .collect(),
            _ => Vec::new(),
        };
        let variadic = entry.get("provariadic").is_some_and(|ty| ty != "0");
        let set = entry.get("proretset").is_some_and(|set| set == "t");
        let kind = match entry.get("prokind").map(String::as_str) {
            Some("a") => "aggregate",
            Some("w") => "window",
            Some("p") => "procedure",
            _ if set => "table",
            _ => "scalar",
        };
        let args: Vec<Value> = types
            .iter()
            .enumerate()
            .map(|(index, ty)| {
                let mut arg = json!({"type": ty});
                if let Some(name) = names.get(index).filter(|name| !name.is_empty()) {
                    arg["name"] = json!(name);
                }
                if variadic && index + 1 == types.len() {
                    arg["variadic"] = json!(true);
                }
                arg
            })
            .collect();
        let shown: Vec<String> = types
            .iter()
            .enumerate()
            .map(|(index, ty)| {
                let ty = pg_type_text(ty);
                let named = match names.get(index).filter(|name| !name.is_empty()) {
                    Some(name) => format!("{name} {ty}"),
                    None => ty,
                };
                if variadic && index + 1 == types.len() {
                    format!("VARIADIC {named}")
                } else {
                    named
                }
            })
            .collect();
        let result = if kind == "procedure" {
            String::new()
        } else if set {
            format!(" → setof {}", pg_type_text(returns))
        } else {
            format!(" → {}", pg_type_text(returns))
        };
        let id = format!("{name}({})", types.join(","));
        let mut line = json!({
            "name": name,
            "kind": kind,
            "args": args,
            "signature": format!("{name}({}){result}", shown.join(", ")),
            "source": "pg_proc.dat",
            "summary": descr,
            "summary_source": "pg_proc.dat",
        });
        if kind != "procedure" {
            line["returns"] = json!(returns);
        }
        if set {
            line["returns_set"] = json!(true);
        }
        if args.is_empty()
            && let Value::Object(map) = &mut line
        {
            map.remove("args");
        }
        found.entry(id).or_insert(line);
    }
    found
}

/// The types a PostgreSQL user writes, from `pg_type.dat`: base, range and
/// multirange types that are not arrays, pseudo-types or internal ones.
fn pg_types(entries: &[BTreeMap<String, String>]) -> BTreeMap<String, Value> {
    let mut found = BTreeMap::new();
    for entry in entries {
        let Some(name) = entry.get("typname") else {
            continue;
        };
        let typtype = entry.get("typtype").map_or("b", String::as_str);
        let category = entry.get("typcategory").map_or("", String::as_str);
        if name.starts_with('_')
            || !matches!(typtype, "b" | "r" | "m")
            || matches!(category, "P" | "X" | "Z")
        {
            continue;
        }
        let (display, params) = pg_display(name);
        let mut line = json!({
            "display": display,
            "category": match category {
                "B" => "boolean",
                "D" | "T" => "datetime",
                "G" => "geometric",
                "I" => "network",
                "N" => "numeric",
                "R" => "range",
                "S" => "string",
                "V" => "bit",
                _ => "other",
            },
            "source": "pg_type.dat",
        });
        if display != name {
            line["aliases"] = json!([display]);
        }
        if let Some(params) = params {
            line["params"] = json!(params);
        }
        found.insert(name.clone(), line);
    }
    found
}

/// The major version of a PostgreSQL release tag (`REL_18_0` is 18).
fn pg_major(tag: &str) -> Result<(String, String), String> {
    let parts: Vec<&str> = tag
        .strip_prefix("REL_")
        .ok_or_else(|| format!("{tag:?} is not a REL_<major>_<minor> tag"))?
        .split('_')
        .collect();
    match parts.as_slice() {
        [major, minor] if major.parse::<u32>().is_ok() && minor.parse::<u32>().is_ok() => {
            Ok(((*major).to_owned(), format!("{major}.{minor}")))
        }
        _ => Err(format!("{tag:?} is not a REL_<major>_<minor> tag")),
    }
}

fn postgresql(
    root: &Path,
    dir: &Path,
    tags: &[String],
    verified_through: Option<String>,
) -> Result<(), String> {
    const FILES: [&str; 3] = [
        "src/include/parser/kwlist.h",
        "src/include/catalog/pg_proc.dat",
        "src/include/catalog/pg_type.dat",
    ];
    let sources = root.join("target/catalog-sources/postgresql");
    let mut kinds: [Vec<(String, BTreeMap<String, Value>)>; 3] = Default::default();
    let mut last_release = String::new();
    for tag in tags {
        let (major, release) = pg_major(tag)?;
        last_release = release;
        let mut texts = Vec::new();
        for file in FILES.iter().chain(["COPYRIGHT"].iter()) {
            let path = sources.join(tag).join(file);
            download(
                &format!("https://raw.githubusercontent.com/postgres/postgres/{tag}/{file}"),
                &path,
            )?;
            texts.push(read(&path)?);
        }
        let procs = dat_entries(&texts[1]).map_err(|e| format!("{tag} pg_proc.dat: {e}"))?;
        let types = dat_entries(&texts[2]).map_err(|e| format!("{tag} pg_type.dat: {e}"))?;
        kinds[0].push((major.clone(), kwlist(&texts[0])));
        kinds[1].push((major.clone(), pg_functions(&procs)));
        kinds[2].push((major, pg_types(&types)));
        // The license notice the extracted descriptions carry, from the
        // latest release read.
        fs::create_dir_all(dir.join("postgresql")).map_err(|e| e.to_string())?;
        fs::write(dir.join("postgresql/LICENSE-postgresql"), &texts[3])
            .map_err(|e| e.to_string())?;
    }
    if kinds[0].is_empty() {
        return Err("No PostgreSQL tags given".into());
    }
    let (lines, versions) = fold_kinds(kinds);
    let blob = |file: &str| {
        format!("https://github.com/postgres/postgres/blob/REL_{{series}}_STABLE/{file}")
    };
    let manifest = json!({
        "schema_version": 1,
        "dialect": "postgresql",
        "name": "PostgreSQL",
        "version_scheme": "numeric",
        "versions": versions,
        "verified_through": verified_through.unwrap_or(last_release),
        "series_components": 1,
        "doc_base": "https://www.postgresql.org/docs/{series}/",
        "files": files(),
        "generator": generator(),
        "sources": [
            json!({"id": "kwlist.h", "title": "Keywords of the parser", "url": blob(FILES[0]), "license": "PostgreSQL", "notice": "LICENSE-postgresql", "redistributable": true}),
            json!({"id": "pg_proc.dat", "title": "Built-in functions of the catalog", "url": blob(FILES[1]), "license": "PostgreSQL", "notice": "LICENSE-postgresql", "redistributable": true}),
            json!({"id": "pg_type.dat", "title": "Built-in types of the catalog", "url": blob(FILES[2]), "license": "PostgreSQL", "notice": "LICENSE-postgresql", "redistributable": true}),
            curated(),
        ],
    });
    write_dialect(&dir.join("postgresql"), manifest, lines)
}

/// Folds each kind's snapshots; the versions that changed any kind are the
/// manifest's versions, and every kind is folded over the same list.
fn fold_kinds(
    kinds: [Vec<(String, BTreeMap<String, Value>)>; 3],
) -> ([Vec<Value>; 3], Vec<String>) {
    let mut changed: BTreeSet<usize> = BTreeSet::new();
    for (index, (_, key)) in KINDS.iter().enumerate() {
        let (_, versions) = fold(key, &kinds[index]);
        for version in versions {
            if let Some(at) = kinds[index].iter().position(|(known, _)| *known == version) {
                changed.insert(at);
            }
        }
    }
    // Only versions that changed something stay; the snapshots of the others
    // equal the version before them, so dropping them changes no line.
    let keep = |snapshots: &[(String, BTreeMap<String, Value>)]| -> Vec<(String, BTreeMap<String, Value>)> {
        changed.iter().map(|at| snapshots[*at].clone()).collect()
    };
    let versions: Vec<String> = changed.iter().map(|at| kinds[0][*at].0.clone()).collect();
    let lines = [0, 1, 2].map(|index| fold(KINDS[index].1, &keep(&kinds[index])).0);
    (lines, versions)
}

// --- MySQL -----------------------------------------------------------------

/// Curated MySQL types (project content, CC0).
fn mysql_types(vector: bool) -> BTreeMap<String, Value> {
    let mut found: BTreeMap<String, Value> = [
        curated_type("tinyint", "TINYINT", &[], None),
        curated_type("smallint", "SMALLINT", &[], None),
        curated_type("mediumint", "MEDIUMINT", &[], None),
        curated_type("int", "INT", &["integer"], None),
        curated_type("bigint", "BIGINT", &[], None),
        curated_type(
            "decimal",
            "DECIMAL",
            &["numeric", "dec", "fixed"],
            Some("(precision, scale)"),
        ),
        curated_type("float", "FLOAT", &[], None),
        curated_type("double", "DOUBLE", &["double precision", "real"], None),
        curated_type("bit", "BIT", &[], Some("(n)")),
        curated_type("boolean", "BOOLEAN", &["bool"], None),
        curated_type("date", "DATE", &[], None),
        curated_type("datetime", "DATETIME", &[], Some("(fsp)")),
        curated_type("timestamp", "TIMESTAMP", &[], Some("(fsp)")),
        curated_type("time", "TIME", &[], Some("(fsp)")),
        curated_type("year", "YEAR", &[], None),
        curated_type("char", "CHAR", &["character"], Some("(n)")),
        curated_type("varchar", "VARCHAR", &["character varying"], Some("(n)")),
        curated_type("binary", "BINARY", &[], Some("(n)")),
        curated_type("varbinary", "VARBINARY", &[], Some("(n)")),
        curated_type("tinyblob", "TINYBLOB", &[], None),
        curated_type("blob", "BLOB", &[], None),
        curated_type("mediumblob", "MEDIUMBLOB", &[], None),
        curated_type("longblob", "LONGBLOB", &[], None),
        curated_type("tinytext", "TINYTEXT", &[], None),
        curated_type("text", "TEXT", &[], None),
        curated_type("mediumtext", "MEDIUMTEXT", &[], None),
        curated_type("longtext", "LONGTEXT", &[], None),
        curated_type("enum", "ENUM", &[], Some("('value', ...)")),
        curated_type("set", "SET", &[], Some("('value', ...)")),
        curated_type("json", "JSON", &[], None),
        curated_type("geometry", "GEOMETRY", &[], None),
        curated_type("point", "POINT", &[], None),
        curated_type("linestring", "LINESTRING", &[], None),
        curated_type("polygon", "POLYGON", &[], None),
        curated_type("multipoint", "MULTIPOINT", &[], None),
        curated_type("multilinestring", "MULTILINESTRING", &[], None),
        curated_type("multipolygon", "MULTIPOLYGON", &[], None),
        curated_type("geometrycollection", "GEOMETRYCOLLECTION", &[], None),
    ]
    .into_iter()
    .collect();
    if vector {
        let (name, line) = curated_type("vector", "VECTOR", &[], Some("(dimension)"));
        found.insert(name, line);
    }
    found
}

/// The keywords and functions one MySQL server reports, from
/// `INFORMATION_SCHEMA.KEYWORDS` and the names (never the text) of its help
/// topics, whose content is the manual's.
fn mysql_snapshot(
    keywords_tsv: &str,
    topics_tsv: &str,
) -> (BTreeMap<String, Value>, BTreeMap<String, Value>) {
    let mut keywords = BTreeMap::new();
    for line in keywords_tsv.lines() {
        if let Some((word, reserved)) = line.split_once('\t') {
            keywords.insert(
                word.trim().to_lowercase(),
                json!({"reserved": reserved.trim() == "1", "source": "information_schema.keywords"}),
            );
        }
    }
    let mut functions = BTreeMap::new();
    for line in topics_tsv.lines() {
        let Some((name, category)) = line.split_once('\t') else {
            continue;
        };
        let name = name.trim().to_lowercase();
        if !category.contains("Functions") || !identifier(&name) {
            continue;
        }
        let kind = if category.contains("Aggregate") {
            "aggregate"
        } else if category.contains("Window") {
            "window"
        } else {
            "scalar"
        };
        functions.insert(
            format!("{name}(…)"),
            json!({"name": name, "kind": kind, "signature": format!("{name}(…)"), "source": "mysql.help_topic"}),
        );
    }
    (keywords, functions)
}

fn mysql(root: &Path, dir: &Path) -> Result<(), String> {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or_default();
    let project = super::run_id(std::process::id(), nanos);
    let services = ["mysql-lts", "mysql-current"];
    let result = (|| {
        let mut up = vec!["up", "-d", "--wait"];
        up.extend(services);
        super::compose(root, &project, &up)?;
        let query = |service: &str, sql: &str| {
            super::compose_output(
                root,
                &project,
                &[
                    "exec",
                    "-T",
                    service,
                    "mysql",
                    "-uroot",
                    "-pjykre-test",
                    "-N",
                    "-B",
                    "-e",
                    sql,
                ],
            )
        };
        let mut kinds: [Vec<(String, BTreeMap<String, Value>)>; 3] = Default::default();
        for service in services {
            let version = query(service, "SELECT VERSION()")?.trim().to_owned();
            let version = version
                .split(['-', ' '])
                .next()
                .unwrap_or_default()
                .to_owned();
            let keywords = query(
                service,
                "SELECT WORD, RESERVED FROM INFORMATION_SCHEMA.KEYWORDS ORDER BY WORD",
            )?;
            let topics = query(
                service,
                "SELECT t.name, c.name FROM mysql.help_topic t JOIN mysql.help_category c ON c.help_category_id = t.help_category_id ORDER BY t.name",
            )?;
            let (keywords, functions) = mysql_snapshot(&keywords, &topics);
            // VECTOR arrived in MySQL 9.0.
            let vector = compare(Scheme::Numeric, &version, "9.0").is_some_and(Ordering::is_ge);
            kinds[0].push((version.clone(), keywords));
            kinds[1].push((version.clone(), functions));
            kinds[2].push((version.clone(), mysql_types(vector)));
        }
        kinds.iter_mut().for_each(|kind| {
            kind.sort_by(|a, b| compare(Scheme::Numeric, &a.0, &b.0).unwrap_or(Ordering::Equal));
        });
        let verified = kinds[0]
            .last()
            .map(|(version, _)| version.clone())
            .unwrap_or_default();
        let (lines, versions) = fold_kinds(kinds);
        let manifest = json!({
            "schema_version": 1,
            "dialect": "mysql",
            "name": "MySQL",
            "version_scheme": "numeric",
            "versions": versions,
            "verified_through": verified,
            "series_components": 2,
            "doc_base": "https://dev.mysql.com/doc/refman/{series}/en/",
            "files": files(),
            "generator": generator(),
            "sources": [
                source("information_schema.keywords", "INFORMATION_SCHEMA.KEYWORDS of the server", "https://dev.mysql.com/doc/refman/8.4/en/information-schema-keywords-table.html", "facts", false),
                source("mysql.help_topic", "Names of the server's help topics (never their text)", "https://dev.mysql.com/doc/refman/8.4/en/server-side-help-support.html", "facts", false),
                curated(),
            ],
        });
        write_dialect(&dir.join("mysql"), manifest, lines)
    })();
    let cleanup = super::compose(root, &project, &["down", "--volumes"]);
    super::combine(result, cleanup)
}

// --- SQL Server ------------------------------------------------------------

/// The reserved keywords of Transact-SQL, as Microsoft lists them (facts).
/// SQL Server has no table of its keywords to read them from.
const SQLSERVER_RESERVED: &[&str] = &[
    "add",
    "all",
    "alter",
    "and",
    "any",
    "as",
    "asc",
    "authorization",
    "backup",
    "begin",
    "between",
    "break",
    "browse",
    "bulk",
    "by",
    "cascade",
    "case",
    "check",
    "checkpoint",
    "close",
    "clustered",
    "coalesce",
    "collate",
    "column",
    "commit",
    "compute",
    "constraint",
    "contains",
    "containstable",
    "continue",
    "convert",
    "create",
    "cross",
    "current",
    "current_date",
    "current_time",
    "current_timestamp",
    "current_user",
    "cursor",
    "database",
    "dbcc",
    "deallocate",
    "declare",
    "default",
    "delete",
    "deny",
    "desc",
    "disk",
    "distinct",
    "distributed",
    "double",
    "drop",
    "dump",
    "else",
    "end",
    "errlvl",
    "escape",
    "except",
    "exec",
    "execute",
    "exists",
    "exit",
    "external",
    "fetch",
    "file",
    "fillfactor",
    "for",
    "foreign",
    "freetext",
    "freetexttable",
    "from",
    "full",
    "function",
    "goto",
    "grant",
    "group",
    "having",
    "holdlock",
    "identity",
    "identity_insert",
    "identitycol",
    "if",
    "in",
    "index",
    "inner",
    "insert",
    "intersect",
    "into",
    "is",
    "join",
    "key",
    "kill",
    "left",
    "like",
    "lineno",
    "load",
    "merge",
    "national",
    "nocheck",
    "nonclustered",
    "not",
    "null",
    "nullif",
    "of",
    "off",
    "offsets",
    "on",
    "open",
    "opendatasource",
    "openquery",
    "openrowset",
    "openxml",
    "option",
    "or",
    "order",
    "outer",
    "over",
    "percent",
    "pivot",
    "plan",
    "precision",
    "primary",
    "print",
    "proc",
    "procedure",
    "public",
    "raiserror",
    "read",
    "readtext",
    "reconfigure",
    "references",
    "replication",
    "restore",
    "restrict",
    "return",
    "revert",
    "revoke",
    "right",
    "rollback",
    "rowcount",
    "rowguidcol",
    "rule",
    "save",
    "schema",
    "securityaudit",
    "select",
    "semantickeyphrasetable",
    "semanticsimilaritydetailstable",
    "semanticsimilaritytable",
    "session_user",
    "set",
    "setuser",
    "shutdown",
    "some",
    "statistics",
    "system_user",
    "table",
    "tablesample",
    "textsize",
    "then",
    "to",
    "top",
    "tran",
    "transaction",
    "trigger",
    "truncate",
    "try_convert",
    "tsequal",
    "union",
    "unique",
    "unpivot",
    "update",
    "updatetext",
    "use",
    "user",
    "values",
    "varying",
    "view",
    "waitfor",
    "when",
    "where",
    "while",
    "with",
    "within",
    "writetext",
];

/// Words of Transact-SQL statements that are not reserved (curated).
const SQLSERVER_KEYWORDS: &[&str] = &[
    "after",
    "apply",
    "catch",
    "cte",
    "encryption",
    "first",
    "following",
    "include",
    "instead",
    "last",
    "login",
    "matched",
    "max",
    "next",
    "nocount",
    "nolock",
    "nowait",
    "offset",
    "only",
    "output",
    "partition",
    "persisted",
    "preceding",
    "range",
    "readonly",
    "recompile",
    "returns",
    "role",
    "rows",
    "schemabinding",
    "sequence",
    "source",
    "synonym",
    "target",
    "throw",
    "ties",
    "try",
    "type",
    "unbounded",
    "xact_abort",
];

/// Built-in Transact-SQL functions (curated from Microsoft's reference),
/// with their kind and the major version that brought them.
const SQLSERVER_FUNCTIONS: &[(&str, &str, u32)] = &[
    ("abs", "scalar", 0),
    ("acos", "scalar", 0),
    ("app_name", "scalar", 0),
    ("approx_count_distinct", "aggregate", 15),
    ("approx_percentile_cont", "aggregate", 16),
    ("approx_percentile_disc", "aggregate", 16),
    ("ascii", "scalar", 0),
    ("asin", "scalar", 0),
    ("atan", "scalar", 0),
    ("atn2", "scalar", 0),
    ("avg", "aggregate", 0),
    ("binary_checksum", "scalar", 0),
    ("bit_count", "scalar", 16),
    ("cast", "scalar", 0),
    ("ceiling", "scalar", 0),
    ("char", "scalar", 0),
    ("charindex", "scalar", 0),
    ("checksum", "scalar", 0),
    ("checksum_agg", "aggregate", 0),
    ("choose", "scalar", 0),
    ("compress", "scalar", 0),
    ("concat", "scalar", 0),
    ("concat_ws", "scalar", 0),
    ("cos", "scalar", 0),
    ("cot", "scalar", 0),
    ("count", "aggregate", 0),
    ("count_big", "aggregate", 0),
    ("cume_dist", "window", 0),
    ("datalength", "scalar", 0),
    ("date_bucket", "scalar", 16),
    ("dateadd", "scalar", 0),
    ("datediff", "scalar", 0),
    ("datediff_big", "scalar", 0),
    ("datefromparts", "scalar", 0),
    ("datename", "scalar", 0),
    ("datepart", "scalar", 0),
    ("datetime2fromparts", "scalar", 0),
    ("datetimefromparts", "scalar", 0),
    ("datetimeoffsetfromparts", "scalar", 0),
    ("datetrunc", "scalar", 16),
    ("day", "scalar", 0),
    ("db_id", "scalar", 0),
    ("db_name", "scalar", 0),
    ("decompress", "scalar", 0),
    ("degrees", "scalar", 0),
    ("dense_rank", "window", 0),
    ("difference", "scalar", 0),
    ("eomonth", "scalar", 0),
    ("error_line", "scalar", 0),
    ("error_message", "scalar", 0),
    ("error_number", "scalar", 0),
    ("error_procedure", "scalar", 0),
    ("error_severity", "scalar", 0),
    ("error_state", "scalar", 0),
    ("exp", "scalar", 0),
    ("first_value", "window", 0),
    ("floor", "scalar", 0),
    ("format", "scalar", 0),
    ("generate_series", "table", 16),
    ("get_bit", "scalar", 16),
    ("getdate", "scalar", 0),
    ("getutcdate", "scalar", 0),
    ("greatest", "scalar", 16),
    ("grouping", "aggregate", 0),
    ("grouping_id", "aggregate", 0),
    ("hashbytes", "scalar", 0),
    ("host_name", "scalar", 0),
    ("ident_current", "scalar", 0),
    ("iif", "scalar", 0),
    ("isdate", "scalar", 0),
    ("isjson", "scalar", 0),
    ("isnull", "scalar", 0),
    ("isnumeric", "scalar", 0),
    ("json_array", "scalar", 16),
    ("json_modify", "scalar", 0),
    ("json_object", "scalar", 16),
    ("json_path_exists", "scalar", 16),
    ("json_query", "scalar", 0),
    ("json_value", "scalar", 0),
    ("lag", "window", 0),
    ("last_value", "window", 0),
    ("lead", "window", 0),
    ("least", "scalar", 16),
    ("left_shift", "scalar", 16),
    ("len", "scalar", 0),
    ("log", "scalar", 0),
    ("log10", "scalar", 0),
    ("lower", "scalar", 0),
    ("ltrim", "scalar", 0),
    ("max", "aggregate", 0),
    ("min", "aggregate", 0),
    ("month", "scalar", 0),
    ("nchar", "scalar", 0),
    ("newid", "scalar", 0),
    ("newsequentialid", "scalar", 0),
    ("ntile", "window", 0),
    ("object_id", "scalar", 0),
    ("object_name", "scalar", 0),
    ("openjson", "table", 0),
    ("parse", "scalar", 0),
    ("patindex", "scalar", 0),
    ("percent_rank", "window", 0),
    ("percentile_cont", "window", 0),
    ("percentile_disc", "window", 0),
    ("pi", "scalar", 0),
    ("power", "scalar", 0),
    ("quotename", "scalar", 0),
    ("radians", "scalar", 0),
    ("rand", "scalar", 0),
    ("rank", "window", 0),
    ("replace", "scalar", 0),
    ("replicate", "scalar", 0),
    ("reverse", "scalar", 0),
    ("right_shift", "scalar", 16),
    ("round", "scalar", 0),
    ("row_number", "window", 0),
    ("rtrim", "scalar", 0),
    ("schema_id", "scalar", 0),
    ("schema_name", "scalar", 0),
    ("scope_identity", "scalar", 0),
    ("set_bit", "scalar", 16),
    ("sign", "scalar", 0),
    ("sin", "scalar", 0),
    ("smalldatetimefromparts", "scalar", 0),
    ("soundex", "scalar", 0),
    ("space", "scalar", 0),
    ("sqrt", "scalar", 0),
    ("square", "scalar", 0),
    ("stdev", "aggregate", 0),
    ("stdevp", "aggregate", 0),
    ("str", "scalar", 0),
    ("string_agg", "aggregate", 0),
    ("string_escape", "scalar", 0),
    ("string_split", "table", 0),
    ("stuff", "scalar", 0),
    ("substring", "scalar", 0),
    ("sum", "aggregate", 0),
    ("suser_sname", "scalar", 0),
    ("switchoffset", "scalar", 0),
    ("sysdatetime", "scalar", 0),
    ("sysdatetimeoffset", "scalar", 0),
    ("sysutcdatetime", "scalar", 0),
    ("tan", "scalar", 0),
    ("timefromparts", "scalar", 0),
    ("todatetimeoffset", "scalar", 0),
    ("translate", "scalar", 0),
    ("trim", "scalar", 0),
    ("try_cast", "scalar", 0),
    ("try_parse", "scalar", 0),
    ("unicode", "scalar", 0),
    ("upper", "scalar", 0),
    ("user_name", "scalar", 0),
    ("var", "aggregate", 0),
    ("varp", "aggregate", 0),
    ("xact_state", "scalar", 0),
    ("year", "scalar", 0),
];

/// How SQL spells a SQL Server system type and its parameters (curated).
fn sqlserver_type(name: &str) -> (String, Value) {
    let params = match name {
        "binary" | "char" | "nchar" | "nvarchar" | "varbinary" | "varchar" => Some("(n | max)"),
        "decimal" | "numeric" => Some("(precision, scale)"),
        "datetime2" | "datetimeoffset" | "time" => Some("(fractional seconds)"),
        "float" => Some("(mantissa bits)"),
        "vector" => Some("(dimensions)"),
        _ => None,
    };
    let aliases: &[&str] = match name {
        "int" => &["integer"],
        "decimal" => &["dec"],
        "float" => &["double precision"],
        "nvarchar" => &["national character varying"],
        "nchar" => &["national character"],
        "varchar" => &["character varying"],
        "char" => &["character"],
        "rowversion" => &["timestamp"],
        _ => &[],
    };
    let (name, mut line) = curated_type(name, &name.to_ascii_uppercase(), aliases, params);
    line["source"] = json!("sys.types");
    (name, line)
}

/// The keywords, functions and types of one SQL Server version: its major
/// number and the names of its system types (`sys.types`).
fn sqlserver_snapshot(major: u32, types: &str) -> [BTreeMap<String, Value>; 3] {
    let mut keywords = BTreeMap::new();
    for word in SQLSERVER_RESERVED {
        keywords.insert(
            (*word).to_owned(),
            json!({"reserved": true, "source": "tsql-reserved-keywords"}),
        );
    }
    for word in SQLSERVER_KEYWORDS {
        keywords
            .entry((*word).to_owned())
            .or_insert_with(|| json!({"reserved": false, "source": "curated"}));
    }
    let functions = SQLSERVER_FUNCTIONS
        .iter()
        .filter(|(_, _, since)| *since <= major)
        .map(|(name, kind, _)| {
            (
                format!("{name}(…)"),
                json!({"name": name, "kind": kind, "signature": format!("{name}(…)"), "source": "curated"}),
            )
        })
        .collect();
    let types = types
        .lines()
        .map(str::trim)
        .filter(|name| identifier(name) && *name != "sysname")
        .map(|name| sqlserver_type(&name.to_ascii_lowercase()))
        .collect();
    [keywords, functions, types]
}

fn sqlserver(root: &Path, dir: &Path) -> Result<(), String> {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or_default();
    let project = super::run_id(std::process::id(), nanos);
    let services = ["sqlserver-2019", "sqlserver-2022"];
    let result = (|| {
        let mut up = vec!["up", "-d", "--wait"];
        up.extend(services);
        super::compose(root, &project, &up)?;
        let query = |service: &str, sql: &str| {
            let sql = format!("SET NOCOUNT ON; {sql}");
            super::sqlcmd(root, &project, service, &["-h", "-1", "-W", "-Q", &sql])
        };
        let mut kinds: [Vec<(String, BTreeMap<String, Value>)>; 3] = Default::default();
        for service in services {
            let version = query(
                service,
                "SELECT CAST(SERVERPROPERTY('ProductVersion') AS nvarchar(128))",
            )?
            .trim()
            .to_owned();
            let major = version
                .split('.')
                .next()
                .and_then(|major| major.parse::<u32>().ok())
                .ok_or_else(|| format!("Unexpected SQL Server version {version:?}"))?;
            let types = query(
                service,
                "SELECT name FROM sys.types WHERE is_user_defined = 0 ORDER BY name",
            )?;
            let [keywords, functions, types] = sqlserver_snapshot(major, &types);
            kinds[0].push((version.clone(), keywords));
            kinds[1].push((version.clone(), functions));
            kinds[2].push((version, types));
        }
        kinds.iter_mut().for_each(|kind| {
            kind.sort_by(|a, b| compare(Scheme::Numeric, &a.0, &b.0).unwrap_or(Ordering::Equal));
        });
        let verified = kinds[0]
            .last()
            .map(|(version, _)| version.clone())
            .unwrap_or_default();
        let (lines, versions) = fold_kinds(kinds);
        let manifest = json!({
            "schema_version": 1,
            "dialect": "sqlserver",
            "name": "SQL Server",
            "version_scheme": "numeric",
            "versions": versions,
            "verified_through": verified,
            "series_components": 1,
            "doc_base": "https://learn.microsoft.com/sql/t-sql/",
            "files": files(),
            "generator": generator(),
            "sources": [
                source("tsql-reserved-keywords", "Transact-SQL reserved keywords (the list, not its text)", "https://learn.microsoft.com/sql/t-sql/language-elements/reserved-keywords-transact-sql", "facts", false),
                source("sys.types", "System types of the server (sys.types)", "https://learn.microsoft.com/sql/relational-databases/system-catalog-views/sys-types-transact-sql", "facts", false),
                curated(),
            ],
        });
        write_dialect(&dir.join("sqlserver"), manifest, lines)
    })();
    let cleanup = super::compose(root, &project, &["down", "--volumes"]);
    super::combine(result, cleanup)
}

// --- Databricks ------------------------------------------------------------

/// The keywords of Spark's ANSI compliance page, reserved as in ANSI mode
/// (the default of Databricks SQL).
fn spark_keywords(text: &str) -> BTreeMap<String, Value> {
    let mut found = BTreeMap::new();
    let mut table = false;
    for line in text.lines() {
        if line.starts_with("|Keyword|") {
            table = true;
            continue;
        }
        if !table {
            continue;
        }
        let cells: Vec<&str> = line.trim_matches('|').split('|').map(str::trim).collect();
        match cells.as_slice() {
            [word, ansi, ..] if identifier(word) => {
                found.insert(
                    word.to_lowercase(),
                    json!({"reserved": *ansi == "reserved", "source": "spark-keywords"}),
                );
            }
            [first, ..] if first.starts_with('-') => {}
            _ if !found.is_empty() => break,
            _ => {}
        }
    }
    found
}

/// Spark's generator functions, which return rows.
const SPARK_TABLE_FUNCTIONS: &[&str] = &[
    "explode",
    "explode_outer",
    "posexplode",
    "posexplode_outer",
    "inline",
    "inline_outer",
    "stack",
    "json_tuple",
];

/// Spark's window functions.
const SPARK_WINDOW_FUNCTIONS: &[&str] = &[
    "row_number",
    "rank",
    "dense_rank",
    "percent_rank",
    "cume_dist",
    "ntile",
    "lag",
    "lead",
    "nth_value",
];

/// The built-in functions of Spark's generated schema table: one row per
/// function name or alias.
fn spark_functions(text: &str) -> BTreeMap<String, Value> {
    let mut found = BTreeMap::new();
    for line in text.lines() {
        let cells: Vec<&str> = line.trim_matches('|').split('|').map(str::trim).collect();
        let [class, name, ..] = cells.as_slice() else {
            continue;
        };
        if !class.starts_with("org.apache.spark") || !identifier(name) {
            continue;
        }
        let name = name.to_lowercase();
        let kind = if class.contains(".aggregate.") {
            "aggregate"
        } else if SPARK_WINDOW_FUNCTIONS.contains(&name.as_str()) {
            "window"
        } else if SPARK_TABLE_FUNCTIONS.contains(&name.as_str()) {
            "table"
        } else {
            "scalar"
        };
        found.insert(
            format!("{name}(…)"),
            json!({"name": name, "kind": kind, "signature": format!("{name}(…)"), "doc": format!("functions/{name}"), "source": "spark-functions"}),
        );
    }
    found
}

/// Curated Databricks SQL types (project content, CC0).
fn databricks_types() -> BTreeMap<String, Value> {
    [
        curated_type("bigint", "BIGINT", &["long"], None),
        curated_type("binary", "BINARY", &[], None),
        curated_type("boolean", "BOOLEAN", &[], None),
        curated_type("date", "DATE", &[], None),
        curated_type(
            "decimal",
            "DECIMAL",
            &["dec", "numeric"],
            Some("(precision, scale)"),
        ),
        curated_type("double", "DOUBLE", &[], None),
        curated_type("float", "FLOAT", &["real"], None),
        curated_type("int", "INT", &["integer"], None),
        curated_type("interval", "INTERVAL", &[], Some("start [TO end]")),
        curated_type("smallint", "SMALLINT", &["short"], None),
        curated_type("string", "STRING", &[], None),
        curated_type("char", "CHAR", &[], Some("(n)")),
        curated_type("varchar", "VARCHAR", &[], Some("(n)")),
        curated_type("timestamp", "TIMESTAMP", &[], None),
        curated_type("timestamp_ntz", "TIMESTAMP_NTZ", &[], None),
        curated_type("tinyint", "TINYINT", &["byte"], None),
        curated_type("void", "VOID", &[], None),
        curated_type("array", "ARRAY", &[], Some("<element>")),
        curated_type("map", "MAP", &[], Some("<key, value>")),
        curated_type("struct", "STRUCT", &[], Some("<field: type, ...>")),
        curated_type("variant", "VARIANT", &[], None),
    ]
    .into_iter()
    .collect()
}

/// A CSV export's rows after the header, split on commas (the exports this
/// reads hold names and booleans only).
fn csv_rows(text: &str) -> Vec<Vec<String>> {
    text.lines()
        .skip(1)
        .map(|line| {
            line.split(',')
                .map(|cell| cell.trim().trim_matches('"').to_owned())
                .collect()
        })
        .filter(|cells: &Vec<String>| cells.first().is_some_and(|cell| !cell.is_empty()))
        .collect()
}

fn databricks(
    root: &Path,
    dir: &Path,
    spark_tag: &str,
    keywords_csv: Option<PathBuf>,
    functions_csv: Option<PathBuf>,
    observed: Option<String>,
) -> Result<(), String> {
    let sources = root.join("target/catalog-sources/spark").join(spark_tag);
    const ANSI: &str = "docs/sql-ref-ansi-compliance.md";
    const SCHEMA: &str = "sql/core/src/test/resources/sql-functions/sql-expression-schema.md";
    for file in [ANSI, SCHEMA] {
        download(
            &format!("https://raw.githubusercontent.com/apache/spark/{spark_tag}/{file}"),
            &sources.join(file),
        )?;
    }
    let mut keywords = spark_keywords(&read(&sources.join(ANSI))?);
    let mut functions = spark_functions(&read(&sources.join(SCHEMA))?);
    let mut exported = false;
    // Exports of `SELECT keyword, reserved FROM sql_keywords()` and
    // `SHOW SYSTEM FUNCTIONS` from a workspace, when the maintainer has them.
    if let Some(path) = &keywords_csv {
        for row in csv_rows(&read(path)?) {
            let reserved = row
                .get(1)
                .is_some_and(|cell| cell.eq_ignore_ascii_case("true"));
            keywords.insert(
                row[0].to_lowercase(),
                json!({"reserved": reserved, "source": "databricks-export"}),
            );
        }
        exported = true;
    }
    if let Some(path) = &functions_csv {
        for row in csv_rows(&read(path)?) {
            let name = row[0].rsplit('.').next().unwrap_or_default().to_lowercase();
            if identifier(&name) {
                functions.entry(format!("{name}(…)")).or_insert_with(|| {
                    json!({"name": name, "kind": "scalar", "signature": format!("{name}(…)"), "doc": format!("functions/{name}"), "source": "databricks-export"})
                });
            }
        }
        exported = true;
    }
    let observed = observed.unwrap_or_else(today);
    let kinds = [
        vec![(observed.clone(), keywords)],
        vec![(observed.clone(), functions)],
        vec![(observed.clone(), databricks_types())],
    ];
    let (lines, versions) = fold_kinds(kinds);
    let mut sources = vec![
        source(
            "spark-keywords",
            &format!("Keywords of Apache Spark {spark_tag} (ANSI mode)"),
            &format!("https://github.com/apache/spark/blob/{spark_tag}/{ANSI}"),
            "Apache-2.0",
            true,
        ),
        source(
            "spark-functions",
            &format!("Built-in functions of Apache Spark {spark_tag}"),
            &format!("https://github.com/apache/spark/blob/{spark_tag}/{SCHEMA}"),
            "Apache-2.0",
            true,
        ),
        curated(),
    ];
    if exported {
        sources.push(source(
            "databricks-export",
            "sql_keywords() and SHOW SYSTEM FUNCTIONS exported from a workspace",
            "https://docs.databricks.com/aws/en/sql/language-manual/functions/sql_keywords",
            "facts",
            false,
        ));
    }
    let manifest = json!({
        "schema_version": 1,
        "dialect": "databricks",
        "name": "Databricks SQL",
        "version_scheme": "date",
        "versions": versions,
        "verified_through": observed,
        "doc_base": "https://docs.databricks.com/aws/en/sql/language-manual/",
        "files": files(),
        "generator": generator(),
        "sources": sources,
    });
    write_dialect(&dir.join("databricks"), manifest, lines)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(version: &str, entries: &[(&str, Value)]) -> (String, BTreeMap<String, Value>) {
        (
            version.to_owned(),
            entries
                .iter()
                .map(|(key, value)| ((*key).to_owned(), value.clone()))
                .collect(),
        )
    }

    #[test]
    fn folding_versions_opens_closes_and_reopens_lines() {
        let reserved = json!({"reserved": true, "source": "s"});
        let free = json!({"reserved": false, "source": "s"});
        let (lines, versions) = fold(
            "word",
            &[
                snapshot("14", &[("order", reserved.clone()), ("old", free.clone())]),
                snapshot("15", &[("order", reserved.clone()), ("old", free.clone())]),
                snapshot("16", &[("order", reserved.clone())]),
                snapshot(
                    "17",
                    &[("order", free.clone()), ("json_table", free.clone())],
                ),
            ],
        );
        let text: Vec<String> = lines.iter().map(canonical).collect();
        assert_eq!(
            text,
            [
                r#"{"reserved":false,"since":"17","source":"s","word":"json_table"}"#,
                r#"{"reserved":false,"source":"s","until":"16","word":"old"}"#,
                r#"{"reserved":true,"source":"s","until":"17","word":"order"}"#,
                r#"{"reserved":false,"since":"17","source":"s","word":"order"}"#,
            ]
        );
        // 15 changed nothing, so it is not a version of its own.
        assert_eq!(versions, ["14", "16", "17"]);
    }

    #[test]
    fn canonical_json_sorts_keys_at_every_level() {
        let value = json!({"b": 1, "a": {"d": true, "c": "é"}, "list": [{"z": 1, "y": 2}]});
        assert_eq!(
            canonical(&value),
            r#"{"a":{"c":"é","d":true},"b":1,"list":[{"y":2,"z":1}]}"#
        );
    }

    #[test]
    fn postgres_data_files_and_keywords_are_read() {
        let dat = r#"[
# a comment with 'quotes'
{ oid => '1284',
  descr => 'truncate timestamp with time zone to specified units in specified time zone',
  proname => 'date_trunc', prorettype => 'timestamptz',
  proargtypes => 'text timestamptz text', prosrc => 'timestamptz_trunc_zone' },
{ oid => '42', proname => 'int4pl', prorettype => 'int4', proargtypes => 'int4 int4' },
{ oid => '43', descr => 'I/O', proname => 'int4in', prorettype => 'int4', proargtypes => 'cstring' },
{ oid => '1066', descr => 'non-persistent series generator', proname => 'generate_series',
  proretset => 't', prorettype => 'int4', proargtypes => 'int4 int4' },
{ oid => '2', descr => 'format \'quoted\'', proname => 'concat', provariadic => 'any',
  prorettype => 'text', proargtypes => 'any', proargnames => '{args}' },
]"#;
        let entries = dat_entries(dat).unwrap();
        assert_eq!(entries.len(), 5);
        assert_eq!(entries[4]["descr"], "format 'quoted'");
        let functions = pg_functions(&entries);
        assert_eq!(
            functions.keys().collect::<Vec<_>>(),
            [
                "concat(any)",
                "date_trunc(text,timestamptz,text)",
                "generate_series(int4,int4)"
            ]
        );
        assert_eq!(
            functions["date_trunc(text,timestamptz,text)"]["signature"],
            "date_trunc(text, timestamp with time zone, text) → timestamp with time zone"
        );
        assert_eq!(functions["generate_series(int4,int4)"]["kind"], "table");
        assert_eq!(
            functions["concat(any)"]["signature"],
            "concat(VARIADIC args any) → text"
        );
        let keywords = kwlist(
            "PG_KEYWORD(\"order\", ORDER, RESERVED_KEYWORD, AS_LABEL)\nPG_KEYWORD(\"left\", LEFT, TYPE_FUNC_NAME_KEYWORD, BARE_LABEL)\nPG_KEYWORD(\"schema\", SCHEMA, UNRESERVED_KEYWORD, BARE_LABEL)",
        );
        assert_eq!(keywords["order"]["reserved"], true);
        assert!(keywords["order"].get("bare_label").is_none());
        assert_eq!(keywords["left"]["reserved"], true);
        assert_eq!(keywords["schema"]["reserved"], false);
        assert_eq!(pg_major("REL_18_0").unwrap(), ("18".into(), "18.0".into()));
        assert!(pg_major("v18").is_err());
    }

    #[test]
    fn mysql_and_spark_names_are_read_without_their_text() {
        let (keywords, functions) = mysql_snapshot(
            "SELECT\t1\nNAME\t0\n",
            "ABS\tNumeric Functions\nCOUNT\tAggregate Functions and Modifiers\nCREATE TABLE\tData Definition\n+\tArithmetic Operators\nROW_NUMBER\tWindow Functions\n",
        );
        assert_eq!(keywords["select"]["reserved"], true);
        assert_eq!(keywords["name"]["reserved"], false);
        assert_eq!(
            functions.keys().collect::<Vec<_>>(),
            ["abs(…)", "count(…)", "row_number(…)"]
        );
        assert_eq!(functions["count(…)"]["kind"], "aggregate");
        assert_eq!(functions["row_number(…)"]["kind"], "window");
        let keywords = spark_keywords(
            "intro\n|Keyword|Spark SQL<br/>ANSI Mode|Spark SQL<br/>NonANSI Mode|SQL-2016|\n|-------|----|----|----|\n|ADD|non-reserved|non-reserved|non-reserved|\n|AND|reserved|non-reserved|reserved|\n\nafter",
        );
        assert_eq!(keywords.len(), 2);
        assert_eq!(keywords["and"]["reserved"], true);
        let functions = spark_functions(
            "| org.apache.spark.sql.catalyst.expressions.Abs | abs | SELECT abs(-1) | struct<abs(-1):int> |\n| org.apache.spark.sql.catalyst.expressions.Add | + | SELECT 1 + 2 | struct<> |\n| org.apache.spark.sql.catalyst.expressions.aggregate.Count | count | SELECT count(*) | struct<> |\n| org.apache.spark.sql.catalyst.expressions.Explode | explode | SELECT explode(array(1)) | struct<> |",
        );
        assert_eq!(functions.len(), 3);
        assert_eq!(functions["count(…)"]["kind"], "aggregate");
        assert_eq!(functions["explode(…)"]["kind"], "table");
    }

    #[test]
    fn the_check_refuses_what_the_format_forbids() {
        let root = std::env::temp_dir().join(format!("xtask-catalog-{}", std::process::id()));
        let dialect = root.join("postgresql");
        fs::create_dir_all(&dialect).unwrap();
        let manifest = json!({
            "schema_version": 1, "version_scheme": "numeric", "versions": ["14", "17"],
            "verified_through": "17.2",
            "sources": [{"id": "s", "license": "PostgreSQL", "redistributable": true},
                        {"id": "m", "license": "facts", "redistributable": false}],
        });
        fs::write(dialect.join("manifest.json"), manifest.to_string()).unwrap();
        let write = |file: &str, lines: &[&str]| {
            fs::write(
                dialect.join(file),
                lines.iter().map(|l| format!("{l}\n")).collect::<String>(),
            )
            .unwrap();
        };
        write("functions.jsonl", &[]);
        write(
            "types.jsonl",
            &[r#"{"display":"integer","name":"int4","source":"s"}"#],
        );
        write(
            "keywords.jsonl",
            &[
                r#"{"reserved":true,"source":"s","until":"17","word":"order"}"#,
                r#"{"reserved":false,"since":"17","source":"s","word":"order"}"#,
            ],
        );
        assert!(check_dialect(&dialect).is_ok());
        write(
            "keywords.jsonl",
            &[
                r#"{"source":"s","reserved":true,"word":"a"}"#,
                r#"{"reserved":true,"source":"s","word":"order"}"#,
                r#"{"reserved":false,"since":"17","source":"s","word":"order"}"#,
                r#"{"reserved":true,"since":"15","source":"s","word":"x"}"#,
                r#"{"reserved":true,"source":"nowhere","word":"y"}"#,
                r#"{"reserved":true,"source":"s","word":"b"}"#,
            ],
        );
        write(
            "functions.jsonl",
            &[
                r#"{"id":"f()","kind":"scalar","name":"f","signature":"f()","source":"s","summary":"text","summary_source":"m"}"#,
            ],
        );
        let problems = check_dialect(&dialect).unwrap_err().join("\n");
        fs::remove_dir_all(&root).unwrap();
        for expected in [
            "keywords.jsonl:1: not in canonical form",
            "keywords.jsonl:3: validity overlaps the line before",
            "keywords.jsonl:4: since is not a tracked version",
            "keywords.jsonl:5: unknown source",
            "keywords.jsonl:6: not sorted by word",
            "functions.jsonl:1: summary without a redistributable summary_source",
        ] {
            assert!(problems.contains(expected), "{expected:?} in {problems}");
        }
    }

    #[test]
    fn dates_and_options() {
        assert_eq!(civil_date(0), "1970-01-01");
        assert_eq!(civil_date(20_761), "2026-11-04");
        assert_eq!(options(&["check"]), Ok(Job::Check));
        assert_eq!(
            options(&[
                "postgresql",
                "REL_17_0,REL_18_0",
                "--verified-through",
                "18.6"
            ]),
            Ok(Job::Postgresql {
                tags: vec!["REL_17_0".into(), "REL_18_0".into()],
                verified_through: Some("18.6".into()),
            })
        );
        assert!(options(&["postgresql", "REL_18_0", "--verbose"]).is_err());
        assert!(options(&["databricks"]).is_err());
    }
}
