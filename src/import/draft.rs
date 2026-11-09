//! **New table from this file…** (decision I-2): a draft `CREATE TABLE`
//! built from the file's header, opened in a new query tab for the user to
//! edit and run before importing. The import itself never creates a table.
//!
//! Names are quoted by the driver's rules (golden rule 3); every column is
//! the dialect's text type, since the file says nothing reliable about
//! types and the user chooses them.
use crate::drivers::{Driver, ObjectKind, ObjectPath, SqlDialect};
use crate::tr;

/// The text type a draft gives every column, by dialect.
pub fn text_type(dialect: SqlDialect) -> &'static str {
    match dialect {
        SqlDialect::DatabricksSql => "STRING",
        // PostgreSQL, MySQL and SQLite, and a dialect added later until it
        // says otherwise.
        _ => "TEXT",
    }
}

/// The names a draft gives the file's `columns`: the header's, with
/// `column_N` for an empty one and `_2`, `_3`… after a name already taken
/// (ignoring case, as most databases compare them).
pub fn column_names(columns: &[String]) -> Vec<String> {
    let mut names: Vec<String> = Vec::with_capacity(columns.len());
    for (index, column) in columns.iter().enumerate() {
        let base = if column.trim().is_empty() {
            format!("column_{}", index + 1)
        } else {
            column.clone()
        };
        let taken = |candidate: &str, names: &[String]| {
            names
                .iter()
                .any(|name| name.to_lowercase() == candidate.to_lowercase())
        };
        let mut name = base.clone();
        let mut suffix = 2;
        while taken(&name, &names) {
            name = format!("{base}_{suffix}");
            suffix += 1;
        }
        names.push(name);
    }
    names
}

/// The name a draft proposes for the file `stem` (its name without the
/// extension), `imported` when it has none.
pub fn wanted_name(stem: &str) -> String {
    let stem = stem.trim();
    if stem.is_empty() {
        "imported".into()
    } else {
        stem.into()
    }
}

/// The table name a draft proposes for the file `stem`: [`wanted_name`],
/// followed by `_2`, `_3`… while one of `taken` (the relations the metadata
/// cache knows in the draft's namespace) has it, ignoring case as most
/// databases compare names. The cache may be behind the database, so the
/// draft stays a proposal the user edits.
pub fn table_name(stem: &str, taken: &[&str]) -> String {
    let base = wanted_name(stem);
    let is_taken = |candidate: &str| {
        let candidate = candidate.to_lowercase();
        taken.iter().any(|name| name.to_lowercase() == candidate)
    };
    let mut name = base.clone();
    let mut suffix = 2;
    while is_taken(&name) {
        name = format!("{base}_{suffix}");
        suffix += 1;
    }
    name
}

/// `CREATE TABLE` for a table named `table` in `namespace` with the file's
/// `columns`, every one of the dialect's text type, quoted by `driver`. A
/// comment first says that the types are the user's to choose and, when
/// the file's name was taken (`instead_of`), which table already has it.
/// Nothing in it comes from the file but the names, which are quoted.
pub fn create_table(
    driver: &dyn Driver,
    namespace: &ObjectPath,
    table: &str,
    columns: &[String],
    instead_of: Option<&str>,
) -> String {
    let descriptor = driver.descriptor();
    let kind = text_type(descriptor.dialect);
    let relation = driver.relation_name(&namespace.child(ObjectKind::Table, table));
    let mut sql = String::new();
    // A line break in a translation, or in a name, would end the comment.
    let mut comment = |text: String| {
        sql.push_str("-- ");
        sql.push_str(&text.replace(['\r', '\n'], " "));
        sql.push('\n');
    };
    comment(
        tr!(
            "Every column is text. Choose each column's type, then run this before importing the file."
        )
        .into(),
    );
    if let Some(taken) = instead_of {
        comment(tr!(
            "A table named {0} already exists here, so this draft names the new one {1}.",
            taken,
            table
        ));
    }
    sql.push_str("CREATE TABLE ");
    sql.push_str(&relation);
    sql.push_str(" (\n");
    let names = column_names(columns);
    let lines: Vec<String> = names
        .iter()
        .map(|name| format!("    {} {kind}", driver.quote_identifier(name)))
        .collect();
    sql.push_str(&lines.join(",\n"));
    sql.push_str("\n);\n");
    sql
}
