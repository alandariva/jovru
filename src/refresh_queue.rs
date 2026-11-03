//! The metadata refreshes a connection waits to run. No window needed.
//!
//! A connection runs one refresh at a time, so the cache is written by one
//! reader. Refreshes asked for meanwhile wait here, in order, instead of
//! being dropped: asking for a node already covered by one that waits does
//! nothing, and asking for a broader node takes the place of the waiting
//! nodes inside it. The refresh that is running is not part of the queue: it
//! may have read a node before the user asked again, so a node inside it
//! still waits for its own turn.
use crate::drivers::ObjectPath;
use crate::metadata_scope::MetadataScope;
use std::collections::VecDeque;

/// One refresh of a connection's metadata.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RefreshJob {
    /// Reads one node and everything inside it (`backend::refresh_metadata`).
    Node(ObjectPath),
    /// Reads `read` (the whole choice, or what a change added) and removes
    /// what the choice no longer covers (`backend::apply_selection`).
    Selection(MetadataScope),
}

/// What [`RefreshQueue::push`] did with a refresh.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Queued {
    /// It waits for the refreshes before it.
    Added,
    /// An equal or broader refresh already waits; nothing changed.
    Covered,
}

/// The refreshes waiting for the one that runs, in the order they run.
#[derive(Clone, Debug, Default)]
pub struct RefreshQueue {
    waiting: VecDeque<RefreshJob>,
}

impl RefreshQueue {
    /// Queues `job` behind the refreshes already waiting.
    ///
    /// A node inside a waiting node is covered by it; a node that holds
    /// waiting nodes replaces them. A choice equal to a waiting one is
    /// covered; different choices both wait, so neither read is lost.
    pub fn push(&mut self, job: RefreshJob) -> Queued {
        let covered = self.waiting.iter().any(|waiting| match (waiting, &job) {
            (RefreshJob::Node(broader), RefreshJob::Node(path)) => path.is_within(broader),
            (waiting, job) => waiting == job,
        });
        if covered {
            return Queued::Covered;
        }
        if let RefreshJob::Node(path) = &job {
            self.waiting.retain(
                |waiting| !matches!(waiting, RefreshJob::Node(inner) if inner.is_within(path)),
            );
        }
        self.waiting.push_back(job);
        Queued::Added
    }

    /// Takes the next refresh to run.
    pub fn pop(&mut self) -> Option<RefreshJob> {
        self.waiting.pop_front()
    }

    /// Whether a refresh of exactly this node waits.
    pub fn waits_for(&self, path: &ObjectPath) -> bool {
        self.waiting
            .iter()
            .any(|waiting| matches!(waiting, RefreshJob::Node(node) if node == path))
    }

    /// Whether a refresh of the chosen namespaces waits.
    pub fn waits_for_selection(&self) -> bool {
        self.waiting
            .iter()
            .any(|waiting| matches!(waiting, RefreshJob::Selection(_)))
    }

    /// Takes the waiting refresh of this node out of the queue. Returns
    /// whether one waited.
    pub fn remove(&mut self, path: &ObjectPath) -> bool {
        let before = self.waiting.len();
        self.waiting
            .retain(|waiting| !matches!(waiting, RefreshJob::Node(node) if node == path));
        self.waiting.len() != before
    }

    /// Takes the waiting refreshes of the chosen namespaces out of the queue.
    /// Returns whether one waited.
    pub fn remove_selection(&mut self) -> bool {
        let before = self.waiting.len();
        self.waiting
            .retain(|waiting| !matches!(waiting, RefreshJob::Selection(_)));
        self.waiting.len() != before
    }

    /// Forgets every waiting refresh, as when the connection changes.
    pub fn clear(&mut self) {
        self.waiting.clear();
    }

    pub fn len(&self) -> usize {
        self.waiting.len()
    }

    pub fn is_empty(&self) -> bool {
        self.waiting.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drivers::ObjectKind;

    fn schema(name: &str) -> ObjectPath {
        ObjectPath::default()
            .child(ObjectKind::Database, "db")
            .child(ObjectKind::Schema, name)
    }

    #[test]
    fn nodes_wait_in_the_order_they_were_asked() {
        let mut queue = RefreshQueue::default();
        assert_eq!(queue.push(RefreshJob::Node(schema("sales"))), Queued::Added);
        assert_eq!(queue.push(RefreshJob::Node(schema("hr"))), Queued::Added);
        assert!(queue.waits_for(&schema("hr")));
        assert_eq!(queue.pop(), Some(RefreshJob::Node(schema("sales"))));
        assert_eq!(queue.pop(), Some(RefreshJob::Node(schema("hr"))));
        assert_eq!(queue.pop(), None);
    }

    #[test]
    fn a_node_inside_a_waiting_one_is_covered_and_a_broader_one_replaces_them() {
        let mut queue = RefreshQueue::default();
        let orders = schema("sales").child(ObjectKind::Table, "orders");
        // A name that starts like the schema is not inside it.
        let sibling = schema("sales.archive");
        queue.push(RefreshJob::Node(orders.clone()));
        queue.push(RefreshJob::Node(sibling.clone()));
        assert_eq!(
            queue.push(RefreshJob::Node(orders.clone())),
            Queued::Covered
        );
        assert_eq!(queue.push(RefreshJob::Node(schema("sales"))), Queued::Added);
        assert!(!queue.waits_for(&orders));
        assert_eq!(queue.push(RefreshJob::Node(orders)), Queued::Covered);
        assert_eq!(queue.pop(), Some(RefreshJob::Node(sibling)));
        assert_eq!(queue.pop(), Some(RefreshJob::Node(schema("sales"))));
        assert!(queue.is_empty());
    }

    #[test]
    fn equal_choices_wait_once_and_different_ones_both_wait() {
        let mut queue = RefreshQueue::default();
        let everything = MetadataScope::everything();
        let sales = MetadataScope {
            all: false,
            include: vec![schema("sales")],
        };
        assert_eq!(
            queue.push(RefreshJob::Selection(everything.clone())),
            Queued::Added
        );
        assert_eq!(
            queue.push(RefreshJob::Selection(everything)),
            Queued::Covered
        );
        assert_eq!(queue.push(RefreshJob::Selection(sales)), Queued::Added);
        // A choice never covers a node, nor a node a choice.
        assert_eq!(queue.push(RefreshJob::Node(schema("sales"))), Queued::Added);
        assert_eq!(queue.len(), 3);
        assert!(queue.waits_for_selection());
        assert!(queue.remove_selection());
        assert!(!queue.waits_for_selection());
        assert_eq!(queue.len(), 1);
    }

    #[test]
    fn a_waiting_node_can_be_taken_out_and_the_queue_cleared() {
        let mut queue = RefreshQueue::default();
        queue.push(RefreshJob::Node(schema("sales")));
        queue.push(RefreshJob::Node(schema("hr")));
        assert!(queue.remove(&schema("sales")));
        assert!(!queue.remove(&schema("sales")));
        assert_eq!(queue.len(), 1);
        queue.clear();
        assert!(queue.is_empty());
    }
}
