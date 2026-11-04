//! What the SQL editor suggests, explains and opens at the cursor, from a
//! connection's cached metadata. No window and no server: the editor's
//! providers ask this module what the SQL around the cursor means, and read
//! the names it needs from a [`NameIndex`] built off the UI thread.
use crate::catalog::DialectCatalog;
use jykre_core::{MetadataObject, ObjectKind, ObjectPath, SessionContext, SqlDialect};
use jykre_sql::names::{Name, Part, name_at, parts, stored};
use scopes::{Binding, Scopes};
use std::collections::HashMap;
use std::ops::Range;

mod scopes;

/// A relation the cache holds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Relation {
    pub path: ObjectPath,
    pub kind: ObjectKind,
}

impl Relation {
    pub fn name(&self) -> &str {
        self.path.0.last().map_or("", |part| part.name.as_str())
    }
}

/// The names of a connection's cached relations, grouped by namespace. It
/// never changes once built: a new cache version builds a new index, so a
/// tab can keep using the one it has while the next is read.
#[derive(Clone, Debug, Default)]
pub struct NameIndex {
    /// The cache version the names were read from.
    pub version: i64,
    levels: Vec<ObjectKind>,
    dialect: Option<SqlDialect>,
    relations: HashMap<ObjectPath, Vec<Relation>>,
    namespaces: Vec<ObjectPath>,
}

impl NameIndex {
    /// Groups `relations` (paths of tables, views and materialized views)
    /// by their last-level namespace.
    pub fn new(
        version: i64,
        levels: Vec<ObjectKind>,
        dialect: SqlDialect,
        relations: Vec<Relation>,
    ) -> Self {
        let mut by_namespace: HashMap<ObjectPath, Vec<Relation>> = HashMap::new();
        for relation in relations {
            if relation.path.0.len() != levels.len() + 1 {
                continue;
            }
            if let Some(namespace) = relation.path.parent() {
                by_namespace.entry(namespace).or_default().push(relation);
            }
        }
        for relations in by_namespace.values_mut() {
            relations.sort_by(|a, b| a.name().cmp(b.name()));
        }
        let mut namespaces: Vec<_> = by_namespace.keys().cloned().collect();
        namespaces.sort_by(|a, b| {
            a.0.iter()
                .map(|part| &part.name)
                .cmp(b.0.iter().map(|part| &part.name))
        });
        Self {
            version,
            levels,
            dialect: Some(dialect),
            relations: by_namespace,
            namespaces,
        }
    }

    fn dialect(&self) -> SqlDialect {
        self.dialect.unwrap_or(SqlDialect::Postgres)
    }

    /// The relation a written name refers to, resolving unqualified parts in
    /// `context`.
    pub fn resolve(&self, name: &[Name], context: &SessionContext) -> Option<&Relation> {
        let (relation, qualifier) = name.split_last()?;
        let namespace = self.namespace(qualifier, context)?;
        let wanted = stored(relation, self.dialect());
        self.relations
            .get(&namespace)?
            .iter()
            .find(|candidate| candidate.name() == wanted)
    }

    /// The last-level namespace a qualifier names, filling the levels it
    /// leaves out from `context`.
    fn namespace(&self, qualifier: &[Name], context: &SessionContext) -> Option<ObjectPath> {
        let written: Vec<String> = qualifier
            .iter()
            .map(|name| stored(name, self.dialect()))
            .collect();
        let levels = self.levels.len();
        let filled = [context.catalog.clone(), context.schema.clone()];
        let known = &filled[filled.len().saturating_sub(levels)..];
        let take = written.len().min(levels);
        let mut names = Vec::with_capacity(levels);
        for value in &known[..levels - take] {
            names.push(value.clone()?);
        }
        names.extend(written[written.len() - take..].iter().cloned());
        Some(
            self.levels
                .iter()
                .zip(names)
                .fold(ObjectPath::default(), |path, (kind, name)| {
                    path.child(*kind, name)
                }),
        )
    }

    fn relations_in(&self, namespace: &ObjectPath) -> &[Relation] {
        self.relations.get(namespace).map_or(&[], Vec::as_slice)
    }
}

/// What the cursor asks for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Wanted {
    /// Relations, namespaces and the statement's CTEs, after `FROM` or
    /// `JOIN`; inside `qualifier`'s namespace when one is written.
    Relations { qualifier: Vec<Name> },
    /// Columns of these relations: after `alias.`, the relation it names
    /// (a derived table's relations); otherwise those of the query around
    /// the cursor (the statement, a subquery, a CTE body or a `UNION`
    /// branch). Where nothing qualifies the name it may also be a function
    /// or a keyword.
    Columns {
        relations: Vec<ObjectPath>,
        qualified: bool,
    },
    /// A data type, after `::`.
    Types,
}

/// A completion request: what to suggest and the text it replaces.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Request {
    /// Byte range of the partial word in the whole text.
    pub replace: Range<usize>,
    pub prefix: String,
    pub wanted: Wanted,
    /// The statement's CTE names, suggested with relations.
    ctes: Vec<String>,
}

/// One suggestion.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Candidate {
    pub label: String,
    /// What is inserted: the name, quoted when the dialect needs it.
    pub insert: String,
    /// The kind of object, a column's type or a function's signature.
    pub detail: String,
    /// More about it, in Markdown (a function's description and page).
    pub documentation: Option<String>,
    pub kind: CandidateKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CandidateKind {
    Namespace,
    Relation,
    View,
    Column,
    Cte,
    Keyword,
    Function,
    Type,
}

/// Words that end a relation reference or cannot be an alias.
const CLAUSES: &[&str] = &[
    "WHERE",
    "JOIN",
    "ON",
    "USING",
    "LEFT",
    "RIGHT",
    "INNER",
    "OUTER",
    "FULL",
    "CROSS",
    "NATURAL",
    "GROUP",
    "ORDER",
    "LIMIT",
    "OFFSET",
    "HAVING",
    "WINDOW",
    "UNION",
    "EXCEPT",
    "INTERSECT",
    "SET",
    "VALUES",
    "SELECT",
    "FROM",
    "AS",
    "WITH",
    "RETURNING",
    "QUALIFY",
    "LATERAL",
    "FOR",
    "INTO",
];

/// Words after which the next name is a relation.
const RELATION_SLOTS: &[&str] = &["FROM", "JOIN", "UPDATE", "INTO", "TABLE", "DESCRIBE"];

fn word_of(part: &Part) -> Option<&str> {
    match part {
        Part::Word(upper, _) => Some(upper),
        _ => None,
    }
}

fn is_symbol(part: Option<&Part>, symbol: &str) -> bool {
    matches!(part, Some(Part::Symbol(text)) if text == symbol)
}

/// The cache's relations that `bindings` stand for, each once and in order.
/// CTEs and names the cache does not hold are left out.
fn resolved<'a>(
    bindings: impl Iterator<Item = &'a Binding>,
    scopes: &Scopes,
    index: &NameIndex,
    context: &SessionContext,
) -> Vec<ObjectPath> {
    let mut paths: Vec<ObjectPath> = Vec::new();
    for name in bindings.flat_map(|binding| &binding.names) {
        if scopes.is_cte(name) {
            continue;
        }
        if let Some(relation) = index.resolve(name, context)
            && !paths.contains(&relation.path)
        {
            paths.push(relation.path.clone());
        }
    }
    paths
}

/// The statement around `offset` and where it starts, split by the rules of
/// `dialect`. A cursor after the last word of a statement that no `;` ended,
/// with only spaces in between (`WHERE |`), still continues that statement.
fn statement_at(sql: &str, offset: usize, dialect: Option<SqlDialect>) -> Option<(usize, &str)> {
    let ranges = jykre_sql::statements(sql, dialect);
    if let Some(range) = ranges
        .iter()
        .find(|range| range.start <= offset && offset <= range.end)
    {
        return Some((range.start, &sql[range.clone()]));
    }
    let range = ranges.iter().rev().find(|range| range.end <= offset)?;
    let between = sql.get(range.end..offset)?;
    let ended = sql[range.clone()].ends_with(';');
    (!ended && between.trim().is_empty()).then(|| (range.start, &sql[range.start..offset]))
}

/// Parts of `statement` before byte `local`, with the partial word at the
/// cursor (unquoted words only) and where it starts.
fn before_cursor(
    statement: &str,
    local: usize,
    dialect: Option<SqlDialect>,
) -> (Vec<Part>, String, usize) {
    let head = &statement[..local];
    let start = head
        .char_indices()
        .rev()
        .take_while(|(_, ch)| ch.is_alphanumeric() || *ch == '_')
        .last()
        .map_or(local, |(index, _)| index);
    (
        parts(&head[..start], dialect),
        head[start..].to_owned(),
        start,
    )
}

/// The qualifier written right before the cursor (`a.b.` before `c|`).
fn qualifier(parts: &[Part]) -> Vec<Name> {
    let mut names = Vec::new();
    let mut at = parts.len();
    while at >= 2 && is_symbol(parts.get(at - 1), ".") {
        match &parts[at - 2] {
            Part::Word(_, text) => names.push(Name {
                text: text.clone(),
                quoted: false,
            }),
            Part::Quoted(text) => names.push(Name {
                text: text.clone(),
                quoted: true,
            }),
            _ => break,
        }
        at -= 2;
    }
    names.reverse();
    names
}

/// What the editor should suggest at `offset` in `sql`. `None` outside a
/// statement, inside a literal or comment, or where nothing fits.
pub fn request(
    sql: &str,
    offset: usize,
    index: &NameIndex,
    context: &SessionContext,
) -> Option<Request> {
    let offset = offset.min(sql.len());
    let (start, statement) = statement_at(sql, offset, index.dialect)?;
    let local = offset - start;
    if !statement.is_char_boundary(local) {
        return None;
    }
    let (before, prefix, prefix_start) = before_cursor(statement, local, index.dialect);
    // Inside a literal or comment there is nothing to suggest.
    if let Some(last) = jykre_sql::tokens(&statement[..local], index.dialect).last()
        && matches!(
            last.1,
            jykre_sql::TokenKind::String | jykre_sql::TokenKind::Comment
        )
        && last.0.end >= local
    {
        return None;
    }
    let qualifier = qualifier(&before);
    let slot_at = before.len() - qualifier.len() * 2;
    let slot = before[..slot_at].iter().rev().find_map(|part| match part {
        Part::Word(upper, _) => Some(upper.as_str()),
        Part::Symbol(symbol) if symbol == "," => Some(","),
        _ => None,
    });
    let scopes = Scopes::new(statement, index.dialect);
    let ctes = scopes.ctes().to_vec();
    let replace = start + prefix_start..offset;
    if qualifier.is_empty() && matches!(before.last(), Some(Part::Symbol(symbol)) if symbol == "::")
    {
        return Some(Request {
            replace,
            prefix,
            wanted: Wanted::Types,
            ctes,
        });
    }
    let relations_slot = slot.is_some_and(|slot| RELATION_SLOTS.contains(&slot))
        || (slot == Some(",") && in_from_list(&before[..slot_at]));
    if relations_slot {
        return Some(Request {
            replace,
            prefix,
            wanted: Wanted::Relations { qualifier },
            ctes,
        });
    }
    let relations = match qualifier.as_slice() {
        [] => resolved(scopes.visible(local), &scopes, index, context),
        [one] => {
            let wanted = stored(one, index.dialect()).to_lowercase();
            match scopes.qualified(local, &wanted) {
                Some(binding) => resolved(std::iter::once(binding), &scopes, index, context),
                // `schema.` outside FROM: relations of that namespace.
                None => {
                    return Some(Request {
                        replace,
                        prefix,
                        wanted: Wanted::Relations { qualifier },
                        ctes,
                    });
                }
            }
        }
        written => index
            .resolve(written, context)
            .map(|relation| vec![relation.path.clone()])
            .unwrap_or_default(),
    };
    Some(Request {
        replace,
        prefix,
        wanted: Wanted::Columns {
            relations,
            qualified: !qualifier.is_empty(),
        },
        ctes,
    })
}

/// Whether a comma at the end of `parts` continues a `FROM` list.
fn in_from_list(parts: &[Part]) -> bool {
    parts
        .iter()
        .rev()
        .find_map(|part| word_of(part).filter(|word| CLAUSES.contains(word) || *word == "UPDATE"))
        == Some("FROM")
}

/// Whether a name must be quoted to be read back as written: it has other
/// characters than letters, digits and `_`, PostgreSQL would fold its upper
/// case, or it is a word the dialect reserves (`order`, `user`).
fn needs_quotes(name: &str, dialect: SqlDialect, catalog: &DialectCatalog) -> bool {
    let simple = name
        .chars()
        .next()
        .is_some_and(|first| first.is_alphabetic() || first == '_')
        && name.chars().all(|ch| ch.is_alphanumeric() || ch == '_');
    let folded = match dialect {
        SqlDialect::Postgres => name.chars().any(char::is_uppercase),
        _ => false,
    };
    !simple || folded || catalog.is_reserved(name)
}

/// `name` as SQL writes it to be read back as written: quoted with `quote`
/// (the quote inside doubled) only when the dialect needs it, as the
/// suggestions insert it.
pub fn quote_name(
    name: &str,
    dialect: SqlDialect,
    quote: char,
    catalog: &DialectCatalog,
) -> String {
    quoted(name, dialect, quote, catalog)
}

fn quoted(name: &str, dialect: SqlDialect, quote: char, catalog: &DialectCatalog) -> String {
    if !needs_quotes(name, dialect, catalog) {
        return name.to_owned();
    }
    let doubled = name.replace(quote, &format!("{quote}{quote}"));
    format!("{quote}{doubled}{quote}")
}

fn matches_prefix(name: &str, prefix: &str) -> bool {
    name.to_lowercase().starts_with(&prefix.to_lowercase())
}

/// Where a candidate kind ranks for what is wanted: the most likely first.
fn kind_rank(wanted: &Wanted, kind: CandidateKind) -> u8 {
    match (wanted, kind) {
        (Wanted::Columns { .. }, CandidateKind::Column) => 0,
        (Wanted::Columns { .. }, CandidateKind::Function) => 1,
        (Wanted::Relations { .. }, CandidateKind::Cte) => 0,
        (Wanted::Relations { .. }, CandidateKind::Relation | CandidateKind::View) => 1,
        (Wanted::Relations { .. }, CandidateKind::Namespace) => 2,
        (_, CandidateKind::Keyword) => 3,
        _ => 2,
    }
}

/// Orders the suggestions: names that start with the prefix as typed
/// (letter case included) first, then by what is wanted; within that, as
/// found (columns keep their table order).
fn rank(found: &mut [Candidate], request: &Request) {
    found.sort_by_key(|candidate| {
        let exact = !candidate.label.starts_with(&request.prefix);
        (exact, kind_rank(&request.wanted, candidate.kind))
    });
}

/// A function's suggestion: its name, inserted with the opening parenthesis,
/// its first signature (with how many more it has) and its description.
fn function_candidate(name: &str, overloads: &[crate::catalog::Function]) -> Option<Candidate> {
    let first = overloads.first()?;
    let detail = match overloads.len() {
        1 => first.signature.clone(),
        more => format!("{} (+{})", first.signature, more - 1),
    };
    Some(Candidate {
        label: name.to_owned(),
        insert: format!("{name}("),
        detail,
        documentation: function_documentation(overloads),
        kind: CandidateKind::Function,
    })
}

/// A function's signatures, description and documentation page, in Markdown.
pub fn function_documentation(overloads: &[crate::catalog::Function]) -> Option<String> {
    let first = overloads.first()?;
    let mut text = String::new();
    for overload in overloads.iter().take(8) {
        text.push_str(&format!("`{}`\n\n", overload.signature));
    }
    if overloads.len() > 8 {
        text.push_str(&format!("… +{}\n\n", overloads.len() - 8));
    }
    if let Some(summary) = overloads
        .iter()
        .find_map(|overload| overload.summary.as_ref())
    {
        text.push_str(summary);
        text.push_str("\n\n");
    }
    if let Some(doc) = &first.doc {
        text.push_str(doc);
    }
    Some(text.trim_end().to_owned())
}

/// The suggestions for `request`, and the relations whose columns are not
/// read yet (shown as loading until they are). `columns` returns what was
/// read for a relation; `quote` is the dialect's identifier quote and
/// `catalog` its keywords, so a reserved word is quoted.
pub fn candidates(
    request: &Request,
    index: &NameIndex,
    context: &SessionContext,
    quote: char,
    catalog: &DialectCatalog,
    columns: &dyn Fn(&ObjectPath) -> Option<Vec<MetadataObject>>,
) -> (Vec<Candidate>, Vec<ObjectPath>) {
    let dialect = index.dialect();
    let mut found = Vec::new();
    let mut missing = Vec::new();
    match &request.wanted {
        Wanted::Relations { qualifier } => {
            if let Some(namespace) = index.namespace(qualifier, context) {
                for relation in index.relations_in(&namespace) {
                    if matches_prefix(relation.name(), &request.prefix) {
                        found.push(Candidate {
                            label: relation.name().to_owned(),
                            insert: quoted(relation.name(), dialect, quote, catalog),
                            detail: kind_detail(relation.kind),
                            documentation: None,
                            kind: if relation.kind == ObjectKind::Table {
                                CandidateKind::Relation
                            } else {
                                CandidateKind::View
                            },
                        });
                    }
                }
            }
            if qualifier.is_empty() {
                for cte in &request.ctes {
                    if matches_prefix(cte, &request.prefix) {
                        found.push(Candidate {
                            label: cte.clone(),
                            insert: cte.clone(),
                            detail: "CTE".into(),
                            documentation: None,
                            kind: CandidateKind::Cte,
                        });
                    }
                }
                // Namespaces, to qualify the name.
                for namespace in &index.namespaces {
                    let Some(name) = namespace.0.last().map(|part| part.name.as_str()) else {
                        continue;
                    };
                    let parent_matches = index
                        .namespace(&[], context)
                        .is_none_or(|current| current.parent() == namespace.parent());
                    if parent_matches && matches_prefix(name, &request.prefix) {
                        found.push(Candidate {
                            label: name.to_owned(),
                            insert: quoted(name, dialect, quote, catalog),
                            detail: namespace
                                .0
                                .last()
                                .map(|part| kind_detail(part.kind))
                                .unwrap_or_default(),
                            documentation: None,
                            kind: CandidateKind::Namespace,
                        });
                    }
                }
            }
        }
        Wanted::Columns {
            relations,
            qualified,
        } => {
            for relation in relations {
                match columns(relation) {
                    Some(objects) => {
                        for column in objects
                            .iter()
                            .filter(|object| object.kind == ObjectKind::Column)
                        {
                            if matches_prefix(&column.name, &request.prefix)
                                && !found
                                    .iter()
                                    .any(|candidate: &Candidate| candidate.label == column.name)
                            {
                                found.push(Candidate {
                                    label: column.name.clone(),
                                    insert: quoted(&column.name, dialect, quote, catalog),
                                    detail: column.data_type.clone().unwrap_or_default(),
                                    documentation: None,
                                    kind: CandidateKind::Column,
                                });
                            }
                        }
                    }
                    None => missing.push(relation.clone()),
                }
            }
            // Without a qualifier, the expression may be a function call or
            // a keyword (`WHERE`, `CASE`).
            if !qualified && !request.prefix.is_empty() {
                for (name, overloads) in catalog.functions_with_prefix(&request.prefix) {
                    found.extend(function_candidate(name, overloads));
                }
                for keyword in catalog.keywords_with_prefix(&request.prefix) {
                    if found
                        .iter()
                        .any(|candidate| candidate.label.eq_ignore_ascii_case(&keyword.word))
                    {
                        continue;
                    }
                    found.push(Candidate {
                        label: keyword.word.to_uppercase(),
                        insert: keyword.word.to_uppercase(),
                        detail: "keyword".into(),
                        documentation: None,
                        kind: CandidateKind::Keyword,
                    });
                }
            }
        }
        Wanted::Types => {
            for sql_type in catalog.types_with_prefix(&request.prefix) {
                found.push(Candidate {
                    label: sql_type.name.clone(),
                    insert: sql_type.name.clone(),
                    detail: match &sql_type.params {
                        Some(params) => format!("{} {params}", sql_type.display),
                        None => sql_type.display.clone(),
                    },
                    documentation: None,
                    kind: CandidateKind::Type,
                });
            }
        }
    }
    rank(&mut found, request);
    (found, missing)
}

/// The built-in function whose name is under `offset` (a word followed by
/// `(` that the catalog knows), lower case, and the word's byte range.
pub fn function_at(
    sql: &str,
    offset: usize,
    dialect: Option<SqlDialect>,
    catalog: &DialectCatalog,
) -> Option<(String, Range<usize>)> {
    let offset = offset.min(sql.len());
    let (start, statement) = statement_at(sql, offset, dialect)?;
    let local = offset - start;
    let tokens: Vec<_> = jykre_sql::tokens(statement, dialect)
        .into_iter()
        .filter(|(_, kind)| *kind != jykre_sql::TokenKind::Comment)
        .collect();
    let at = tokens
        .iter()
        .position(|(range, _)| range.start <= local && local <= range.end)?;
    let (range, kind) = &tokens[at];
    let next = tokens.get(at + 1).map(|(next, _)| &statement[next.clone()]);
    if *kind != jykre_sql::TokenKind::Word || next != Some("(") {
        return None;
    }
    let name = statement[range.clone()].to_lowercase();
    (!catalog.functions(&name).is_empty()).then(|| (name, start + range.start..start + range.end))
}

fn kind_detail(kind: ObjectKind) -> String {
    match kind {
        ObjectKind::Table => "table",
        ObjectKind::View => "view",
        ObjectKind::MaterializedView => "materialized view",
        ObjectKind::Schema => "schema",
        ObjectKind::Database => "database",
        ObjectKind::Catalog => "catalog",
        _ => "",
    }
    .to_owned()
}

/// What a name under the cursor refers to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Symbol {
    Relation(ObjectPath),
    /// A column of `relation`, by name.
    Column {
        relation: ObjectPath,
        name: String,
    },
}

impl Symbol {
    /// The path of the object in the cache, where the explorer finds it.
    pub fn path(&self) -> ObjectPath {
        match self {
            Self::Relation(path) => path.clone(),
            Self::Column { relation, name } => relation.child(ObjectKind::Column, name.clone()),
        }
    }
}

/// The relation or column the name at `offset` refers to, and its byte
/// range. Columns written without a qualifier resolve in the statement's
/// relations, as `columns` knows them.
pub fn symbol_at(
    sql: &str,
    offset: usize,
    index: &NameIndex,
    context: &SessionContext,
    columns: &dyn Fn(&ObjectPath) -> Option<Vec<MetadataObject>>,
) -> Option<(Symbol, Range<usize>)> {
    let offset = offset.min(sql.len());
    let (start, statement) = statement_at(sql, offset, index.dialect)?;
    let local = offset - start;
    let all = parts(statement, index.dialect);
    // The dotted name around the cursor, from the token ranges.
    let tokens: Vec<_> = jykre_sql::tokens(statement, index.dialect)
        .into_iter()
        .filter(|(_, kind)| *kind != jykre_sql::TokenKind::Comment)
        .collect();
    let at = tokens
        .iter()
        .position(|(range, _)| range.start <= local && local <= range.end)?;
    if !matches!(all.get(at), Some(Part::Word(..) | Part::Quoted(_))) {
        return None;
    }
    let mut first = at;
    while first >= 2 && is_symbol(all.get(first - 1), ".") {
        first -= 2;
    }
    let (name, _) = name_at(&all, first)?;
    let upto = (at - first) / 2 + 1;
    let name = &name[..upto.min(name.len())];
    let range = start + tokens[at].0.start..start + tokens[at].0.end;
    if let Some(relation) = index.resolve(name, context) {
        // A relation, unless it is the statement's alias or column.
        return Some((Symbol::Relation(relation.path.clone()), range));
    }
    let scopes = Scopes::new(statement, index.dialect);
    let (column, qualifier) = name.split_last()?;
    let column = stored(column, index.dialect());
    let candidates: Vec<ObjectPath> = match qualifier {
        // The closest query that has the column, as a correlated subquery
        // reads it.
        [] => resolved(scopes.outwards(local), &scopes, index, context),
        [one] => {
            let wanted = stored(one, index.dialect()).to_lowercase();
            scopes
                .qualified(local, &wanted)
                .map(|binding| resolved(std::iter::once(binding), &scopes, index, context))
                .unwrap_or_default()
        }
        written => index
            .resolve(written, context)
            .map(|relation| vec![relation.path.clone()])
            .unwrap_or_default(),
    };
    candidates.into_iter().find_map(|relation| {
        columns(&relation)?
            .iter()
            .any(|object| object.kind == ObjectKind::Column && object.name == column)
            .then(|| {
                (
                    Symbol::Column {
                        relation,
                        name: column.clone(),
                    },
                    range.clone(),
                )
            })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn schema(name: &str) -> ObjectPath {
        ObjectPath::default()
            .child(ObjectKind::Database, "shop")
            .child(ObjectKind::Schema, name)
    }

    fn index() -> NameIndex {
        let relation = |namespace: &str, name: &str, kind| Relation {
            path: schema(namespace).child(kind, name),
            kind,
        };
        NameIndex::new(
            3,
            vec![ObjectKind::Database, ObjectKind::Schema],
            SqlDialect::Postgres,
            vec![
                relation("sales", "orders", ObjectKind::Table),
                relation("sales", "order_lines", ObjectKind::Table),
                relation("sales", "Totals", ObjectKind::View),
                relation("crm", "customers", ObjectKind::Table),
            ],
        )
    }

    fn context() -> SessionContext {
        SessionContext {
            catalog: Some("shop".into()),
            schema: Some("sales".into()),
        }
    }

    fn column(relation: &ObjectPath, name: &str, data_type: &str) -> MetadataObject {
        let mut column = MetadataObject::new(ObjectKind::Column, relation, name);
        column.data_type = Some(data_type.into());
        column
    }

    fn columns(relation: &ObjectPath) -> Option<Vec<MetadataObject>> {
        let orders = schema("sales").child(ObjectKind::Table, "orders");
        let customers = schema("crm").child(ObjectKind::Table, "customers");
        if *relation == orders {
            Some(vec![
                column(relation, "id", "integer"),
                column(relation, "total", "numeric(12,2)"),
            ])
        } else if *relation == customers {
            Some(vec![column(relation, "email", "text")])
        } else {
            None
        }
    }

    fn labels(sql: &str) -> (Vec<String>, Vec<ObjectPath>) {
        let index = index();
        let asked = request(sql, sql.len(), &index, &context()).expect("a request");
        let (found, missing) = candidates(
            &asked,
            &index,
            &context(),
            '"',
            &DialectCatalog::builtin(),
            &columns,
        );
        (found.into_iter().map(|c| c.label).collect(), missing)
    }

    #[test]
    fn relations_follow_from_and_join_in_the_tab_context() {
        assert_eq!(labels("SELECT * FROM ord").0, ["order_lines", "orders"]);
        // A qualified name looks in that schema.
        assert_eq!(labels("SELECT * FROM crm.").0, ["customers"]);
        // CTEs and namespaces are offered too.
        let (found, _) = labels("WITH recent AS (SELECT 1) SELECT * FROM r");
        assert_eq!(found, ["recent"]);
        let (found, _) = labels("SELECT * FROM orders o JOIN c");
        assert_eq!(found, ["crm"]);
        assert_eq!(labels("SELECT * FROM orders, T").0, ["Totals"]);
    }

    #[test]
    fn an_alias_suggests_the_columns_of_its_relation() {
        // Right after the alias only keywords may follow (`ON`, `ORDER`):
        // no column and nothing to read.
        let names = index();
        let sql = "SELECT o. FROM orders o";
        let asked = request(sql, sql.len(), &names, &context()).unwrap();
        let (found, missing) = candidates(
            &asked,
            &names,
            &context(),
            '"',
            &DialectCatalog::builtin(),
            &columns,
        );
        assert!(missing.is_empty());
        assert!(
            found
                .iter()
                .all(|candidate| candidate.kind == CandidateKind::Keyword)
        );
        // The cursor sits after `o.` with the FROM written after it.
        let sql = "SELECT o.t FROM orders o";
        let index = index();
        let asked = request(sql, 10, &index, &context()).unwrap();
        assert_eq!(asked.prefix, "t");
        assert_eq!(asked.replace, 9..10);
        let (found, _) = candidates(
            &asked,
            &index,
            &context(),
            '"',
            &DialectCatalog::builtin(),
            &columns,
        );
        assert_eq!(found[0].label, "total");
        assert_eq!(found[0].detail, "numeric(12,2)");
        // Unqualified: the columns of every relation of the cursor's query.
        let sql = "SELECT  FROM orders o JOIN crm.customers c ON true";
        let asked = request(sql, 7, &index, &context()).unwrap();
        let (found, _) = candidates(
            &asked,
            &index,
            &context(),
            '"',
            &DialectCatalog::builtin(),
            &columns,
        );
        let found: Vec<_> = found.into_iter().map(|c| c.label).collect();
        assert_eq!(found, ["id", "total", "email"]);
    }

    #[test]
    fn a_relation_without_read_columns_is_reported_missing() {
        let sql = "SELECT l. FROM order_lines l";
        let index = index();
        let asked = request(sql, 9, &index, &context()).unwrap();
        let (found, missing) = candidates(
            &asked,
            &index,
            &context(),
            '"',
            &DialectCatalog::builtin(),
            &columns,
        );
        assert!(found.is_empty());
        assert_eq!(
            missing,
            [schema("sales").child(ObjectKind::Table, "order_lines")]
        );
    }

    #[test]
    fn names_are_quoted_when_the_dialect_needs_it_and_literals_are_skipped() {
        let index = index();
        let sql = "SELECT * FROM To";
        let asked = request(sql, sql.len(), &index, &context()).unwrap();
        let (found, _) = candidates(
            &asked,
            &index,
            &context(),
            '"',
            &DialectCatalog::builtin(),
            &columns,
        );
        assert_eq!(found[0].insert, "\"Totals\"");
        assert!(request_in("SELECT 'FROM o", &index).is_none());
        assert!(request_in("SELECT 1; -- FROM o", &index).is_none());
    }

    #[test]
    fn a_name_the_dialect_reserves_is_quoted() {
        let catalog = crate::catalog::parse(
            crate::catalog::DialectFiles {
                manifest: r#"{"schema_version":1,"version_scheme":"numeric","versions":["18"],"verified_through":"18.6"}"#,
                keywords: r#"{"reserved":true,"source":"s","word":"user"}"#,
                functions: "",
                types: "",
            },
            None,
        )
        .unwrap();
        let relation = |name: &str| Relation {
            path: schema("sales").child(ObjectKind::Table, name),
            kind: ObjectKind::Table,
        };
        let index = NameIndex::new(
            1,
            vec![ObjectKind::Database, ObjectKind::Schema],
            SqlDialect::Postgres,
            vec![relation("user"), relation("users")],
        );
        let sql = "SELECT * FROM us";
        let asked = request(sql, sql.len(), &index, &context()).unwrap();
        let (found, _) = candidates(&asked, &index, &context(), '"', &catalog, &columns);
        let inserted: Vec<&str> = found.iter().map(|c| c.insert.as_str()).collect();
        assert_eq!(inserted, ["\"user\"", "users"]);
    }

    #[test]
    fn expressions_also_suggest_functions_and_keywords_and_casts_suggest_types() {
        let index = index();
        let catalog = DialectCatalog::builtin();
        let ask = |sql: &str| {
            let asked = request(sql, sql.len(), &index, &context()).unwrap();
            candidates(&asked, &index, &context(), '"', &catalog, &columns).0
        };
        // `co` after SELECT: the builtin `count` and `coalesce`, inserted
        // with their parenthesis.
        let found = ask("SELECT co");
        let functions: Vec<_> = found
            .iter()
            .filter(|candidate| candidate.kind == CandidateKind::Function)
            .map(|candidate| candidate.insert.as_str())
            .collect();
        assert_eq!(functions, ["coalesce(", "count("]);
        // A clause keyword after the FROM list, upper case.
        let found = ask("SELECT * FROM orders WH");
        let keywords: Vec<_> = found
            .iter()
            .filter(|candidate| candidate.kind == CandidateKind::Keyword)
            .map(|candidate| candidate.label.as_str())
            .collect();
        assert_eq!(keywords, ["WHEN", "WHERE"]);
        // Columns come before functions and keywords.
        let sql = "SELECT t FROM orders";
        let asked = request(sql, 8, &index, &context()).unwrap();
        let found = candidates(&asked, &index, &context(), '"', &catalog, &columns).0;
        assert_eq!(found[0].label, "total");
        assert!(found.iter().any(|candidate| candidate.label == "THEN"));
        // After `alias.` only columns.
        let sql = "SELECT o.co FROM orders o";
        let asked = request(sql, 11, &index, &context()).unwrap();
        assert!(
            candidates(&asked, &index, &context(), '"', &catalog, &columns)
                .0
                .is_empty()
        );
        // After `::`, types.
        let asked = request("SELECT a::", 10, &index, &context()).unwrap();
        assert_eq!(asked.wanted, Wanted::Types);
    }

    #[test]
    fn the_function_under_the_cursor_is_found_before_its_parenthesis() {
        let catalog = DialectCatalog::builtin();
        let sql = "SELECT count(*), total FROM orders";
        let (name, range) = function_at(sql, 9, Some(SqlDialect::Postgres), &catalog).unwrap();
        assert_eq!(name, "count");
        assert_eq!(&sql[range], "count");
        assert!(function_at(sql, 19, Some(SqlDialect::Postgres), &catalog).is_none());
    }

    #[test]
    fn a_cursor_after_a_space_still_continues_the_statement() {
        // Found by the tree-sitter proof of concept: Ctrl+Space after
        // `WHERE ` suggested nothing.
        let index = index();
        let sql = "SELECT * FROM orders WHERE ";
        let asked = request(sql, sql.len(), &index, &context()).unwrap();
        assert_eq!(
            asked.wanted,
            Wanted::Columns {
                relations: vec![schema("sales").child(ObjectKind::Table, "orders")],
                qualified: false,
            }
        );
        let (found, _) = candidates(
            &asked,
            &index,
            &context(),
            '"',
            &DialectCatalog::builtin(),
            &columns,
        );
        assert!(found.iter().any(|candidate| candidate.label == "total"));
        // After a `;` the next statement has not started.
        assert!(request_in("SELECT 1; ", &index).is_none());
    }

    fn request_in(sql: &str, index: &NameIndex) -> Option<Request> {
        request(sql, sql.len(), index, &context())
    }

    #[test]
    fn the_symbol_under_the_cursor_is_a_relation_or_its_column() {
        let index = index();
        let sql = "SELECT o.total FROM orders o";
        let (symbol, range) = symbol_at(sql, 10, &index, &context(), &columns).unwrap();
        let orders = schema("sales").child(ObjectKind::Table, "orders");
        assert_eq!(
            symbol,
            Symbol::Column {
                relation: orders.clone(),
                name: "total".into()
            }
        );
        assert_eq!(&sql[range], "total");
        assert_eq!(symbol.path(), orders.child(ObjectKind::Column, "total"));
        let (symbol, _) = symbol_at(sql, 22, &index, &context(), &columns).unwrap();
        assert_eq!(symbol, Symbol::Relation(orders));
        assert!(symbol_at(sql, 2, &index, &context(), &columns).is_none());
        // A connection's index never answers for another one's names.
        let empty = NameIndex::default();
        assert!(symbol_at(sql, 22, &empty, &context(), &columns).is_none());
    }
}
