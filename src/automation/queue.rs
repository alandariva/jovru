//! Admission limits and the queue (specification, sections 5.3 and 9.3.6):
//! a pure state machine the engine drives under its lock, so its rules are
//! tested without a runtime.
//!
//! At most [`Limits::global`] runs are active in the application and
//! [`Limits::per_connection`] per connection profile; program runs count
//! only towards the global limit. A run that finds the limits full waits in
//! a FIFO queue of at most [`Limits::queue_global`] runs, and
//! [`Limits::queue_per_connection`] per connection, for at most
//! [`Limits::queue_wait`]. A run whose connection is busy does not hold back
//! a later run on another connection. Queued runs live in memory only.
use jykre_core::ConnectionId;
use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// How many runs may be active and wait (sections 5.3 and 9.3.6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// Active runs in the whole application.
    pub global: usize,
    /// Active runs per connection profile. Two profiles that open the same
    /// SQLite file are not serialized.
    pub per_connection: usize,
    /// Runs waiting in the application.
    pub queue_global: usize,
    /// Runs waiting per connection profile.
    pub queue_per_connection: usize,
    /// The longest a run waits before it leaves the queue.
    pub queue_wait: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            global: 2,
            per_connection: 1,
            queue_global: 8,
            queue_per_connection: 4,
            queue_wait: Duration::from_secs(60),
        }
    }
}

/// Which limit a run met, and the active runs that hold it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Blocked {
    /// `limit` runs are active in the application.
    Global { limit: usize, holders: Vec<u64> },
    /// The connection has as many active runs as it may.
    Connection {
        connection: ConnectionId,
        holders: Vec<u64>,
    },
}

/// Why a run cannot even wait.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueueFull {
    /// `limit` runs already wait in the application.
    Global { limit: usize },
    /// `limit` runs already wait for this connection.
    Connection {
        connection: ConnectionId,
        limit: usize,
    },
}

/// What happens to a run asking to start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// It starts now and holds a slot until [`Admission::release`].
    Admit,
    /// It waits, at `position` (from 1), behind the limit it met.
    Queued { position: usize, blocked: Blocked },
    /// It cannot wait either.
    Refused(QueueFull),
}

#[derive(Debug, Clone)]
struct Waiting {
    ticket: u64,
    connection: Option<ConnectionId>,
    since: Instant,
}

/// The active and waiting runs, by ticket. A ticket is the engine's handle
/// for one run; this type never sees the run itself.
#[derive(Debug, Default)]
pub struct Admission {
    limits: Limits,
    active: Vec<(u64, Option<ConnectionId>)>,
    queue: VecDeque<Waiting>,
}

impl Admission {
    pub fn new(limits: Limits) -> Self {
        Self {
            limits,
            active: Vec::new(),
            queue: VecDeque::new(),
        }
    }

    pub fn limits(&self) -> Limits {
        self.limits
    }

    /// Asks to start `ticket` on `connection` (`None` for a program).
    pub fn request(
        &mut self,
        ticket: u64,
        connection: Option<ConnectionId>,
        now: Instant,
    ) -> Decision {
        // Every waiting run that could start has started (`release` starts
        // them), so a run that meets no limit passes nobody.
        let blocked = self.blocked(connection);
        let Some(blocked) = blocked else {
            self.active.push((ticket, connection));
            return Decision::Admit;
        };
        if self.queue.len() >= self.limits.queue_global {
            return Decision::Refused(QueueFull::Global {
                limit: self.limits.queue_global,
            });
        }
        if let Some(connection) = connection
            && self
                .queue
                .iter()
                .filter(|waiting| waiting.connection == Some(connection))
                .count()
                >= self.limits.queue_per_connection
        {
            return Decision::Refused(QueueFull::Connection {
                connection,
                limit: self.limits.queue_per_connection,
            });
        }
        self.queue.push_back(Waiting {
            ticket,
            connection,
            since: now,
        });
        Decision::Queued {
            position: self.queue.len(),
            blocked,
        }
    }

    /// Ends `ticket`'s slot, or takes it out of the queue, and starts the
    /// waiting runs that can start now, in FIFO order.
    pub fn release(&mut self, ticket: u64) -> Vec<u64> {
        self.active.retain(|(active, _)| *active != ticket);
        self.queue.retain(|waiting| waiting.ticket != ticket);
        self.start_waiting()
    }

    /// Takes `ticket` out of the queue (Stop on a queued run). `false` when
    /// it was not waiting.
    pub fn withdraw(&mut self, ticket: u64) -> bool {
        let before = self.queue.len();
        self.queue.retain(|waiting| waiting.ticket != ticket);
        before != self.queue.len()
    }

    /// Takes out every run that waited longer than allowed, oldest first.
    pub fn expire(&mut self, now: Instant) -> Vec<u64> {
        let wait = self.limits.queue_wait;
        let mut expired = Vec::new();
        self.queue.retain(|waiting| {
            let keep = now.saturating_duration_since(waiting.since) < wait;
            if !keep {
                expired.push(waiting.ticket);
            }
            keep
        });
        expired
    }

    /// Empties the queue (quitting), oldest first.
    pub fn drain_queue(&mut self) -> Vec<u64> {
        self.queue.drain(..).map(|waiting| waiting.ticket).collect()
    }

    /// When the oldest waiting run leaves the queue if nothing starts it.
    pub fn next_expiry(&self) -> Option<Instant> {
        self.queue
            .iter()
            .map(|waiting| waiting.since + self.limits.queue_wait)
            .min()
    }

    /// `ticket`'s place in the queue, from 1, and when it leaves it.
    pub fn position(&self, ticket: u64) -> Option<(usize, Instant)> {
        self.queue
            .iter()
            .position(|waiting| waiting.ticket == ticket)
            .map(|index| (index + 1, self.queue[index].since + self.limits.queue_wait))
    }

    /// What a waiting run is blocked by now.
    pub fn blocked_by(&self, ticket: u64) -> Option<Blocked> {
        let waiting = self.queue.iter().find(|waiting| waiting.ticket == ticket)?;
        Some(self.blocked(waiting.connection).unwrap_or(Blocked::Global {
            limit: self.limits.global,
            holders: self.holders(None),
        }))
    }

    pub fn active_count(&self) -> usize {
        self.active.len()
    }

    pub fn queued_count(&self) -> usize {
        self.queue.len()
    }

    pub fn is_active(&self, ticket: u64) -> bool {
        self.active.iter().any(|(active, _)| *active == ticket)
    }

    fn can_start(&self, connection: Option<ConnectionId>) -> bool {
        self.blocked(connection).is_none()
    }

    fn holders(&self, connection: Option<ConnectionId>) -> Vec<u64> {
        self.active
            .iter()
            .filter(|(_, active)| connection.is_none() || *active == connection)
            .map(|(ticket, _)| *ticket)
            .collect()
    }

    /// The limit `connection` meets now, ignoring the queue.
    fn blocked(&self, connection: Option<ConnectionId>) -> Option<Blocked> {
        if self.active.len() >= self.limits.global {
            return Some(Blocked::Global {
                limit: self.limits.global,
                holders: self.holders(None),
            });
        }
        let connection = connection?;
        let on_connection = self
            .active
            .iter()
            .filter(|(_, active)| *active == Some(connection))
            .count();
        (on_connection >= self.limits.per_connection).then(|| Blocked::Connection {
            connection,
            holders: self.holders(Some(connection)),
        })
    }

    fn start_waiting(&mut self) -> Vec<u64> {
        let mut started = Vec::new();
        let mut index = 0;
        while index < self.queue.len() {
            if self.active.len() >= self.limits.global {
                break;
            }
            let connection = self.queue[index].connection;
            if self.can_start(connection) {
                if let Some(waiting) = self.queue.remove(index) {
                    self.active.push((waiting.ticket, waiting.connection));
                    started.push(waiting.ticket);
                }
            } else {
                index += 1;
            }
        }
        started
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limits(global: usize) -> Limits {
        Limits {
            global,
            ..Limits::default()
        }
    }

    #[test]
    fn runs_beyond_the_limits_wait_and_start_in_order() {
        let now = Instant::now();
        let (a, b) = (ConnectionId::new(), ConnectionId::new());
        let mut admission = Admission::new(Limits::default());
        assert_eq!(admission.request(1, Some(a), now), Decision::Admit);
        // The connection is busy: waits behind run 1.
        assert_eq!(
            admission.request(2, Some(a), now),
            Decision::Queued {
                position: 1,
                blocked: Blocked::Connection {
                    connection: a,
                    holders: vec![1]
                }
            }
        );
        // Another connection passes the waiting run.
        assert_eq!(admission.request(3, Some(b), now), Decision::Admit);
        // Both slots are taken: a program waits for the global limit.
        assert!(matches!(
            admission.request(4, None, now),
            Decision::Queued {
                position: 2,
                blocked: Blocked::Global { limit: 2, .. }
            }
        ));
        // Run 3 ends: run 2 still cannot start (run 1 holds `a`), run 4 can.
        assert_eq!(admission.release(3), vec![4]);
        assert_eq!(admission.position(2).map(|(position, _)| position), Some(1));
        assert_eq!(admission.release(1), vec![2]);
        assert_eq!(admission.queued_count(), 0);
        assert_eq!(admission.active_count(), 2);
    }

    #[test]
    fn a_later_run_never_jumps_an_earlier_one_for_the_same_slot() {
        let now = Instant::now();
        let a = ConnectionId::new();
        let mut admission = Admission::new(limits(1));
        assert_eq!(admission.request(1, None, now), Decision::Admit);
        assert!(matches!(
            admission.request(2, Some(a), now),
            Decision::Queued { .. }
        ));
        admission.release(1);
        // Run 2 started; a new run on `a` waits behind it.
        assert!(admission.is_active(2));
        assert!(matches!(
            admission.request(3, Some(a), now),
            Decision::Queued { .. }
        ));
    }

    #[test]
    fn the_queue_holds_eight_runs_and_four_per_connection() {
        let now = Instant::now();
        let (a, b) = (ConnectionId::new(), ConnectionId::new());
        let mut admission = Admission::new(Limits::default());
        admission.request(100, Some(a), now);
        admission.request(101, Some(b), now);
        for ticket in 0..4 {
            assert!(matches!(
                admission.request(ticket, Some(a), now),
                Decision::Queued { .. }
            ));
        }
        assert_eq!(
            admission.request(4, Some(a), now),
            Decision::Refused(QueueFull::Connection {
                connection: a,
                limit: 4
            })
        );
        for ticket in 10..14 {
            assert!(matches!(
                admission.request(ticket, None, now),
                Decision::Queued { .. }
            ));
        }
        assert_eq!(
            admission.request(20, None, now),
            Decision::Refused(QueueFull::Global { limit: 8 })
        );
    }

    #[test]
    fn a_run_leaves_the_queue_after_sixty_seconds_or_when_withdrawn() {
        let start = Instant::now();
        let mut admission = Admission::new(limits(1));
        admission.request(1, None, start);
        admission.request(2, None, start);
        admission.request(3, None, start + Duration::from_secs(30));
        assert_eq!(
            admission.next_expiry(),
            Some(start + Duration::from_secs(60))
        );
        assert!(admission.expire(start + Duration::from_secs(59)).is_empty());
        assert_eq!(admission.expire(start + Duration::from_secs(60)), vec![2]);
        assert!(admission.withdraw(3));
        assert!(!admission.withdraw(3));
        assert_eq!(admission.release(1), Vec::<u64>::new());
    }

    #[test]
    fn a_global_limit_of_one_still_progresses() {
        let now = Instant::now();
        let mut admission = Admission::new(limits(1));
        let mut started = vec![];
        assert_eq!(admission.request(1, None, now), Decision::Admit);
        for ticket in 2..6 {
            admission.request(ticket, Some(ConnectionId::new()), now);
        }
        let mut current = 1;
        while let Some(next) = admission.release(current).first().copied() {
            started.push(next);
            current = next;
        }
        assert_eq!(started, vec![2, 3, 4, 5]);
        assert_eq!(admission.drain_queue(), Vec::<u64>::new());
    }
}
