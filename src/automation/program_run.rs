//! STUB.
use super::engine::{Ended, Engine, RunSpec};
use super::{Refusal, RunStatus};
use jykre_core::CancellationToken;
use jykre_storage::AutomationRepository;
use tokio::sync::watch;
pub(super) async fn run(
    _engine: &Engine,
    _repository: &AutomationRepository,
    _spec: RunSpec,
    _stop: &CancellationToken,
    _status: &watch::Sender<RunStatus>,
) -> Ended {
    Ended::Refused(Refusal::Missing)
}
