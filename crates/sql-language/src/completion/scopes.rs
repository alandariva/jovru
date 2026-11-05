//! The queries of a statement, the relations each one reads and the columns
//! each one makes, from the dialect's tokens. The statement, every subquery
//! in parentheses, every CTE body and every `SELECT` branch (after `UNION`,
//! `INSERT …`, `CREATE … AS` or a `WITH` list) is a query that binds the
//! relations of its own `FROM`, `JOIN`, `USING`, `UPDATE` and `INTO`. A CTE
//! or a derived table stands for the columns its query's `SELECT` list makes
//! (those of its first branch), renamed by a column list when one is written.
//!
//! A name without a qualifier sees the relations of the query around it;
//! `alias.` names a relation of that query or, as in a correlated subquery,
//! of an enclosing one. The text may be unfinished: a parenthesis left open
//! keeps its query open to the end of the statement.
use jykre_core::SqlDialect;
use jykre_sql::TokenKind;
use jykre_sql::names::{Name, Part, name_at, parts};
use std::ops::Range;

/// Words that end a relation reference instead of being its alias.
const NOT_ALIAS: &[&str] = &[
    "ANTI",
    "AS",
    "CROSS",
    "EXCEPT",
    "FOR",
    "FORCE",
    "FROM",
    "FULL",
    "GROUP",
    "HAVING",
    "IGNORE",
    "INNER",
    "INTERSECT",
    "INTO",
    "JOIN",
    "LATERAL",
    "LEFT",
    "LIMIT",
    "MINUS",
    "NATURAL",
    "OFFSET",
    "ON",
    "ORDER",
    "OUTER",
    "PARTITION",
    "PIVOT",
    "QUALIFY",
    "RETURNING",
    "RIGHT",
    "SELECT",
    "SEMI",
    "SET",
    "STRAIGHT_JOIN",
    "TABLESAMPLE",
    "UNION",
    "UNPIVOT",
    "USE",
    "USING",
    "VALUES",
    "WHEN",
    "WHERE",
    "WINDOW",
    "WITH",
];

/// Words that end a `FROM` list at the query's own level.
const END_LIST: &[&str] = &[
    "EXCEPT",
    "FOR",
    "GROUP",
    "HAVING",
    "INTERSECT",
    "LATERAL",
    "LIMIT",
    "MINUS",
    "OFFSET",
    "ON",
    "ORDER",
    "PIVOT",
    "QUALIFY",
    "RETURNING",
    "SELECT",
    "SET",
    "UNION",
    "UNPIVOT",
    "VALUES",
    "WHEN",
    "WHERE",
    "WINDOW",
];

/// Words that start a join.
const JOINS: &[&str] = &[
    "ANTI",
    "CROSS",
    "FULL",
    "INNER",
    "JOIN",
    "LEFT",
    "NATURAL",
    "RIGHT",
    "SEMI",
    "STRAIGHT_JOIN",
];

/// Words that end a `SELECT` list at the query's own level.
const END_SELECT: &[&str] = &[
    "EXCEPT",
    "FETCH",
    "FOR",
    "FROM",
    "GROUP",
    "HAVING",
    "INTERSECT",
    "INTO",
    "LIMIT",
    "MINUS",
    "OFFSET",
    "ORDER",
    "QUALIFY",
    "UNION",
    "WHERE",
    "WINDOW",
];

/// Words that may come before the first column of a `SELECT` list.
const MODIFIERS: &[&str] = &[
    "ALL",
    "DISTINCT",
    "DISTINCTROW",
    "HIGH_PRIORITY",
    "SQL_BIG_RESULT",
    "SQL_BUFFER_RESULT",
    "SQL_CALC_FOUND_ROWS",
    "SQL_NO_CACHE",
    "SQL_SMALL_RESULT",
    "STRAIGHT_JOIN",
];

/// Words that are not a column's alias written without `AS`: operators,
/// literals and the units of an `INTERVAL`.
const NOT_COLUMN: &[&str] = &[
    "AND", "ANY", "ARRAY", "ASC", "BETWEEN", "CASE", "COLLATE", "DAY", "DESC", "DISTINCT", "ELSE",
    "END", "ESCAPE", "EXISTS", "FALSE", "FILTER", "HOUR", "ILIKE", "IN", "INTERVAL", "IS", "LIKE",
    "MINUTE", "MONTH", "NOT", "NULL", "OR", "OVER", "QUARTER", "SECOND", "SOME", "THEN", "TRUE",
    "UNKNOWN", "WEEK", "WITHIN", "YEAR", "ZONE",
];

/// Words that end an expression although `NOT_COLUMN` holds them.
const ENDS_EXPRESSION: &[&str] = &["END", "FALSE", "NULL", "TRUE", "UNKNOWN"];

/// A column a query's `SELECT` list makes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Output {
    /// `expression AS name`, `expression name` or a column reference, with
    /// the column it reads when it is a plain reference.
    Named { name: Name, reads: Option<Read> },
    /// `*`, or `alias.*` with the alias.
    Star(Option<Name>),
    /// An expression the server names by itself (`count(*)`); it keeps its
    /// place for a column list written after the CTE or the derived table.
    Unnamed,
}

/// The column a plain reference reads: `[qualifier.]column`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Read {
    pub(crate) qualifier: Option<Name>,
    pub(crate) column: Name,
}

/// A relation a query reads.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Binding {
    /// Its name as written: a table, a view or a CTE. A derived table has
    /// none.
    pub(crate) name: Option<Vec<Name>>,
    /// Its alias, unquoted.
    alias: Option<String>,
    /// The query a derived table reads (its first branch).
    query: Option<usize>,
    /// The column names written after a derived table's alias:
    /// `AS t (a, b)`.
    columns: Option<Vec<Name>>,
    /// Where it is seen, when not in the whole query: the condition of a
    /// `SEMI` or `ANTI` join (Databricks), whose relation is not part of the
    /// join's result.
    only_in: Option<Range<usize>>,
}

impl Binding {
    /// Whether `qualifier` (lower case) names it: its alias or, without one,
    /// the last part of its name.
    fn answers_to(&self, qualifier: &str) -> bool {
        match &self.alias {
            Some(alias) => alias.to_lowercase() == qualifier,
            None => self
                .name
                .as_ref()
                .and_then(|name| name.last())
                .is_some_and(|last| last.text.to_lowercase() == qualifier),
        }
    }

    fn seen_at(&self, offset: usize) -> bool {
        self.only_in
            .as_ref()
            .is_none_or(|range| range.contains(&offset))
    }
}

#[derive(Default)]
struct Query {
    parent: Option<usize>,
    /// The bytes of the statement it covers.
    span: Range<usize>,
    bindings: Vec<Binding>,
    /// The columns its `SELECT` list makes.
    outputs: Vec<Output>,
    /// A word was read at this query's own level.
    started: bool,
    /// A comma at this level continues a `FROM` list.
    from_list: bool,
    /// Inside a `WITH` list, where `name AS (` opens a CTE.
    with_list: bool,
    /// Where its `SELECT` list starts (a token), while it is being read.
    select_from: Option<usize>,
    /// The tokens of its `SELECT` list.
    select: Option<Range<usize>>,
    /// An open `SEMI` or `ANTI` join: its first binding and where it starts.
    semi: Option<(usize, usize)>,
}

impl Query {
    /// Ends the `SELECT` list being read at token `at`.
    fn end_select(&mut self, at: usize) {
        if let Some(from) = self.select_from.take() {
            self.select = Some(from..at);
        }
    }
}

/// Ends `query`'s open `SEMI` or `ANTI` join at byte `end`.
fn close_semi(query: &mut Query, end: usize) {
    if let Some((first, start)) = query.semi.take() {
        for binding in query.bindings.iter_mut().skip(first) {
            binding.only_in = Some(start..end);
        }
    }
}

/// A CTE: its name as written, the query of its body and the column names
/// written after its name.
struct Cte {
    name: String,
    query: usize,
    columns: Option<Vec<Name>>,
}

enum Frame {
    /// The statement or a query in parentheses: its branches so far (the
    /// last one is current) and whether it is a derived table.
    Query { branches: Vec<usize>, derived: bool },
    /// Parentheses around anything else: arguments, a list, an expression.
    Other,
}

/// The query at whose own level the reader is: the current branch of the
/// innermost query frame.
fn current_query(frames: &[Frame]) -> usize {
    frames
        .iter()
        .rev()
        .find_map(|frame| match frame {
            Frame::Query { branches, .. } => branches.last().copied(),
            Frame::Other => None,
        })
        .unwrap_or(0)
}

/// The queries of a statement, the relations they read and the columns
/// they make.
pub(crate) struct Scopes {
    queries: Vec<Query>,
    ctes: Vec<Cte>,
}

impl Scopes {
    /// Reads `statement` by the rules of `dialect`.
    pub(crate) fn new(statement: &str, dialect: Option<SqlDialect>) -> Self {
        let tokens = jykre_sql::tokens(statement, dialect)
            .into_iter()
            .filter(|(_, kind)| *kind != TokenKind::Comment);
        // MySQL's executable comments (`/*! … */`) are hints, not structure.
        let (ranges, parts): (Vec<_>, Vec<_>) = tokens
            .zip(parts(statement, dialect))
            .filter(|((_, kind), _)| *kind != TokenKind::Executable)
            .map(|((range, _), part)| (range, part))
            .unzip();
        let mut reader = Reader {
            ranges,
            parts,
            end: statement.len(),
            queries: vec![Query {
                span: 0..statement.len(),
                ..Query::default()
            }],
            ctes: Vec::new(),
        };
        reader.read();
        Self {
            queries: reader.queries,
            ctes: reader.ctes,
        }
    }

    /// The statement's CTE names, as written.
    pub(crate) fn cte_names(&self) -> Vec<String> {
        self.ctes.iter().map(|cte| cte.name.clone()).collect()
    }

    fn cte(&self, name: &[Name]) -> Option<&Cte> {
        match name {
            [only] => self
                .ctes
                .iter()
                .find(|cte| cte.name.eq_ignore_ascii_case(&only.text)),
            _ => None,
        }
    }

    /// The columns a CTE or a derived table stands for, and the query that
    /// makes them; `None` for a relation of the cache.
    pub(crate) fn outputs(&self, binding: &Binding) -> Option<(usize, Vec<Output>)> {
        let (query, names) = match (binding.query, &binding.name) {
            (Some(query), _) => (query, binding.columns.as_ref()),
            (None, Some(name)) => {
                let cte = self.cte(name)?;
                (cte.query, cte.columns.as_ref())
            }
            (None, None) => return None,
        };
        let outputs = &self.queries.get(query)?.outputs;
        Some((
            query,
            match names {
                Some(names) => renamed(outputs, names),
                None => outputs.clone(),
            },
        ))
    }

    /// The relations of `query` a column reference reads: the one
    /// `qualifier` (lower case) names, from that query outwards, or without
    /// one every relation of the query.
    pub(crate) fn read_by(&self, query: usize, qualifier: Option<&str>) -> Vec<&Binding> {
        match qualifier {
            Some(qualifier) => self
                .chain(query)
                .find_map(|query| {
                    query
                        .bindings
                        .iter()
                        .find(|binding| binding.answers_to(qualifier))
                })
                .into_iter()
                .collect(),
            None => self.queries.get(query).map_or_else(Vec::new, |query| {
                query
                    .bindings
                    .iter()
                    .filter(|binding| binding.only_in.is_none())
                    .collect()
            }),
        }
    }

    /// `query` and the queries around it, the innermost first.
    fn chain(&self, query: usize) -> impl Iterator<Item = &Query> {
        std::iter::successors(Some(query), |query| self.queries.get(*query)?.parent)
            .filter_map(|query| self.queries.get(query))
    }

    /// The queries around byte `offset`, the innermost first.
    fn around(&self, offset: usize) -> impl Iterator<Item = &Query> {
        let innermost = self
            .queries
            .iter()
            .enumerate()
            .filter(|(_, query)| query.span.start <= offset && offset <= query.span.end)
            .max_by_key(|(_, query)| query.span.start)
            .map_or(0, |(index, _)| index);
        self.chain(innermost)
    }

    /// The relations a name without a qualifier at `offset` may be a column
    /// of: those of the query around it.
    pub(crate) fn visible(&self, offset: usize) -> impl Iterator<Item = &Binding> {
        self.around(offset).take(1).flat_map(move |query| {
            query
                .bindings
                .iter()
                .filter(move |binding| binding.seen_at(offset))
        })
    }

    /// The relations around `offset` from the innermost query outwards, where
    /// a column written without a qualifier is found.
    pub(crate) fn outwards(&self, offset: usize) -> impl Iterator<Item = &Binding> {
        self.around(offset).flat_map(move |query| {
            query
                .bindings
                .iter()
                .filter(move |binding| binding.seen_at(offset))
        })
    }

    /// The relation `qualifier` (lower case) names at `offset`, in the query
    /// around it or else in the closest enclosing one.
    pub(crate) fn qualified(&self, offset: usize, qualifier: &str) -> Option<&Binding> {
        self.around(offset).find_map(|query| {
            query
                .bindings
                .iter()
                .find(|binding| binding.answers_to(qualifier))
        })
    }
}

/// `outputs` under the column `names` written after a CTE or a derived
/// table, by position. Behind a `*` the positions are unknown, so the names
/// read nothing.
fn renamed(outputs: &[Output], names: &[Name]) -> Vec<Output> {
    let star = outputs
        .iter()
        .any(|output| matches!(output, Output::Star(_)));
    names
        .iter()
        .enumerate()
        .map(|(at, name)| Output::Named {
            name: name.clone(),
            reads: match outputs.get(at) {
                Some(Output::Named { reads, .. }) if !star => reads.clone(),
                _ => None,
            },
        })
        .collect()
}

fn is(part: Option<&Part>, symbol: &str) -> bool {
    matches!(part, Some(Part::Symbol(text)) if text == symbol)
}

/// A name a token makes: a word or a quoted identifier.
fn name_of(part: &Part) -> Option<Name> {
    match part {
        Part::Word(_, text) => Some(Name {
            text: text.clone(),
            quoted: false,
        }),
        Part::Quoted(text) => Some(Name {
            text: text.clone(),
            quoted: true,
        }),
        _ => None,
    }
}

/// The names in the parentheses `parts` starts with, `(a, b)` (the first
/// word of each item, so `(a int, b text)` gives `a` and `b`), and how many
/// tokens they take.
fn name_list(parts: &[Part]) -> (Vec<Name>, usize) {
    let mut names = Vec::new();
    let mut expect = true;
    for (at, part) in parts.iter().enumerate().skip(1) {
        match part {
            Part::Symbol(symbol) if symbol == ")" => return (names, at + 1),
            Part::Symbol(symbol) if symbol == "," => expect = true,
            _ => {
                if expect && let Some(name) = name_of(part) {
                    names.push(name);
                }
                expect = false;
            }
        }
    }
    (names, parts.len())
}

/// What follows the parenthesis that `parts` opens at `open`, and its
/// match.
fn after_group(parts: &[Part], open: usize) -> &[Part] {
    let mut depth = 0usize;
    for (at, part) in parts.iter().enumerate().skip(open) {
        if is(Some(part), "(") {
            depth += 1;
        } else if is(Some(part), ")") {
            depth = depth.saturating_sub(1);
            if depth == 0 {
                return parts.get(at + 1..).unwrap_or_default();
            }
        }
    }
    &[]
}

/// The columns a `SELECT` list makes, from its tokens.
fn select_outputs(parts: &[Part]) -> Vec<Output> {
    let mut items = Vec::new();
    let mut depth = 0usize;
    let mut start = 0;
    for (at, part) in parts.iter().enumerate() {
        if let Part::Symbol(symbol) = part {
            match symbol.as_str() {
                "(" | "[" => depth += 1,
                ")" | "]" => depth = depth.saturating_sub(1),
                "," if depth == 0 => {
                    items.push(&parts[start..at]);
                    start = at + 1;
                }
                _ => {}
            }
        }
    }
    items.push(&parts[start..]);
    let mut outputs = Vec::new();
    for (index, item) in items.into_iter().enumerate() {
        let item = if index == 0 {
            without_modifiers(item)
        } else {
            item
        };
        if !item.is_empty() {
            output(item, &mut outputs);
        }
    }
    outputs
}

/// The first item of a `SELECT` list without what may come before its
/// column: `DISTINCT`, `ALL`, MySQL's hints and PostgreSQL's
/// `DISTINCT ON (…)`.
fn without_modifiers(mut item: &[Part]) -> &[Part] {
    while let [Part::Word(word, _), rest @ ..] = item
        && MODIFIERS.contains(&word.as_str())
    {
        item = rest;
        if word == "DISTINCT"
            && let [Part::Word(on, _), Part::Symbol(open), ..] = item
            && on == "ON"
            && open == "("
        {
            item = after_group(item, 1);
        }
    }
    item
}

/// The column `expression` reads when it is a plain reference: `column` or
/// `qualifier.column`, and nothing else.
fn plain(expression: &[Part]) -> Option<Read> {
    let (name, after) = name_at(expression, 0)?;
    if after != expression.len() {
        return None;
    }
    let (column, qualifiers) = name.split_last()?;
    let literal = ENDS_EXPRESSION.contains(&column.text.to_ascii_uppercase().as_str());
    if qualifiers.is_empty() && !column.quoted && literal {
        return None;
    }
    Some(Read {
        qualifier: qualifiers.last().cloned(),
        column: column.clone(),
    })
}

/// Whether `part` may end an expression, so a name after it is an alias.
fn ends_expression(part: &Part) -> bool {
    match part {
        Part::Word(word, _) => {
            ENDS_EXPRESSION.contains(&word.as_str())
                || !(NOT_COLUMN.contains(&word.as_str()) || NOT_ALIAS.contains(&word.as_str()))
        }
        Part::Quoted(_) | Part::Other => true,
        Part::Symbol(symbol) => symbol == ")" || symbol == "]",
    }
}

/// The alias written without `AS` that `part` may be.
fn implicit_alias(part: &Part) -> Option<Name> {
    match part {
        Part::Word(word, _)
            if NOT_COLUMN.contains(&word.as_str()) || NOT_ALIAS.contains(&word.as_str()) =>
        {
            None
        }
        _ => name_of(part),
    }
}

/// The columns one item of a `SELECT` list makes.
fn output(item: &[Part], outputs: &mut Vec<Output>) {
    // `*` (also Databricks' `* EXCEPT (…)`) and `alias.*`.
    if is(item.first(), "*") {
        outputs.push(Output::Star(None));
        return;
    }
    let mut at = 0;
    while let Some(name) = item.get(at).and_then(name_of) {
        if !is(item.get(at + 1), ".") {
            break;
        }
        if is(item.get(at + 2), "*") {
            outputs.push(Output::Star(Some(name)));
            return;
        }
        at += 2;
    }
    // `expression AS name`, or `AS (a, b)` for a function that makes several.
    let mut depth = 0usize;
    let mut alias = None;
    for (at, part) in item.iter().enumerate() {
        match part {
            Part::Symbol(symbol) if symbol == "(" || symbol == "[" => depth += 1,
            Part::Symbol(symbol) if symbol == ")" || symbol == "]" => {
                depth = depth.saturating_sub(1);
            }
            Part::Word(word, _) if word == "AS" && depth == 0 => alias = Some(at),
            _ => {}
        }
    }
    if let Some(at) = alias {
        let (expression, written) = item.split_at(at);
        let written = written.get(1..).unwrap_or_default();
        if let [single] = written
            && let Some(name) = name_of(single)
        {
            outputs.push(Output::Named {
                name,
                reads: plain(expression),
            });
        } else if is(written.first(), "(") {
            for name in name_list(written).0 {
                outputs.push(Output::Named { name, reads: None });
            }
        } else {
            outputs.push(Output::Unnamed);
        }
        return;
    }
    if let Some(read) = plain(item) {
        outputs.push(Output::Named {
            name: read.column.clone(),
            reads: Some(read),
        });
        return;
    }
    // `expression name`.
    if let [.., before, last] = item
        && ends_expression(before)
        && let Some(name) = implicit_alias(last)
    {
        outputs.push(Output::Named {
            name,
            reads: plain(&item[..item.len() - 1]),
        });
        return;
    }
    outputs.push(Output::Unnamed);
}

/// One pass over a statement's tokens.
struct Reader {
    ranges: Vec<Range<usize>>,
    parts: Vec<Part>,
    /// The statement's length.
    end: usize,
    queries: Vec<Query>,
    ctes: Vec<Cte>,
}

impl Reader {
    fn word(&self, at: usize) -> Option<&str> {
        match self.parts.get(at)? {
            Part::Word(upper, _) => Some(upper),
            _ => None,
        }
    }

    fn is(&self, at: usize, symbol: &str) -> bool {
        is(self.parts.get(at), symbol)
    }

    fn start(&self, at: usize) -> usize {
        self.ranges.get(at).map_or(self.end, |range| range.start)
    }

    fn read(&mut self) {
        let mut frames = vec![Frame::Query {
            branches: vec![0],
            derived: false,
        }];
        let mut at = 0;
        while at < self.parts.len() {
            let current = current_query(&frames);
            let level = matches!(frames.last(), Some(Frame::Query { .. }));
            at = if self.is(at, "(") {
                self.open(&mut frames, at, current, level)
            } else if self.is(at, ")") {
                self.close(&mut frames, at)
            } else if !level {
                at + 1
            } else if self.is(at, ",") && self.queries[current].from_list {
                let start = self.start(at);
                close_semi(&mut self.queries[current], start);
                self.relation_list(at + 1, current)
            } else if let Some(word) = self.word(at).map(str::to_owned) {
                self.word_at(&mut frames, at, current, &word)
            } else {
                at + 1
            };
        }
        let last = self.parts.len();
        for query in &mut self.queries {
            let end = query.span.end;
            close_semi(query, end);
            query.end_select(last);
            if let Some(select) = query.select.clone() {
                query.outputs = select_outputs(self.parts.get(select).unwrap_or_default());
            }
        }
    }

    /// A `(`. A subquery opens a query of its own: a derived table after
    /// `FROM`, `JOIN`, `LATERAL`, `USING` or a comma of a `FROM` list, or a
    /// CTE body after `name AS`. Other parentheses only nest.
    fn open(&mut self, frames: &mut Vec<Frame>, at: usize, current: usize, level: bool) -> usize {
        if !matches!(self.word(at + 1), Some("SELECT" | "WITH" | "VALUES")) {
            frames.push(Frame::Other);
            return at + 1;
        }
        let derived = level
            && at.checked_sub(1).is_some_and(|before| {
                matches!(
                    self.word(before),
                    Some("FROM" | "JOIN" | "LATERAL" | "USING" | "STRAIGHT_JOIN")
                ) || (self.is(before, ",") && self.queries[current].from_list)
            });
        let query = self.queries.len();
        if level
            && self.queries[current].with_list
            && let Some((name, columns)) = self.cte_name(at)
        {
            self.ctes.push(Cte {
                name,
                query,
                columns,
            });
        }
        let start = self.ranges.get(at).map_or(self.end, |range| range.end);
        self.queries.push(Query {
            parent: Some(current),
            span: start..self.end,
            ..Query::default()
        });
        frames.push(Frame::Query {
            branches: vec![query],
            derived,
        });
        at + 1
    }

    /// A `)`: ends the parentheses, or the query they opened; a derived table
    /// binds its alias, and the column names after it, in the enclosing
    /// query.
    fn close(&mut self, frames: &mut Vec<Frame>, at: usize) -> usize {
        // An unbalanced `)` leaves the statement open.
        if frames.len() == 1 {
            return at + 1;
        }
        let Some(Frame::Query { branches, derived }) = frames.pop() else {
            return at + 1;
        };
        let end = self.start(at);
        if let Some(last) = branches.last() {
            let query = &mut self.queries[*last];
            query.span.end = end;
            query.end_select(at);
        }
        if !derived {
            return at + 1;
        }
        let parent = current_query(frames);
        let (alias, mut after) = self.alias_at(at + 1);
        let mut columns = None;
        if alias.is_some() && self.is(after, "(") {
            let (names, taken) = name_list(self.parts.get(after..).unwrap_or_default());
            columns = Some(names);
            after += taken;
        }
        self.queries[parent].bindings.push(Binding {
            name: None,
            alias,
            query: branches.first().copied(),
            columns,
            only_in: None,
        });
        if self.is(after, ",") && self.queries[parent].from_list {
            return self.relation_list(after + 1, parent);
        }
        after
    }

    /// A word at the current query's own level.
    fn word_at(&mut self, frames: &mut [Frame], at: usize, current: usize, word: &str) -> usize {
        let first = !self.queries[current].started;
        self.queries[current].started = true;
        let before = at
            .checked_sub(1)
            .and_then(|before| self.word(before))
            .map(str::to_owned);
        let start = self.start(at);
        if JOINS.contains(&word) || (word != "ON" && END_LIST.contains(&word)) {
            close_semi(&mut self.queries[current], start);
        }
        // `IS DISTINCT FROM` is an operator.
        let operator = word == "FROM" && before.as_deref() == Some("DISTINCT");
        if END_SELECT.contains(&word) && !operator {
            self.queries[current].end_select(at);
        }
        match word {
            "SELECT" => {
                let owner = if first {
                    current
                } else {
                    self.branch(frames, current, at)
                };
                let query = &mut self.queries[owner];
                query.from_list = false;
                query.select_from = Some(at + 1);
                at + 1
            }
            "WITH" => {
                self.queries[current].with_list = true;
                at + 1
            }
            // `SELECT STRAIGHT_JOIN` is a hint.
            "FROM" if operator => at + 1,
            "STRAIGHT_JOIN" if !self.queries[current].from_list => at + 1,
            "FROM" | "JOIN" | "USING" | "STRAIGHT_JOIN" => {
                let query = &mut self.queries[current];
                if word == "JOIN" && matches!(before.as_deref(), Some("SEMI" | "ANTI")) {
                    query.semi = Some((query.bindings.len(), start));
                }
                query.from_list = true;
                self.relation_list(at + 1, current)
            }
            // Not `FOR UPDATE`, `ON DUPLICATE KEY UPDATE`, `DO UPDATE` or
            // `THEN UPDATE`.
            "UPDATE" if !matches!(before.as_deref(), Some("FOR" | "KEY" | "DO" | "THEN")) => {
                self.queries[current].from_list = true;
                self.relation_list(at + 1, current)
            }
            "INTO" if matches!(before.as_deref(), Some("INSERT" | "MERGE" | "REPLACE")) => {
                self.relation(at + 1, current, false).1
            }
            "TABLE" => self.relation(at + 1, current, false).1,
            _ => {
                if END_LIST.contains(&word) {
                    self.queries[current].from_list = false;
                }
                at + 1
            }
        }
    }

    /// A `SELECT` after the query's first word starts a branch of it: after
    /// `UNION` and the like, `INSERT …`, `CREATE … AS` or a `WITH` list.
    /// Branches share the enclosing query, not their relations.
    fn branch(&mut self, frames: &mut [Frame], current: usize, at: usize) -> usize {
        let start = self.start(at);
        let query = &mut self.queries[current];
        query.span.end = start;
        query.end_select(at);
        let parent = query.parent;
        let branch = self.queries.len();
        self.queries.push(Query {
            parent,
            span: start..self.end,
            started: true,
            ..Query::default()
        });
        if let Some(Frame::Query { branches, .. }) = frames.last_mut() {
            branches.push(branch);
        }
        branch
    }

    /// Relations separated by commas from `at`, bound in `query`; where
    /// reading stopped.
    fn relation_list(&mut self, mut at: usize, query: usize) -> usize {
        loop {
            let (bound, after) = self.relation(at, query, true);
            if !(bound && self.is(after, ",")) {
                return after;
            }
            let start = self.start(after);
            close_semi(&mut self.queries[query], start);
            at = after + 1;
        }
    }

    /// The relation at `at`, `[ONLY | LATERAL] name [[AS] alias]`, bound in
    /// `query`: whether one was, and where reading stopped. A `(` stops it,
    /// where a subquery is a derived table; in a `FROM` list (`functions`), a
    /// name followed by `(` is a table function, not a relation.
    fn relation(&mut self, mut at: usize, query: usize, functions: bool) -> (bool, usize) {
        while matches!(self.word(at), Some("ONLY" | "LATERAL")) {
            at += 1;
        }
        if self.word(at).is_some_and(|word| NOT_ALIAS.contains(&word)) {
            return (false, at);
        }
        let Some((name, after)) = name_at(&self.parts, at) else {
            return (false, at);
        };
        if functions && self.is(after, "(") {
            return (false, after);
        }
        let (alias, after) = self.alias_at(after);
        self.queries[query].bindings.push(Binding {
            name: Some(name),
            alias,
            query: None,
            columns: None,
            only_in: None,
        });
        (true, after)
    }

    /// `[AS] alias` at `at`, unquoted, and where it ends.
    fn alias_at(&self, mut at: usize) -> (Option<String>, usize) {
        if self.word(at) == Some("AS") {
            at += 1;
        }
        match self.parts.get(at) {
            Some(Part::Quoted(text)) => (Some(text.clone()), at + 1),
            Some(Part::Word(upper, text)) if !NOT_ALIAS.contains(&upper.as_str()) => {
                (Some(text.clone()), at + 1)
            }
            _ => (None, at),
        }
    }

    /// The CTE whose body opens at `at`, and the column names written after
    /// its name: `name [(columns)] AS [[NOT] MATERIALIZED] (`.
    fn cte_name(&self, at: usize) -> Option<(String, Option<Vec<Name>>)> {
        let mut before = at.checked_sub(1)?;
        while matches!(self.word(before), Some("MATERIALIZED" | "NOT")) {
            before = before.checked_sub(1)?;
        }
        if self.word(before) != Some("AS") {
            return None;
        }
        before = before.checked_sub(1)?;
        let mut columns = None;
        if self.is(before, ")") {
            // Back over the column list.
            let mut depth = 0usize;
            loop {
                if self.is(before, ")") {
                    depth += 1;
                } else if self.is(before, "(") {
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        break;
                    }
                }
                before = before.checked_sub(1)?;
            }
            columns = Some(name_list(self.parts.get(before..).unwrap_or_default()).0);
            before = before.checked_sub(1)?;
        }
        match self.parts.get(before)? {
            Part::Word(_, text) | Part::Quoted(text) => Some((text.clone(), columns)),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Output, Read, Scopes, select_outputs};
    use jykre_core::SqlDialect;
    use jykre_sql::names::{Name, parts};
    use std::time::{Duration, Instant};

    /// The relations seen at the `‸` in `sql`: after `alias.`, the one it
    /// names; otherwise those of the query around the cursor. Only relations
    /// of the cache, by name; CTEs and derived tables left out.
    fn seen(sql: &str) -> Vec<String> {
        let cursor = sql.find('‸').unwrap();
        let sql = sql.replace('‸', "");
        let scopes = Scopes::new(&sql, Some(SqlDialect::DatabricksSql));
        let head = sql[..cursor].trim_end_matches(|ch: char| ch.is_alphanumeric() || ch == '_');
        let bindings: Vec<_> = match head.strip_suffix('.') {
            Some(before) => {
                let alias = before
                    .rsplit(|ch: char| !(ch.is_alphanumeric() || ch == '_'))
                    .next()
                    .unwrap()
                    .to_lowercase();
                scopes.qualified(cursor, &alias).into_iter().collect()
            }
            None => scopes.visible(cursor).collect(),
        };
        bindings
            .iter()
            .filter(|binding| scopes.outputs(binding).is_none())
            .filter_map(|binding| binding.name.as_ref())
            .map(|name| {
                name.iter()
                    .map(|part| part.text.as_str())
                    .collect::<Vec<_>>()
                    .join(".")
            })
            .collect()
    }

    fn name(text: &str) -> Name {
        Name {
            text: text.into(),
            quoted: false,
        }
    }

    fn named(text: &str, reads: Option<(Option<&str>, &str)>) -> Output {
        Output::Named {
            name: name(text),
            reads: reads.map(|(qualifier, column)| Read {
                qualifier: qualifier.map(name),
                column: name(column),
            }),
        }
    }

    #[test]
    fn a_semi_or_anti_join_is_seen_only_in_its_condition() {
        let sql = "SELECT * FROM orders o LEFT ANTI JOIN customers c ON c.id = ‸ WHERE o.total > 0";
        assert_eq!(seen(sql), ["orders", "customers"]);
        let sql =
            "SELECT * FROM orders o LEFT ANTI JOIN customers c ON c.id = o.customer_id WHERE ‸";
        assert_eq!(seen(sql), ["orders"]);
        let sql = "SELECT ‸ FROM orders o SEMI JOIN customers c ON c.id = o.customer_id JOIN lines l ON l.order_id = o.id";
        assert_eq!(seen(sql), ["orders", "lines"]);
    }

    #[test]
    fn unfinished_text_keeps_the_open_query() {
        // Typing inside a subquery whose parenthesis is not closed yet.
        let sql = "SELECT * FROM orders o WHERE o.id IN (SELECT order_id FROM lines l WHERE ‸";
        assert_eq!(seen(sql), ["lines"]);
        let sql = "SELECT * FROM orders o WHERE o.id IN (SELECT order_id FROM lines l WHERE o.‸";
        assert_eq!(seen(sql), ["orders"]);
        // A `)` with nothing to close leaves the statement as it was.
        assert_eq!(seen("SELECT ‸) FROM orders"), ["orders"]);
    }

    #[test]
    fn words_that_only_look_like_relation_slots_bind_nothing() {
        let sql = "SELECT extract(year FROM created_at), ‸ FROM orders \
                   WHERE a IS DISTINCT FROM b FOR UPDATE";
        assert_eq!(seen(sql), ["orders"]);
        let sql = "SELECT ‸ FROM generate_series(1, 3) g, orders";
        assert_eq!(seen(sql), ["orders"]);
    }

    #[test]
    fn cte_names_are_read_with_columns_and_materialization() {
        let sql = "WITH a (x, y) AS (SELECT 1, 2), b AS NOT MATERIALIZED (SELECT * FROM a) \
                   SELECT ‸ FROM b JOIN orders o ON true";
        let scopes = Scopes::new(&sql.replace('‸', ""), Some(SqlDialect::Postgres));
        assert_eq!(scopes.cte_names(), ["a", "b"]);
        assert_eq!(seen(sql), ["orders"]);
    }

    #[test]
    fn a_select_list_names_its_columns() {
        let list = "DISTINCT ON (o.id) o.id, nome AS num, c.email, count(*) n, \
                    upper(x), t.*, *, \"Quoted\" q, x::int, CASE WHEN a THEN 1 END, \
                    CASE WHEN a THEN 1 END status, a IS NULL, total amount, NULL, \
                    explode(m) AS (k, v), interval '1' day";
        let outputs = select_outputs(&parts(list, Some(SqlDialect::Postgres)));
        let quoted = Output::Named {
            name: Name {
                text: "q".into(),
                quoted: false,
            },
            reads: Some(Read {
                qualifier: None,
                column: Name {
                    text: "Quoted".into(),
                    quoted: true,
                },
            }),
        };
        assert_eq!(
            outputs,
            [
                named("id", Some((Some("o"), "id"))),
                named("num", Some((None, "nome"))),
                named("email", Some((Some("c"), "email"))),
                named("n", None),
                Output::Unnamed,
                Output::Star(Some(name("t"))),
                Output::Star(None),
                quoted,
                Output::Unnamed,
                Output::Unnamed,
                named("status", None),
                Output::Unnamed,
                named("amount", Some((None, "total"))),
                Output::Unnamed,
                named("k", None),
                named("v", None),
                Output::Unnamed,
            ]
        );
    }

    #[test]
    fn ctes_and_derived_tables_make_the_columns_of_their_select_list() {
        let sql = "WITH t (a, b) AS (SELECT nome, email FROM clientes), \
                   u AS (SELECT * FROM t UNION ALL SELECT 1, 2) \
                   SELECT ‸ FROM t, u, (SELECT nome AS n FROM clientes) AS d (renamed)";
        let cursor = sql.find('‸').unwrap();
        let sql = sql.replace('‸', "");
        let scopes = Scopes::new(&sql, Some(SqlDialect::Postgres));
        let outputs: Vec<_> = scopes
            .visible(cursor)
            .map(|binding| scopes.outputs(binding).unwrap().1)
            .collect();
        assert_eq!(
            outputs,
            [
                // The CTE's column list renames its columns, by position.
                vec![
                    named("a", Some((None, "nome"))),
                    named("b", Some((None, "email")))
                ],
                // A UNION takes its columns from its first branch.
                vec![Output::Star(None)],
                vec![named("renamed", Some((None, "nome")))],
            ]
        );
    }

    #[test]
    fn a_long_statement_is_read_within_budget() {
        // One pass over the tokens: about 100 KB, 6,000 columns and 100
        // joins, read in about 24 ms in a debug build (3.6 ms in release).
        let mut sql = String::from("SELECT ");
        for column in 0..6_000 {
            sql.push_str(&format!("t{}.column_{column}, ", column % 100));
        }
        sql.push_str("1 FROM orders t0");
        for join in 1..100 {
            sql.push_str(&format!(
                " JOIN sales.table_{join} t{join} ON t{join}.id = t0.id_{join}"
            ));
        }
        sql.push_str(" WHERE t0.created_at > now() AND ");
        let start = Instant::now();
        let scopes = Scopes::new(&sql, Some(SqlDialect::Postgres));
        let seen = scopes.visible(sql.len()).count();
        let elapsed = start.elapsed();
        assert_eq!(seen, 100);
        assert!(elapsed < Duration::from_millis(500), "took {elapsed:?}");
    }
}
