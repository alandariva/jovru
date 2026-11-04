//! The queries of a statement and the relations each one reads, from the
//! dialect's tokens. The statement, every subquery in parentheses, every CTE
//! body and every `SELECT` branch (after `UNION`, `INSERT …`, `CREATE … AS` or
//! a `WITH` list) is a query that binds the relations of its own `FROM`,
//! `JOIN`, `USING`, `UPDATE` and `INTO`; a derived table binds its alias to
//! the relations its query reads.
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

/// A relation a query reads.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Binding {
    /// The relations it stands for, as written: one for a table or a view,
    /// those its own query reads for a derived table.
    pub(crate) names: Vec<Vec<Name>>,
    /// Its alias, unquoted.
    alias: Option<String>,
    /// A derived table answers only to its alias.
    derived: bool,
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
            None => {
                !self.derived
                    && self
                        .names
                        .first()
                        .and_then(|name| name.last())
                        .is_some_and(|last| last.text.to_lowercase() == qualifier)
            }
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
    /// A word was read at this query's own level.
    started: bool,
    /// A comma at this level continues a `FROM` list.
    from_list: bool,
    /// Inside a `WITH` list, where `name AS (` opens a CTE.
    with_list: bool,
    /// An open `SEMI` or `ANTI` join: its first binding and where it starts.
    semi: Option<(usize, usize)>,
}

/// Ends `query`'s open `SEMI` or `ANTI` join at byte `end`.
fn close_semi(query: &mut Query, end: usize) {
    if let Some((first, start)) = query.semi.take() {
        for binding in query.bindings.iter_mut().skip(first) {
            binding.only_in = Some(start..end);
        }
    }
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

/// The queries of a statement and the relations they read.
pub(crate) struct Scopes {
    queries: Vec<Query>,
    ctes: Vec<String>,
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
    pub(crate) fn ctes(&self) -> &[String] {
        &self.ctes
    }

    /// Whether `name` is one of the statement's CTEs rather than a relation.
    pub(crate) fn is_cte(&self, name: &[Name]) -> bool {
        match name {
            [only] => self
                .ctes
                .iter()
                .any(|cte| cte.eq_ignore_ascii_case(&only.text)),
            _ => false,
        }
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
        std::iter::successors(Some(innermost), |query| self.queries.get(*query)?.parent)
            .filter_map(|query| self.queries.get(query))
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

/// One pass over a statement's tokens.
struct Reader {
    ranges: Vec<Range<usize>>,
    parts: Vec<Part>,
    /// The statement's length.
    end: usize,
    queries: Vec<Query>,
    ctes: Vec<String>,
}

impl Reader {
    fn word(&self, at: usize) -> Option<&str> {
        match self.parts.get(at)? {
            Part::Word(upper, _) => Some(upper),
            _ => None,
        }
    }

    fn is(&self, at: usize, symbol: &str) -> bool {
        matches!(self.parts.get(at), Some(Part::Symbol(text)) if text == symbol)
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
        for query in &mut self.queries {
            let end = query.span.end;
            close_semi(query, end);
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
        if level
            && self.queries[current].with_list
            && let Some(name) = self.cte_name(at)
        {
            self.ctes.push(name);
        }
        let query = self.queries.len();
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
    /// binds its alias in the enclosing query.
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
            self.queries[*last].span.end = end;
        }
        if !derived {
            return at + 1;
        }
        let parent = current_query(frames);
        let mut names: Vec<Vec<Name>> = Vec::new();
        for binding in branches
            .iter()
            .flat_map(|branch| &self.queries[*branch].bindings)
        {
            for name in &binding.names {
                if !names.contains(name) {
                    names.push(name.clone());
                }
            }
        }
        let (alias, after) = self.alias_at(at + 1);
        self.queries[parent].bindings.push(Binding {
            names,
            alias,
            derived: true,
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
        match word {
            "SELECT" if !first => {
                self.branch(frames, current, start);
                at + 1
            }
            "WITH" => {
                self.queries[current].with_list = true;
                at + 1
            }
            // `IS DISTINCT FROM`, and `SELECT STRAIGHT_JOIN` (a hint).
            "FROM" if before.as_deref() == Some("DISTINCT") => at + 1,
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
    fn branch(&mut self, frames: &mut [Frame], current: usize, start: usize) {
        self.queries[current].span.end = start;
        let branch = self.queries.len();
        let parent = self.queries[current].parent;
        self.queries.push(Query {
            parent,
            span: start..self.end,
            started: true,
            ..Query::default()
        });
        if let Some(Frame::Query { branches, .. }) = frames.last_mut() {
            branches.push(branch);
        }
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
            names: vec![name],
            alias,
            derived: false,
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

    /// The CTE whose body opens at `at`:
    /// `name [(columns)] AS [[NOT] MATERIALIZED] (`.
    fn cte_name(&self, at: usize) -> Option<String> {
        let mut before = at.checked_sub(1)?;
        while matches!(self.word(before), Some("MATERIALIZED" | "NOT")) {
            before = before.checked_sub(1)?;
        }
        if self.word(before) != Some("AS") {
            return None;
        }
        before = before.checked_sub(1)?;
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
            before = before.checked_sub(1)?;
        }
        match self.parts.get(before)? {
            Part::Word(_, text) | Part::Quoted(text) => Some(text.clone()),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Scopes;
    use jykre_core::SqlDialect;
    use std::time::{Duration, Instant};

    /// The relations seen at the `‸` in `sql`: after `alias.`, the one it
    /// names; otherwise those of the query around the cursor. CTEs left out.
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
            .flat_map(|binding| &binding.names)
            .filter(|name| !scopes.is_cte(name))
            .map(|name| {
                name.iter()
                    .map(|part| part.text.as_str())
                    .collect::<Vec<_>>()
                    .join(".")
            })
            .collect()
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
        assert_eq!(scopes.ctes(), ["a", "b"]);
        assert_eq!(seen(sql), ["orders"]);
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
