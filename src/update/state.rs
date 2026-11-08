//! What the update is doing, as one value, and what the user can do then.
//! The status bar's badge and Preferences › About derive from it.
use std::path::PathBuf;

use crate::drivers::DriverError;

use super::download::Progress;
use super::layout::Installability;
use super::release::Candidate;

/// Where an update failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    Check,
    Download,
    Verify,
    Install,
}

/// The update's state.
#[derive(Clone, Debug, Default)]
pub enum UpdateState {
    /// Nothing checked in this session yet.
    #[default]
    Idle,
    Checking {
        /// Asked for with **Check now**.
        manual: bool,
    },
    /// The last check found nothing newer.
    UpToDate { checked_at_ms: u64 },
    Available {
        candidate: Candidate,
        install: Installability,
    },
    Downloading {
        candidate: Candidate,
        progress: Progress,
    },
    /// Checking the signature and unpacking the archive.
    Verifying { candidate: Candidate },
    /// Swapping the files; cannot be cancelled.
    Installing { candidate: Candidate },
    /// Installed; the new version runs after a restart.
    ReadyToRestart {
        version: String,
        restart_path: PathBuf,
    },
    Failed {
        error: DriverError,
        phase: Phase,
        /// The version being installed, when it was past the check.
        candidate: Option<Candidate>,
    },
}

/// What the user can do in a state.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Actions {
    pub check: bool,
    pub install: bool,
    pub cancel: bool,
    pub restart: bool,
    /// Open the release's page for its notes.
    pub notes: bool,
    /// Open the release's page to download it by hand.
    pub manual: bool,
}

impl UpdateState {
    pub fn actions(&self) -> Actions {
        match self {
            Self::Idle | Self::UpToDate { .. } => Actions {
                check: true,
                ..Actions::default()
            },
            Self::Checking { .. } | Self::Installing { .. } => Actions::default(),
            Self::Available { install, .. } => {
                let self_install = matches!(install, Installability::SelfInstall(_));
                Actions {
                    check: true,
                    install: self_install,
                    notes: true,
                    manual: !self_install,
                    ..Actions::default()
                }
            }
            Self::Downloading { .. } | Self::Verifying { .. } => Actions {
                cancel: true,
                ..Actions::default()
            },
            Self::ReadyToRestart { .. } => Actions {
                restart: true,
                notes: true,
                ..Actions::default()
            },
            Self::Failed { candidate, .. } => Actions {
                check: true,
                notes: candidate.is_some(),
                manual: candidate.is_some(),
                ..Actions::default()
            },
        }
    }

    /// The version being offered or installed.
    pub fn candidate(&self) -> Option<&Candidate> {
        match self {
            Self::Available { candidate, .. }
            | Self::Downloading { candidate, .. }
            | Self::Verifying { candidate }
            | Self::Installing { candidate } => Some(candidate),
            Self::Failed { candidate, .. } => candidate.as_ref(),
            Self::Idle
            | Self::Checking { .. }
            | Self::UpToDate { .. }
            | Self::ReadyToRestart { .. } => None,
        }
    }

    /// Whether a check, download or install is running.
    pub fn is_busy(&self) -> bool {
        matches!(
            self,
            Self::Checking { .. }
                | Self::Downloading { .. }
                | Self::Verifying { .. }
                | Self::Installing { .. }
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drivers::ErrorCode;
    use crate::update::layout::{InstallDir, ManualReason};
    use crate::update::release::release_page;

    fn candidate() -> Candidate {
        Candidate {
            version: "0.2.1".to_owned(),
            page: release_page("0.2.1"),
            asset: None,
        }
    }

    #[test]
    fn each_state_offers_its_actions() {
        assert!(UpdateState::Idle.actions().check);
        assert_eq!(
            UpdateState::Checking { manual: true }.actions(),
            Actions::default()
        );
        let installable = UpdateState::Available {
            candidate: candidate(),
            install: Installability::SelfInstall(InstallDir {
                dir: PathBuf::from("/opt/jykre"),
                exe: PathBuf::from("/opt/jykre/jykre"),
            }),
        };
        let actions = installable.actions();
        assert!(actions.install && actions.notes && !actions.manual);
        let manual = UpdateState::Available {
            candidate: candidate(),
            install: Installability::ManualOnly(ManualReason::NotWritable),
        };
        let actions = manual.actions();
        assert!(!actions.install && actions.manual);
        let downloading = UpdateState::Downloading {
            candidate: candidate(),
            progress: Progress::default(),
        };
        assert!(downloading.actions().cancel);
        assert_eq!(
            UpdateState::Installing {
                candidate: candidate()
            }
            .actions(),
            Actions::default(),
            "a swap in progress cannot be cancelled"
        );
        let ready = UpdateState::ReadyToRestart {
            version: "0.2.1".to_owned(),
            restart_path: PathBuf::from("/opt/jykre/jykre"),
        };
        assert!(ready.actions().restart);
        let failed = UpdateState::Failed {
            error: DriverError::new(ErrorCode::Protocol, "x"),
            phase: Phase::Verify,
            candidate: Some(candidate()),
        };
        let actions = failed.actions();
        assert!(actions.check && actions.manual && !actions.install);
    }
}
