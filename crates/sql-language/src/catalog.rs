//! What each dialect has built in (keywords, functions and types), read from
//! a catalog directory laid out as
//! [the catalog specification](../../../docs/especificacao-catalogo-sql.md)
//! says: `<dir>/<dialect>/manifest.json` and one JSON Lines file per kind of
//! entry. Every entry holds the range of server versions it is valid for.
//!
//! Pure: the caller reads the files (off the UI thread) and hands their text
//! over; this parses them and keeps the entries valid for one version.
//! Without a catalog, [`DialectCatalog::builtin`] keeps the few words the
//! editor always knew.
use jykre_core::SqlDialect;
use serde::Deserialize;
use std::cmp::Ordering;
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// The directory each dialect has inside the catalog directory.
pub const DIALECTS: [(SqlDialect, &str); 4] = [
    (SqlDialect::Postgres, "postgresql"),
    (SqlDialect::MySql, "mysql"),
    (SqlDialect::DatabricksSql, "databricks"),
    (SqlDialect::SqlServer, "sqlserver"),
];

/// The files of a dialect directory, in the order of [`DialectFiles`].
pub const FILES: [&str; 4] = [
    "manifest.json",
    "keywords.jsonl",
    "functions.jsonl",
    "types.jsonl",
];

/// The only catalog format this build reads.
pub const SCHEMA_VERSION: u64 = 1;

/// A catalog file larger than this is refused before it is read.
pub const MAX_FILE_BYTES: u64 = 32 * 1024 * 1024;

/// A line longer than this is refused.
const MAX_LINE_BYTES: usize = 1024 * 1024;

/// Why a dialect's catalog was not used. The editor then keeps the words of
/// [`DialectCatalog::builtin`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CatalogError {
    /// The manifest declares a format this build does not read.
    Schema(u64),
    /// A file does not hold what the format says, at a 1-based line (0 for
    /// the manifest as a whole).
    Invalid {
        file: &'static str,
        line: usize,
        reason: String,
    },
}

/// How a dialect numbers its versions.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VersionScheme {
    /// Dotted numbers, compared component by component (`18`, `9.7.0`).
    Numeric,
    /// ISO dates, compared as text (`2026-11-04`).
    Date,
}

/// How the version a catalog was resolved for relates to what it tracks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fit {
    /// No server version was given: the latest tracked version.
    Latest,
    /// The server's version is among those the catalog covers.
    Covered,
    /// The server is older than every tracked version: the first is used.
    Older,
    /// The server is newer than the catalog has verified: the latest is used.
    Newer,
}

/// The texts of a dialect directory's files.
#[derive(Clone, Copy, Debug)]
pub struct DialectFiles<'a> {
    pub manifest: &'a str,
    pub keywords: &'a str,
    pub functions: &'a str,
    pub types: &'a str,
}

/// A keyword and whether it must be quoted to be used as a table or column
/// name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Keyword {
    pub word: String,
    pub reserved: bool,
}

/// What a function returns or does, as the catalog classifies it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FunctionKind {
    Scalar,
    Aggregate,
    Window,
    Table,
    Procedure,
}

/// One overload of a built-in function.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Function {
    pub name: String,
    pub kind: FunctionKind,
    /// The signature as the editor shows it.
    pub signature: String,
    pub returns: Option<String>,
    /// A short description, only from a source that may be redistributed.
    pub summary: Option<String>,
    /// The documentation page, as an absolute URL.
    pub doc: Option<String>,
}

/// A data type of the dialect.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SqlType {
    pub name: String,
    /// How SQL spells it.
    pub display: String,
    pub aliases: Vec<String>,
    /// The parameters it takes, such as `(precision, scale)`.
    pub params: Option<String>,
}

/// The keywords, functions and types of one dialect at one version.
#[derive(Clone, Debug, Default)]
pub struct DialectCatalog {
    /// The dialect's name, as the manifest gives it.
    pub name: String,
    /// The version the entries were resolved for; `None` for the built-in
    /// words.
    pub version: Option<String>,
    pub fit: Option<Fit>,
    keywords: HashMap<String, Keyword>,
    /// By lower-case name, sorted, so a prefix is a range.
    functions: BTreeMap<String, Vec<Function>>,
    /// By lower-case name and alias.
    types: HashMap<String, Arc<SqlType>>,
    type_count: usize,
}

/// The words the editor knew before catalogs: the keywords and functions
/// the tokenizer marks.
const BUILTIN_KEYWORDS: &[&str] = &[
    "select", "from", "where", "join", "left", "right", "inner", "outer", "on", "as", "and", "or",
    "not", "null", "is", "in", "between", "like", "group", "by", "order", "desc", "asc", "limit",
    "offset", "with", "distinct", "case", "when", "then", "else", "end", "having", "explain",
    "analyze",
];
const BUILTIN_FUNCTIONS: &[&str] = &[
    "count",
    "sum",
    "avg",
    "round",
    "max",
    "min",
    "coalesce",
    "date",
    "date_format",
    "date_trunc",
];

/// Whether `word` (lower case) is one of the clause words the editor always
/// shows as a keyword.
pub fn is_builtin_keyword(word: &str) -> bool {
    BUILTIN_KEYWORDS.contains(&word)
}

/// Whether `word` (lower case) is one of the functions the editor always
/// knows.
pub fn is_builtin_function(word: &str) -> bool {
    BUILTIN_FUNCTIONS.contains(&word)
}

impl DialectCatalog {
    /// The few words the editor always knows, used without a catalog. None
    /// of them is marked reserved, so nothing is quoted on their account.
    pub fn builtin() -> Self {
        let mut catalog = Self {
            name: String::new(),
            ..Self::default()
        };
        for word in BUILTIN_KEYWORDS {
            catalog.keywords.insert(
                (*word).to_owned(),
                Keyword {
                    word: (*word).to_owned(),
                    reserved: false,
                },
            );
        }
        for name in BUILTIN_FUNCTIONS {
            catalog.functions.insert(
                (*name).to_owned(),
                vec![Function {
                    name: (*name).to_owned(),
                    kind: FunctionKind::Scalar,
                    signature: format!("{name}(…)"),
                    returns: None,
                    summary: None,
                    doc: None,
                }],
            );
        }
        catalog
    }

    /// The keyword `word`, compared without case.
    pub fn keyword(&self, word: &str) -> Option<&Keyword> {
        self.keywords.get(&word.to_lowercase())
    }

    /// Whether `word` must be quoted to name a table or a column.
    pub fn is_reserved(&self, word: &str) -> bool {
        self.keyword(word).is_some_and(|keyword| keyword.reserved)
    }

    /// The overloads of function `name`, compared without case.
    pub fn functions(&self, name: &str) -> &[Function] {
        self.functions
            .get(&name.to_lowercase())
            .map_or(&[], Vec::as_slice)
    }

    /// Function names starting with `prefix` (compared without case), in
    /// name order, each with its overloads.
    pub fn functions_with_prefix<'a>(
        &'a self,
        prefix: &str,
    ) -> impl Iterator<Item = (&'a str, &'a [Function])> + 'a {
        let prefix = prefix.to_lowercase();
        self.functions
            .range(prefix.clone()..)
            .take_while(move |(name, _)| name.starts_with(&prefix))
            .map(|(name, overloads)| (name.as_str(), overloads.as_slice()))
    }

    /// The type named `name` or one of its aliases, compared without case.
    pub fn sql_type(&self, name: &str) -> Option<&SqlType> {
        self.types.get(&name.to_lowercase()).map(Arc::as_ref)
    }

    /// Keywords starting with `prefix` (compared without case), by word.
    pub fn keywords_with_prefix(&self, prefix: &str) -> Vec<&Keyword> {
        let prefix = prefix.to_lowercase();
        let mut found: Vec<&Keyword> = self
            .keywords
            .values()
            .filter(|keyword| keyword.word.starts_with(&prefix))
            .collect();
        found.sort_by(|a, b| a.word.cmp(&b.word));
        found
    }

    /// Types whose name starts with `prefix` (compared without case), once
    /// each, by name.
    pub fn types_with_prefix(&self, prefix: &str) -> Vec<&SqlType> {
        let prefix = prefix.to_lowercase();
        let mut found: Vec<&SqlType> = self
            .types
            .iter()
            .filter(|(name, sql_type)| **name == sql_type.name && name.starts_with(&prefix))
            .map(|(_, sql_type)| sql_type.as_ref())
            .collect();
        found.sort_by(|a, b| a.name.cmp(&b.name));
        found
    }

    pub fn keyword_count(&self) -> usize {
        self.keywords.len()
    }

    /// Distinct function names (overloads count once).
    pub fn function_count(&self) -> usize {
        self.functions.len()
    }

    pub fn type_count(&self) -> usize {
        self.type_count
    }
}

/// The catalogs of every dialect the application knows. A dialect whose
/// files were missing or invalid uses [`DialectCatalog::builtin`].
#[derive(Clone, Debug)]
pub struct SqlCatalogs {
    entries: Vec<(SqlDialect, Arc<DialectCatalog>)>,
    builtin: Arc<DialectCatalog>,
    /// The directory the catalogs were read from, when one was found.
    pub dir: Option<PathBuf>,
}

impl Default for SqlCatalogs {
    fn default() -> Self {
        Self {
            entries: Vec::new(),
            builtin: Arc::new(DialectCatalog::builtin()),
            dir: None,
        }
    }
}

impl SqlCatalogs {
    /// Catalogs read from `dir`, one per dialect that parsed.
    pub fn new(dir: Option<PathBuf>, entries: Vec<(SqlDialect, DialectCatalog)>) -> Self {
        Self {
            entries: entries
                .into_iter()
                .map(|(dialect, catalog)| (dialect, Arc::new(catalog)))
                .collect(),
            dir,
            ..Self::default()
        }
    }

    /// The catalog of `dialect`, or the built-in words for a tab without a
    /// connection or a dialect without a catalog.
    pub fn get(&self, dialect: Option<SqlDialect>) -> Arc<DialectCatalog> {
        dialect
            .and_then(|dialect| {
                self.entries
                    .iter()
                    .find(|(known, _)| *known == dialect)
                    .map(|(_, catalog)| catalog.clone())
            })
            .unwrap_or_else(|| self.builtin.clone())
    }

    /// The dialects that have a catalog, in [`DIALECTS`] order.
    pub fn loaded(&self) -> impl Iterator<Item = (SqlDialect, &DialectCatalog)> {
        DIALECTS.iter().filter_map(|(dialect, _)| {
            self.entries
                .iter()
                .find(|(known, _)| known == dialect)
                .map(|(dialect, catalog)| (*dialect, catalog.as_ref()))
        })
    }
}

/// Where to look for the catalog directory, in order: the
/// `JYKRE_SQL_CATALOG_DIR` variable, a `catalog` directory next to the
/// executable and, in a development build, the repository's `catalog`.
pub fn candidate_dirs(
    variable: Option<PathBuf>,
    executable_dir: Option<&Path>,
    repository: Option<&Path>,
) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    dirs.extend(variable.filter(|dir| !dir.as_os_str().is_empty()));
    dirs.extend(executable_dir.map(|dir| dir.join("catalog")));
    dirs.extend(repository.map(|dir| dir.join("catalog")));
    dirs
}

#[derive(Deserialize)]
struct Manifest {
    schema_version: u64,
    #[serde(default)]
    name: String,
    version_scheme: VersionScheme,
    versions: Vec<String>,
    verified_through: String,
    #[serde(default)]
    doc_base: Option<String>,
    #[serde(default = "one")]
    series_components: usize,
}

fn one() -> usize {
    1
}

#[derive(Deserialize)]
struct Validity {
    #[serde(default)]
    since: Option<String>,
    #[serde(default)]
    until: Option<String>,
}

#[derive(Deserialize)]
struct KeywordLine {
    word: String,
    reserved: bool,
    #[serde(flatten)]
    validity: Validity,
}

#[derive(Deserialize)]
struct FunctionLine {
    name: String,
    kind: String,
    signature: String,
    #[serde(default)]
    returns: Option<String>,
    #[serde(default)]
    summary: Option<String>,
    #[serde(default)]
    doc: Option<String>,
    #[serde(flatten)]
    validity: Validity,
}

#[derive(Deserialize)]
struct TypeLine {
    name: String,
    display: String,
    #[serde(default)]
    aliases: Vec<String>,
    #[serde(default)]
    params: Option<String>,
    #[serde(flatten)]
    validity: Validity,
}

/// Compares two versions of `scheme`; `None` when one does not follow it.
fn compare(scheme: VersionScheme, a: &str, b: &str) -> Option<Ordering> {
    match scheme {
        VersionScheme::Date => Some(a.cmp(b)),
        VersionScheme::Numeric => {
            let parse = |text: &str| {
                text.split('.')
                    .map(|part| part.parse::<u64>().ok())
                    .collect::<Option<Vec<_>>>()
            };
            let (a, b) = (parse(a)?, parse(b)?);
            let length = a.len().max(b.len());
            let at = |parts: &Vec<u64>, index| parts.get(index).copied().unwrap_or(0);
            Some(
                (0..length)
                    .map(|index| at(&a, index).cmp(&at(&b, index)))
                    .find(|order| order.is_ne())
                    .unwrap_or(Ordering::Equal),
            )
        }
    }
}

/// The tracked version whose entries apply to `server`
/// ([specification §4.4](../../../docs/especificacao-catalogo-sql.md)): the
/// greatest of `versions` not after it, the first for an older server and the
/// last for a newer one or none given.
pub fn resolve_version(
    scheme: VersionScheme,
    versions: &[String],
    verified_through: &str,
    server: Option<&str>,
) -> Option<(String, Fit)> {
    let last = versions.last()?.clone();
    let Some(server) = server else {
        return Some((last, Fit::Latest));
    };
    let first = versions.first()?;
    if compare(scheme, server, first)? == Ordering::Less {
        return Some((first.clone(), Fit::Older));
    }
    let fit = if compare(scheme, server, verified_through)? == Ordering::Greater {
        Fit::Newer
    } else {
        Fit::Covered
    };
    let version = versions
        .iter()
        .rev()
        .find(|version| compare(scheme, version, server).is_some_and(Ordering::is_le))?;
    Some((version.clone(), fit))
}

/// Whether an entry is valid at `version`: `since ≤ version < until`.
fn valid(scheme: VersionScheme, validity: &Validity, version: &str) -> Option<bool> {
    let after_since = match &validity.since {
        Some(since) => compare(scheme, since, version)?.is_le(),
        None => true,
    };
    let before_until = match &validity.until {
        Some(until) => compare(scheme, version, until)?.is_lt(),
        None => true,
    };
    Some(after_since && before_until)
}

/// The entries of a JSON Lines file valid at `version`.
fn entries<T: for<'de> Deserialize<'de>>(
    text: &str,
    file: &'static str,
    scheme: VersionScheme,
    version: &str,
    validity: impl Fn(&T) -> &Validity,
) -> Result<Vec<T>, CatalogError> {
    let invalid = |line: usize, reason: String| CatalogError::Invalid { file, line, reason };
    let mut found = Vec::new();
    for (index, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        if line.len() > MAX_LINE_BYTES {
            return Err(invalid(index + 1, "line too long".into()));
        }
        let entry: T =
            serde_json::from_str(line).map_err(|error| invalid(index + 1, error.to_string()))?;
        match valid(scheme, validity(&entry), version) {
            Some(true) => found.push(entry),
            Some(false) => {}
            None => return Err(invalid(index + 1, "version outside the scheme".into())),
        }
    }
    Ok(found)
}

/// The kind of a function as the catalog writes it; a kind this build does
/// not know is shown as a scalar function.
fn function_kind(kind: &str) -> FunctionKind {
    match kind {
        "aggregate" => FunctionKind::Aggregate,
        "window" => FunctionKind::Window,
        "table" => FunctionKind::Table,
        "procedure" => FunctionKind::Procedure,
        _ => FunctionKind::Scalar,
    }
}

/// The documentation URL of an entry: absolute as written, or relative to
/// the manifest's `doc_base`, whose `{series}` is the version's first
/// `series_components` parts.
fn doc_url(manifest: &Manifest, version: &str, doc: Option<String>) -> Option<String> {
    let doc = doc?;
    if doc.starts_with("https://") || doc.starts_with("http://") {
        return Some(doc);
    }
    let base = manifest.doc_base.as_ref()?;
    let series = match manifest.version_scheme {
        VersionScheme::Numeric => version
            .split('.')
            .take(manifest.series_components.max(1))
            .collect::<Vec<_>>()
            .join("."),
        VersionScheme::Date => String::new(),
    };
    Some(format!("{}{doc}", base.replace("{series}", &series)))
}

/// Parses one dialect directory's files and keeps the entries valid for the
/// server's version (or the latest tracked one when it is unknown).
pub fn parse(
    files: DialectFiles<'_>,
    server: Option<&str>,
) -> Result<DialectCatalog, CatalogError> {
    let manifest: Manifest =
        serde_json::from_str(files.manifest).map_err(|error| CatalogError::Invalid {
            file: FILES[0],
            line: 0,
            reason: error.to_string(),
        })?;
    if manifest.schema_version != SCHEMA_VERSION {
        return Err(CatalogError::Schema(manifest.schema_version));
    }
    let scheme = manifest.version_scheme;
    let (version, fit) = resolve_version(
        scheme,
        &manifest.versions,
        &manifest.verified_through,
        server,
    )
    .ok_or_else(|| CatalogError::Invalid {
        file: FILES[0],
        line: 0,
        reason: "no versions, or a version outside the scheme".into(),
    })?;
    let mut catalog = DialectCatalog {
        name: manifest.name.clone(),
        version: Some(version.clone()),
        fit: Some(fit),
        ..DialectCatalog::default()
    };
    for line in entries(
        files.keywords,
        FILES[1],
        scheme,
        &version,
        |line: &KeywordLine| &line.validity,
    )? {
        let word = line.word.to_lowercase();
        catalog.keywords.insert(
            word.clone(),
            Keyword {
                word,
                reserved: line.reserved,
            },
        );
    }
    for line in entries(
        files.functions,
        FILES[2],
        scheme,
        &version,
        |line: &FunctionLine| &line.validity,
    )? {
        let name = line.name.to_lowercase();
        let function = Function {
            name: name.clone(),
            kind: function_kind(&line.kind),
            signature: line.signature,
            returns: line.returns,
            summary: line.summary,
            doc: doc_url(&manifest, &version, line.doc),
        };
        catalog.functions.entry(name).or_default().push(function);
    }
    for line in entries(
        files.types,
        FILES[3],
        scheme,
        &version,
        |line: &TypeLine| &line.validity,
    )? {
        let sql_type = Arc::new(SqlType {
            name: line.name.to_lowercase(),
            display: line.display,
            aliases: line.aliases,
            params: line.params,
        });
        catalog.type_count += 1;
        let names = std::iter::once(sql_type.name.clone())
            .chain(sql_type.aliases.iter().map(|alias| alias.to_lowercase()))
            .collect::<Vec<_>>();
        for name in names {
            catalog
                .types
                .entry(name)
                .or_insert_with(|| sql_type.clone());
        }
    }
    Ok(catalog)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MANIFEST: &str = r#"{
        "schema_version": 1,
        "dialect": "postgresql",
        "name": "PostgreSQL",
        "version_scheme": "numeric",
        "versions": ["14", "15", "16", "17", "18"],
        "verified_through": "18.6",
        "series_components": 1,
        "doc_base": "https://www.postgresql.org/docs/{series}/",
        "a_field_from_a_later_format": true
    }"#;

    const KEYWORDS: &str = concat!(
        r#"{"category":"reserved","reserved":true,"source":"kwlist.h","word":"order"}"#,
        "\n",
        r#"{"bare_label":true,"category":"col_name","reserved":false,"since":"17","source":"kwlist.h","word":"json_table"}"#,
        "\n",
        r#"{"reserved":false,"source":"kwlist.h","until":"16","word":"gone_in_16"}"#,
        "\n"
    );

    const FUNCTIONS: &str = concat!(
        r#"{"args":[{"type":"text"},{"type":"timestamptz"}],"doc":"functions-datetime.html","id":"date_trunc(text,timestamptz)","kind":"scalar","name":"date_trunc","returns":"timestamptz","signature":"date_trunc(text, timestamp with time zone) → timestamp with time zone","source":"pg_proc.dat"}"#,
        "\n",
        r#"{"id":"date_trunc(text,interval)","kind":"scalar","name":"date_trunc","returns":"interval","signature":"date_trunc(text, interval) → interval","source":"pg_proc.dat","summary":"truncate interval to specified units","summary_source":"pg_proc.dat"}"#,
        "\n",
        r#"{"id":"datediff(a,b)","kind":"scalar","name":"datediff","signature":"datediff(a, b)","source":"x"}"#,
        "\n",
        r#"{"id":"string_agg(text,text)","kind":"aggregate","name":"string_agg","signature":"string_agg(text, text) → text","source":"pg_proc.dat"}"#,
        "\n"
    );

    const TYPES: &str = concat!(
        r#"{"aliases":["timestamp with time zone"],"display":"timestamp with time zone","name":"timestamptz","params":"(precision)","source":"pg_type.dat"}"#,
        "\n"
    );

    fn files<'a>(manifest: &'a str, keywords: &'a str) -> DialectFiles<'a> {
        DialectFiles {
            manifest,
            keywords,
            functions: FUNCTIONS,
            types: TYPES,
        }
    }

    #[test]
    fn versions_resolve_to_the_greatest_tracked_one_not_after_the_server() {
        let versions: Vec<String> = ["14", "15", "16", "17", "18"].map(String::from).into();
        let resolve = |server| resolve_version(VersionScheme::Numeric, &versions, "18.6", server);
        assert_eq!(resolve(Some("18.6")), Some(("18".into(), Fit::Covered)));
        assert_eq!(resolve(Some("16.2")), Some(("16".into(), Fit::Covered)));
        assert_eq!(resolve(Some("13.9")), Some(("14".into(), Fit::Older)));
        assert_eq!(resolve(Some("19.0")), Some(("18".into(), Fit::Newer)));
        assert_eq!(resolve(None), Some(("18".into(), Fit::Latest)));
        assert_eq!(resolve(Some("not a version")), None);
        let dates: Vec<String> = ["2026-05-01", "2026-11-04"].map(String::from).into();
        assert_eq!(
            resolve_version(
                VersionScheme::Date,
                &dates,
                "2026-11-04",
                Some("2026-06-30")
            ),
            Some(("2026-05-01".into(), Fit::Covered))
        );
        assert_eq!(
            compare(VersionScheme::Numeric, "9.7", "9.7.0"),
            Some(Ordering::Equal)
        );
        assert_eq!(
            compare(VersionScheme::Numeric, "9.6", "10"),
            Some(Ordering::Less)
        );
    }

    #[test]
    fn entries_are_kept_only_inside_their_validity() {
        let latest = parse(files(MANIFEST, KEYWORDS), None).unwrap();
        assert_eq!(latest.version.as_deref(), Some("18"));
        assert!(latest.keyword("JSON_TABLE").is_some());
        assert!(latest.keyword("gone_in_16").is_none());
        assert!(latest.is_reserved("Order"));
        assert!(!latest.is_reserved("json_table"));
        let old = parse(files(MANIFEST, KEYWORDS), Some("15.4")).unwrap();
        assert_eq!(old.fit, Some(Fit::Covered));
        assert!(old.keyword("json_table").is_none());
        assert!(old.keyword("gone_in_16").is_some());
    }

    #[test]
    fn functions_keep_their_overloads_and_resolve_their_documentation() {
        let catalog = parse(files(MANIFEST, KEYWORDS), Some("16")).unwrap();
        let overloads = catalog.functions("DATE_TRUNC");
        assert_eq!(overloads.len(), 2);
        assert_eq!(
            overloads[0].doc.as_deref(),
            Some("https://www.postgresql.org/docs/16/functions-datetime.html")
        );
        assert_eq!(
            overloads[1].summary.as_deref(),
            Some("truncate interval to specified units")
        );
        let names: Vec<&str> = catalog
            .functions_with_prefix("dat")
            .map(|(name, _)| name)
            .collect();
        assert_eq!(names, ["date_trunc", "datediff"]);
        assert_eq!(catalog.function_count(), 3);
        assert_eq!(
            catalog.functions("string_agg")[0].kind,
            FunctionKind::Aggregate
        );
        let timestamptz = catalog.sql_type("Timestamp With Time Zone").unwrap();
        assert_eq!(timestamptz.name, "timestamptz");
        assert_eq!(catalog.type_count(), 1);
    }

    #[test]
    fn a_later_format_or_a_broken_line_is_refused() {
        let later = MANIFEST.replace("\"schema_version\": 1", "\"schema_version\": 2");
        assert_eq!(
            parse(files(&later, KEYWORDS), None).unwrap_err(),
            CatalogError::Schema(2)
        );
        let broken = format!("{KEYWORDS}{{\"word\": \n");
        assert!(matches!(
            parse(files(MANIFEST, &broken), None),
            Err(CatalogError::Invalid {
                file: "keywords.jsonl",
                line: 4,
                ..
            })
        ));
        let long = format!(
            "{{\"word\":\"{}\",\"reserved\":false}}\n",
            "a".repeat(MAX_LINE_BYTES)
        );
        assert!(parse(files(MANIFEST, &long), None).is_err());
        let outside = r#"{"reserved":false,"since":"seventeen","word":"x"}"#;
        assert!(parse(files(MANIFEST, outside), None).is_err());
    }

    #[test]
    fn a_dialect_without_a_catalog_keeps_the_builtin_words() {
        let catalogs = SqlCatalogs::new(
            Some(PathBuf::from("catalog")),
            vec![(
                SqlDialect::Postgres,
                parse(files(MANIFEST, KEYWORDS), None).unwrap(),
            )],
        );
        assert_eq!(
            catalogs.get(Some(SqlDialect::Postgres)).version.as_deref(),
            Some("18")
        );
        let mysql = catalogs.get(Some(SqlDialect::MySql));
        assert!(mysql.version.is_none());
        assert!(mysql.keyword("select").is_some());
        assert!(!mysql.is_reserved("select"));
        assert!(catalogs.get(None).keyword("from").is_some());
        assert_eq!(
            catalogs
                .loaded()
                .map(|(dialect, _)| dialect)
                .collect::<Vec<_>>(),
            [SqlDialect::Postgres]
        );
    }

    #[test]
    fn the_catalog_directory_is_looked_for_in_order() {
        let dirs = candidate_dirs(
            Some(PathBuf::from("/data/catalogs")),
            Some(Path::new("/opt/jykre")),
            Some(Path::new("/src/jykre")),
        );
        assert_eq!(
            dirs,
            [
                PathBuf::from("/data/catalogs"),
                Path::new("/opt/jykre").join("catalog"),
                Path::new("/src/jykre").join("catalog"),
            ]
        );
        assert_eq!(
            candidate_dirs(Some(PathBuf::new()), None, None),
            Vec::<PathBuf>::new()
        );
    }

    /// The catalog shipped next to the application parses for every dialect.
    #[test]
    fn the_repository_catalog_parses() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../catalog");
        for (dialect, name) in DIALECTS {
            let read = |file: &str| std::fs::read_to_string(root.join(name).join(file)).unwrap();
            let texts = FILES.map(read);
            let parsed = parse(
                DialectFiles {
                    manifest: &texts[0],
                    keywords: &texts[1],
                    functions: &texts[2],
                    types: &texts[3],
                },
                None,
            );
            assert!(parsed.is_ok(), "{dialect:?}: {:?}", parsed.as_ref().err());
            let catalog = parsed.unwrap();
            assert!(catalog.keyword_count() > 50, "{dialect:?} keywords");
            assert!(catalog.function_count() > 50, "{dialect:?} functions");
        }
    }
}
