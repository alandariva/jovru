//! Which paths a performance run measures: `--test-performance` alone runs
//! every one, `--test-performance=grid,editor` only those, so one path can
//! be measured again on its own.

/// The command-line switch of the performance run.
pub const FLAG: &str = "--test-performance";

/// A UI path the run measures, in the order it runs them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum PathId {
    /// The result grid's synthetic sizes, A14's wide one with frozen columns.
    Grid,
    /// Go to a line in a large SQL text (A9).
    Editor,
    /// Pending edits in the result grid (A10).
    Edit,
    /// Finding a column among thousands (A15).
    Columns,
    /// Importing a CSV and an `.xlsx` file into a table (A11).
    Import,
    /// The Automation interface (A6).
    Automation,
}

impl PathId {
    pub const ALL: [PathId; 6] = [
        PathId::Grid,
        PathId::Editor,
        PathId::Edit,
        PathId::Columns,
        PathId::Import,
        PathId::Automation,
    ];

    /// The name the switch takes.
    pub fn name(self) -> &'static str {
        match self {
            PathId::Grid => "grid",
            PathId::Editor => "editor",
            PathId::Edit => "edit",
            PathId::Columns => "columns",
            PathId::Import => "import",
            PathId::Automation => "automation",
        }
    }

    /// What the report calls it.
    pub fn title(self) -> &'static str {
        match self {
            PathId::Grid => "Result grid (A14)",
            PathId::Editor => "Go to a line (A9)",
            PathId::Edit => "Editing in the grid (A10)",
            PathId::Columns => "Finding a column (A15)",
            PathId::Import => "Importing a file (A11)",
            PathId::Automation => "Automation interface (A6)",
        }
    }

    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|path| path.name() == name)
    }
}

/// Every name the switch takes, for a message.
pub fn names() -> String {
    PathId::ALL
        .iter()
        .map(|path| path.name())
        .collect::<Vec<_>>()
        .join(", ")
}

/// What `argument` asks for: `None` when it is not the switch, the paths
/// (in run order, each once) when it is, or why the list cannot be read.
pub fn from_argument(argument: &str) -> Option<Result<Vec<PathId>, String>> {
    let rest = argument.strip_prefix(FLAG)?;
    if rest.is_empty() {
        return Some(Ok(PathId::ALL.to_vec()));
    }
    let list = rest.strip_prefix('=')?;
    let mut paths = Vec::new();
    for name in list.split(',').map(str::trim) {
        if name.is_empty() {
            continue;
        }
        match PathId::parse(&name.to_ascii_lowercase()) {
            Some(path) => paths.push(path),
            None => {
                return Some(Err(format!(
                    "Unknown path `{name}` for {FLAG}; choose from {}.",
                    names()
                )));
            }
        }
    }
    if paths.is_empty() {
        return Some(Err(format!(
            "{FLAG}= names no path; choose from {}.",
            names()
        )));
    }
    paths.sort_unstable();
    paths.dedup();
    Some(Ok(paths))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_switch_alone_runs_every_path() {
        assert_eq!(from_argument(FLAG), Some(Ok(PathId::ALL.to_vec())));
        assert_eq!(from_argument("--other"), None);
        assert_eq!(from_argument("--test-performances"), None);
    }

    #[test]
    fn a_list_runs_its_paths_once_in_run_order() {
        assert_eq!(
            from_argument("--test-performance=automation,grid, editor,grid"),
            Some(Ok(vec![PathId::Grid, PathId::Editor, PathId::Automation]))
        );
        assert_eq!(
            from_argument("--test-performance=Import"),
            Some(Ok(vec![PathId::Import]))
        );
    }

    #[test]
    fn an_unknown_or_empty_list_is_refused_with_the_names() {
        let Some(Err(unknown)) = from_argument("--test-performance=grid,frames") else {
            unreachable!("refused");
        };
        assert!(
            unknown.contains("frames") && unknown.contains("columns"),
            "{unknown}"
        );
        assert!(matches!(from_argument("--test-performance="), Some(Err(_))));
        assert!(matches!(
            from_argument("--test-performance=,"),
            Some(Err(_))
        ));
    }
}
