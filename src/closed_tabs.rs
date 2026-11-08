//! Query tabs closed in this application session, so **Reopen closed query
//! tab** (`Ctrl Shift T`) brings them back, the most recent first (0.2.0
//! M8).
//!
//! They live in memory only and never survive a restart. Each keeps what a
//! tab's saved state keeps ([`TabState`]: its SQL, title, connection,
//! context, timeout and `:name` fields), never results, a session or a
//! transaction, so reopening one runs nothing and promises nothing about
//! work that closing rolled back.
use crate::workspace_state::TabState;

/// How many closed tabs are kept; the oldest is forgotten past it.
pub const LIMIT: usize = 20;

/// The closed query tabs, oldest first.
#[derive(Clone, Debug, Default)]
pub struct ClosedTabs(Vec<TabState>);

impl ClosedTabs {
    /// Remembers a tab that closed. A tab with nothing to bring back (blank
    /// SQL and no `:name` value typed) is not kept, so reopening never
    /// brings an empty tab in place of one that mattered.
    pub fn push(&mut self, tab: TabState) {
        let typed = tab
            .parameters
            .iter()
            .any(|parameter| !parameter.text.is_empty() || parameter.null);
        if tab.sql.trim().is_empty() && !typed {
            return;
        }
        self.0.push(tab);
        if self.0.len() > LIMIT {
            self.0.remove(0);
        }
    }

    /// The most recently closed tab, taken from the list.
    pub fn pop(&mut self) -> Option<TabState> {
        self.0.pop()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parameters::{KindChoice, TabParameter};

    fn tab(sql: &str) -> TabState {
        TabState {
            title: sql.to_owned(),
            sql: sql.to_owned(),
            ..TabState::default()
        }
    }

    #[test]
    fn the_most_recent_comes_back_first_and_the_list_is_bounded() {
        let mut closed = ClosedTabs::default();
        assert!(closed.pop().is_none());
        for n in 0..LIMIT + 3 {
            closed.push(tab(&format!("SELECT {n}")));
        }
        assert_eq!(closed.len(), LIMIT);
        assert_eq!(
            closed.pop().map(|tab| tab.sql),
            Some(format!("SELECT {}", LIMIT + 2))
        );
        // The oldest were forgotten; the rest come back newest first.
        let mut rest = Vec::new();
        while let Some(tab) = closed.pop() {
            rest.push(tab.sql);
        }
        assert_eq!(rest.len(), LIMIT - 1);
        assert_eq!(rest.last().map(String::as_str), Some("SELECT 3"));
        assert!(closed.is_empty());
    }

    #[test]
    fn a_tab_with_nothing_to_bring_back_is_not_kept() {
        let mut closed = ClosedTabs::default();
        closed.push(tab("  \n "));
        assert!(closed.is_empty());
        // A value typed in a `:name` field is something to bring back.
        let mut fields = tab("");
        fields.parameters = vec![TabParameter {
            name: "day".into(),
            kind: KindChoice::Auto,
            text: "2026-11-07".into(),
            null: false,
        }];
        closed.push(fields.clone());
        assert_eq!(closed.pop(), Some(fields));
    }
}
