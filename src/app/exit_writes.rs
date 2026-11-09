//! Writes that must finish before the application exits.
//!
//! GPUI releases a window's views before it runs the quit handlers: on
//! Windows and Linux, closing the last window drops the workspace and then
//! quits, so a handler the workspace registered with `on_app_quit` is gone by
//! the time the application shuts down. The workspace queues its last writes
//! here when it is released instead, and one handler registered for the whole
//! application awaits them, inside GPUI's shutdown timeout.

use gpui_kit::{App, Global};
use std::cell::RefCell;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;

type Write = Pin<Box<dyn Future<Output = ()>>>;

/// The writes queued so far. Shared by the global and the quit handler.
#[derive(Clone, Default)]
struct Queue(Rc<RefCell<Vec<Write>>>);

impl Queue {
    fn push(&self, write: impl Future<Output = ()> + 'static) {
        self.0.borrow_mut().push(Box::pin(write));
    }

    /// Awaits every write, in order, including those queued after this
    /// future was created and while it runs: when the application quits with
    /// the window still open, GPUI calls the quit handlers first and releases
    /// the window afterwards, before awaiting what the handlers returned.
    fn drain(&self) -> impl Future<Output = ()> + use<> {
        let queue = self.clone();
        async move {
            loop {
                let writes = std::mem::take(&mut *queue.0.borrow_mut());
                if writes.is_empty() {
                    return;
                }
                for write in writes {
                    write.await;
                }
            }
        }
    }
}

#[derive(Default)]
struct ExitWrites(Queue);

impl Global for ExitWrites {}

/// Registers the handler that awaits the queued writes when the application
/// quits. Called once, before the main window opens.
pub(super) fn install(cx: &mut App) {
    let queue = cx.default_global::<ExitWrites>().0.clone();
    cx.on_app_quit(move |_| {
        tracing::Instrument::instrument(queue.drain(), tracing::info_span!("quit"))
    })
    .detach();
}

/// Queues a write to finish before the application exits. It runs when the
/// application quits, not before.
pub(super) fn queue(cx: &mut App, write: impl Future<Output = ()> + 'static) {
    cx.default_global::<ExitWrites>().0.push(write);
}

#[cfg(test)]
mod tests {
    use super::Queue;
    use std::cell::RefCell;
    use std::rc::Rc;

    fn block_on(future: impl std::future::Future<Output = ()>) {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("runtime")
            .block_on(future);
    }

    #[test]
    fn the_quit_handler_awaits_writes_queued_after_it_was_called() {
        crate::app::source_text();
        let queue = Queue::default();
        let done = Rc::new(RefCell::new(Vec::new()));
        // GPUI calls the quit handler before it releases the window.
        let drained = queue.drain();
        let log = done.clone();
        let again = queue.clone();
        queue.push(async move {
            log.borrow_mut().push("state");
            let log = log.clone();
            again.push(async move { log.borrow_mut().push("queued while draining") });
        });
        let log = done.clone();
        queue.push(async move { log.borrow_mut().push("reports") });

        block_on(drained);

        assert_eq!(
            *done.borrow(),
            ["state", "reports", "queued while draining"]
        );
        assert!(queue.0.borrow().is_empty());
    }

    #[test]
    fn nothing_queued_finishes_at_once() {
        crate::app::source_text();
        block_on(Queue::default().drain());
    }
}
