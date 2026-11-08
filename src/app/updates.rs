//! Updates ([ADR-0033](../../docs/adr/0033-update-check-and-signed-self-install.md)):
//! the daily check, **Preferences › About › Updates** and the status bar's
//! badge. What to install and how lives in `jykre::update`; this module
//! starts the work, keeps its state and draws it.
use super::ui::*;
use gpui_kit::assets::IconName as I;
use gpui_kit::{prelude::FluentBuilder as _, *};
use jykre::drivers::{CancellationToken, ErrorCode};
use jykre::update::{
    self, Candidate, Endpoints, Installability, ManualReason, Phase, Platform, Progress,
    UpdatePreferences, UpdateRecord, UpdateState,
};
use jykre::{backend, connection_store, tr};
use std::path::PathBuf;
use std::time::Duration;
use tokio::sync::watch;

/// What the updater asks of the window.
pub(super) enum UpdaterEvent {
    /// Show Preferences › About, where the update is.
    OpenUpdates,
    /// Close the window as closing asks, then start `0`.
    RestartRequested(PathBuf),
}

/// The update's state for the whole window, and the work running for it.
pub(super) struct Updater {
    state: UpdateState,
    preferences: UpdatePreferences,
    record: UpdateRecord,
    /// Whether the preferences were read; the schedule waits for them.
    loaded: bool,
    /// Bumped by each check or install, so a stale answer is dropped.
    generation: u64,
    cancel: Option<CancellationToken>,
    /// The next automatic check; dropping it cancels the wait.
    schedule: Option<Task<()>>,
    /// Following a download's progress and phase.
    watching: Vec<Task<()>>,
}

impl EventEmitter<UpdaterEvent> for Updater {}

impl Updater {
    pub(super) fn new(cx: &mut Context<Self>) -> Self {
        let mut updater = Self {
            state: UpdateState::Idle,
            preferences: UpdatePreferences::default(),
            record: UpdateRecord::default(),
            loaded: false,
            generation: 0,
            cancel: None,
            schedule: None,
            watching: Vec::new(),
        };
        updater.load(cx);
        updater
    }

    pub(super) fn state(&self) -> &UpdateState {
        &self.state
    }

    /// Reads the preferences and the last check, removes what earlier
    /// updates left next to the executable, then schedules the check.
    fn load(&mut self, cx: &mut Context<Self>) {
        let dir = connection_store::data_dir();
        cx.spawn(async move |this, cx| {
            let read = backend::run(async move {
                if let Some(folder) = exe_folder() {
                    let _ = tokio::task::spawn_blocking(move || {
                        update::swap::cleanup_leftovers(&folder, Duration::from_secs(60 * 60));
                    })
                    .await;
                }
                update::load(&dir).await
            })
            .await;
            let _ = this.update(cx, |this, cx| {
                match read {
                    Ok((preferences, record)) => {
                        this.preferences = preferences;
                        this.record = record;
                    }
                    Err(error) => {
                        // Checks stay on, with no last check: the default.
                        tracing::warn!(error_code = ?error.code, "update preferences unreadable");
                    }
                }
                this.loaded = true;
                this.reschedule(cx);
                cx.notify();
            });
        })
        .detach();
    }

    /// Plans the next automatic check from the last one, or none.
    fn reschedule(&mut self, cx: &mut Context<Self>) {
        let wait = update::next_check_in(
            update::now_ms(),
            &self.record,
            &self.preferences,
            update::automatic_checks(),
        );
        self.schedule_in(wait, cx);
    }

    fn schedule_in(&mut self, wait: Option<Duration>, cx: &mut Context<Self>) {
        self.schedule = wait.map(|wait| {
            cx.spawn(async move |this, cx| {
                cx.background_executor().timer(wait).await;
                let _ = this.update(cx, |this, cx| this.check(false, cx));
            })
        });
    }

    /// Checks now: **Check now** (`manual`), or the schedule. An automatic
    /// check never replaces an offer, an install or a pending restart.
    pub(super) fn check(&mut self, manual: bool, cx: &mut Context<Self>) {
        if !self.loaded || self.state.is_busy() {
            return;
        }
        if !manual
            && matches!(
                self.state,
                UpdateState::Available { .. } | UpdateState::ReadyToRestart { .. }
            )
        {
            self.reschedule(cx);
            return;
        }
        self.generation += 1;
        let generation = self.generation;
        let cancel = CancellationToken::new();
        self.cancel = Some(cancel.clone());
        self.state = UpdateState::Checking { manual };
        self.schedule = None;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let answer = backend::run(async move {
                let platform = Platform::current();
                let found = update::check(
                    &Endpoints::github(),
                    env!("CARGO_PKG_VERSION"),
                    platform,
                    &cancel,
                )
                .await;
                let install = match &found {
                    Ok(Some(candidate)) => {
                        let has_asset = candidate.asset.is_some();
                        let release_build = !cfg!(debug_assertions);
                        tokio::task::spawn_blocking(move || match std::env::current_exe() {
                            Ok(exe) => {
                                update::layout::detect(&exe, platform, has_asset, release_build)
                            }
                            Err(_) => Installability::ManualOnly(ManualReason::NotReleaseLayout),
                        })
                        .await
                        .unwrap_or(Installability::ManualOnly(ManualReason::NotReleaseLayout))
                    }
                    _ => Installability::ManualOnly(ManualReason::NoAsset),
                };
                Ok((found, install))
            })
            .await;
            let _ = this.update(cx, |this, cx| {
                if this.generation != generation {
                    return;
                }
                this.cancel = None;
                this.finish_check(answer, cx);
                cx.notify();
            });
        })
        .detach();
    }

    fn finish_check(
        &mut self,
        answer: jykre::drivers::DriverResult<(
            Result<Option<Candidate>, update::CheckFailure>,
            Installability,
        )>,
        cx: &mut Context<Self>,
    ) {
        let now = update::now_ms();
        let found = match answer {
            Ok((found, install)) => found.map(|candidate| (candidate, install)),
            Err(error) => Err(update::CheckFailure {
                error,
                retry_in: update::RETRY_AFTER_FAILURE,
            }),
        };
        match found {
            Ok((candidate, install)) => {
                self.state = match candidate {
                    Some(candidate) => UpdateState::Available { candidate, install },
                    None => UpdateState::UpToDate { checked_at_ms: now },
                };
                self.record = UpdateRecord {
                    last_checked_ms: Some(now),
                };
                let (dir, record) = (connection_store::data_dir(), self.record);
                let ticket = update::RECORD_WRITES.ask();
                cx.spawn(async move |_, _| {
                    if let Err(error) =
                        backend::run(async move { update::save_record(&dir, record, ticket).await })
                            .await
                    {
                        tracing::warn!(error_code = ?error.code, "update record not saved");
                    }
                })
                .detach();
                self.reschedule(cx);
            }
            Err(failure) if failure.error.code == ErrorCode::Cancelled => {
                self.state = UpdateState::Idle;
                self.reschedule(cx);
            }
            Err(failure) => {
                // Shown in About; the badge stays quiet about checks.
                self.state = UpdateState::Failed {
                    error: failure.error,
                    phase: Phase::Check,
                    candidate: None,
                };
                let automatic = update::automatic_checks() && self.preferences.check;
                self.schedule_in(automatic.then_some(failure.retry_in), cx);
            }
        }
    }

    /// **Download and install** the version offered.
    pub(super) fn install(&mut self, cx: &mut Context<Self>) {
        let UpdateState::Available {
            candidate,
            install: Installability::SelfInstall(dir),
        } = &self.state
        else {
            return;
        };
        let Some(platform) = Platform::current() else {
            return;
        };
        let (candidate, dir) = (candidate.clone(), dir.clone());
        self.generation += 1;
        let generation = self.generation;
        let cancel = CancellationToken::new();
        self.cancel = Some(cancel.clone());
        let (progress, mut progress_rx) = watch::channel(Progress::default());
        let (phase, mut phase_rx) = watch::channel(Phase::Download);
        self.state = UpdateState::Downloading {
            candidate: candidate.clone(),
            progress: Progress::default(),
        };
        // Redrawn when the download reports, never on a timer.
        self.watching = vec![
            cx.spawn(async move |this, cx| {
                while progress_rx.changed().await.is_ok() {
                    let received = *progress_rx.borrow_and_update();
                    let alive = this.update(cx, |this, cx| {
                        if let UpdateState::Downloading { progress, .. } = &mut this.state {
                            *progress = received;
                            cx.notify();
                        }
                    });
                    if alive.is_err() {
                        return;
                    }
                }
            }),
            cx.spawn(async move |this, cx| {
                while phase_rx.changed().await.is_ok() {
                    let phase = *phase_rx.borrow_and_update();
                    let alive = this.update(cx, |this, cx| {
                        let Some(candidate) = this.state.candidate().cloned() else {
                            return;
                        };
                        match phase {
                            Phase::Verify => this.state = UpdateState::Verifying { candidate },
                            Phase::Install => this.state = UpdateState::Installing { candidate },
                            Phase::Check | Phase::Download => {}
                        }
                        cx.notify();
                    });
                    if alive.is_err() {
                        return;
                    }
                }
            }),
        ];
        cx.notify();
        let task_candidate = candidate.clone();
        let task_dir = dir.clone();
        cx.spawn(async move |this, cx| {
            let reports = update::Reports { progress, phase };
            let result = backend::run(async move {
                update::install(
                    &Endpoints::github(),
                    &task_candidate,
                    &task_dir,
                    platform,
                    reports,
                    cancel,
                )
                .await
            })
            .await;
            let _ = this.update(cx, |this, cx| {
                if this.generation != generation {
                    return;
                }
                let phase = match &this.state {
                    UpdateState::Verifying { .. } => Phase::Verify,
                    UpdateState::Installing { .. } => Phase::Install,
                    _ => Phase::Download,
                };
                this.cancel = None;
                this.watching.clear();
                this.state = match result {
                    Ok(installed) => UpdateState::ReadyToRestart {
                        version: installed.version,
                        restart_path: installed.restart_path,
                    },
                    Err(error) if error.code == ErrorCode::Cancelled => UpdateState::Available {
                        candidate,
                        install: Installability::SelfInstall(dir),
                    },
                    Err(error) => UpdateState::Failed {
                        error,
                        phase,
                        candidate: Some(candidate),
                    },
                };
                cx.notify();
            });
        })
        .detach();
    }

    /// Stops the check or the download running; the swap is not stopped.
    pub(super) fn cancel(&mut self) {
        if let Some(cancel) = self.cancel.take() {
            cancel.cancel();
        }
    }

    pub(super) fn checks_automatically(&self) -> bool {
        self.preferences.check
    }

    /// Turns the automatic check on or off, and saves the choice.
    pub(super) fn set_checks(&mut self, on: bool, cx: &mut Context<Self>) {
        self.preferences = UpdatePreferences { check: on };
        let (dir, preferences) = (connection_store::data_dir(), self.preferences);
        let ticket = update::PREFERENCE_WRITES.ask();
        cx.spawn(async move |_, _| {
            if let Err(error) =
                backend::run(
                    async move { update::save_preferences(&dir, preferences, ticket).await },
                )
                .await
            {
                tracing::warn!(error_code = ?error.code, "update preference not saved");
            }
        })
        .detach();
        if !self.state.is_busy() {
            self.reschedule(cx);
        }
        cx.notify();
    }

    /// **Restart now**: the window closes as closing asks, then the new
    /// version starts.
    pub(super) fn restart(&mut self, cx: &mut Context<Self>) {
        if let UpdateState::ReadyToRestart { restart_path, .. } = &self.state {
            cx.emit(UpdaterEvent::RestartRequested(restart_path.clone()));
        }
    }

    /// Opens the release's page, for its notes or to download it by hand.
    pub(super) fn open_release_page(&self, cx: &mut App) {
        let page = match &self.state {
            UpdateState::ReadyToRestart { version, .. } => {
                Some(update::release::release_page(version))
            }
            state => state.candidate().map(|candidate| candidate.page.clone()),
        };
        if let Some(page) = page {
            cx.open_url(&page);
        }
    }
}

/// The folder of the running executable, where an update's leftovers are.
fn exe_folder() -> Option<PathBuf> {
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(PathBuf::from))
}

/// Why an update is offered as a download only.
fn manual_reason(reason: ManualReason) -> &'static str {
    match reason {
        ManualReason::DevelopmentBuild => {
            tr!("A development build is not replaced; download the release from its page.")
        }
        ManualReason::Platform | ManualReason::NoAsset => {
            tr!("The release has no archive for this system; see its page.")
        }
        ManualReason::NotReleaseLayout => tr!(
            "This copy is not in the folder the release ships, so it is not replaced; download the release from its page."
        ),
        ManualReason::NotWritable => tr!(
            "Jykre Studio cannot write to its folder; download the release and replace the folder by hand."
        ),
    }
}

/// The state as a line for About.
fn status(state: &UpdateState) -> Feedback {
    match state {
        UpdateState::Idle => Feedback::new(
            Tone::Info,
            tr!("Not checked yet since Jykre Studio started."),
        ),
        UpdateState::Checking { .. } => Feedback::new(Tone::Info, tr!("Checking for updates…")),
        UpdateState::UpToDate { .. } => {
            Feedback::new(Tone::Success, tr!("Jykre Studio is up to date."))
        }
        UpdateState::Available { candidate, install } => {
            let available = tr!("Version {0} is available.", candidate.version);
            match install {
                Installability::SelfInstall(_) => Feedback::new(Tone::Info, available),
                Installability::ManualOnly(reason) => Feedback::new(
                    Tone::Warning,
                    format!("{available} {}", manual_reason(*reason)),
                ),
            }
        }
        UpdateState::Downloading {
            candidate,
            progress,
        } => Feedback::new(
            Tone::Info,
            match progress.percent() {
                Some(percent) => tr!("Downloading version {0}… {1} %", candidate.version, percent),
                None => tr!(
                    "Downloading version {0}… {1} MB",
                    candidate.version,
                    progress.received / (1024 * 1024)
                ),
            },
        ),
        UpdateState::Verifying { candidate } => Feedback::new(
            Tone::Info,
            tr!("Checking the signature of version {0}…", candidate.version),
        ),
        UpdateState::Installing { candidate } => Feedback::new(
            Tone::Info,
            tr!("Installing version {0}…", candidate.version),
        ),
        UpdateState::ReadyToRestart { version, .. } => Feedback::new(
            Tone::Success,
            tr!(
                "Version {0} is installed; restart Jykre Studio to use it.",
                version
            ),
        ),
        UpdateState::Failed {
            error,
            phase: Phase::Check,
            ..
        } => Feedback::new(Tone::Error, tr!("The update check failed: {0}", error)),
        UpdateState::Failed {
            error, candidate, ..
        } => Feedback::new(
            Tone::Error,
            tr!(
                "Version {0} was not installed: {1}",
                candidate
                    .as_ref()
                    .map(|candidate| candidate.version.as_str())
                    .unwrap_or("?"),
                error
            ),
        ),
    }
}

/// **Preferences › About › Updates**.
pub(super) struct UpdateSettingsView {
    updater: Entity<Updater>,
    _observation: Subscription,
}

impl UpdateSettingsView {
    pub(super) fn new(updater: Entity<Updater>, cx: &mut Context<Self>) -> Self {
        let observation = cx.observe(&updater, |_, _, cx| cx.notify());
        Self {
            updater,
            _observation: observation,
        }
    }
}

impl Render for UpdateSettingsView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let updater = self.updater.read(cx);
        let state = updater.state();
        let actions = state.actions();
        let on = updater.checks_automatically();
        let status = status(state);
        let updater = self.updater.clone();
        let act = move |action: fn(&mut Updater, &mut Context<Updater>)| {
            let updater = updater.clone();
            move |_: &ClickEvent, _: &mut Window, cx: &mut App| {
                updater.update(cx, action);
            }
        };
        // Its own height inside the scrolling Preferences column.
        column()
            .flex_none()
            .w_full()
            .gap_2()
            .child(
                row().child(
                    button(
                        "update-check-automatically",
                        if on { I::SquareCheck } else { I::Square },
                        tr!("Check for updates automatically"),
                    )
                    .role(Role::CheckBox)
                    .aria_toggled(if on { Toggled::True } else { Toggled::False })
                    .when(on, |s| s.text_color(rgb(colors().mint)))
                    .on_click(act(|this, cx| {
                        let on = !this.checks_automatically();
                        this.set_checks(on, cx);
                    })),
                ),
            )
            .child(label(tr!(
                "Once a day Jykre Studio asks GitHub, without identifying you, whether a newer version exists; GitHub sees this computer's address and the version. Nothing is downloaded without your click, and only an archive signed with the release key is installed."
            )))
            .child(
                div()
                    .id("update-status")
                    .role(Role::Status)
                    .child(feedback_line(&status)),
            )
            .child(
                row()
                    .flex_wrap()
                    .gap_1()
                    .when(actions.install, |s| {
                        s.child(
                            primary_button("update-install", I::Download, tr!("Download and install"))
                                .on_click(act(Updater::install)),
                        )
                    })
                    .when(actions.restart, |s| {
                        s.child(
                            primary_button("update-restart", I::RotateCcw, tr!("Restart now"))
                                .on_click(act(Updater::restart)),
                        )
                    })
                    .when(actions.cancel, |s| {
                        s.child(
                            button("update-cancel", I::Close, tr!("Cancel"))
                                .on_click(act(|this, _| this.cancel())),
                        )
                    })
                    .when(actions.check, |s| {
                        s.child(
                            button("update-check-now", I::RefreshCw, tr!("Check now"))
                                .on_click(act(|this, cx| this.check(true, cx))),
                        )
                    })
                    .when(actions.manual, |s| {
                        s.child(
                            button("update-download-manually", I::ExternalLink, tr!("Download manually"))
                                .on_click(act(|this, cx| this.open_release_page(cx))),
                        )
                    })
                    .when(actions.notes && !actions.manual, |s| {
                        s.child(
                            button("update-release-notes", I::ExternalLink, tr!("Release notes"))
                                .on_click(act(|this, cx| this.open_release_page(cx))),
                        )
                    }),
            )
    }
}

/// The status bar's badge: shown only while there is something to do or
/// see (a version available, a download, a restart to finish it).
pub(super) struct UpdateBadge {
    updater: Entity<Updater>,
    _observation: Subscription,
}

impl UpdateBadge {
    pub(super) fn new(updater: Entity<Updater>, cx: &mut Context<Self>) -> Self {
        let observation = cx.observe(&updater, |_, _, cx| cx.notify());
        Self {
            updater,
            _observation: observation,
        }
    }
}

impl Render for UpdateBadge {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let text = match self.updater.read(cx).state() {
            UpdateState::Available { candidate, .. } => {
                Some(tr!("Version {0} available", candidate.version))
            }
            UpdateState::Downloading { progress, .. } => Some(match progress.percent() {
                Some(percent) => tr!("Downloading update · {0} %", percent),
                None => tr!("Downloading update").to_owned(),
            }),
            UpdateState::Verifying { .. } | UpdateState::Installing { .. } => {
                Some(tr!("Installing update…").to_owned())
            }
            UpdateState::ReadyToRestart { .. } => Some(tr!("Restart to update").to_owned()),
            UpdateState::Idle
            | UpdateState::Checking { .. }
            | UpdateState::UpToDate { .. }
            | UpdateState::Failed { .. } => None,
        };
        let updater = self.updater.clone();
        match text {
            Some(text) => div()
                .id("update-badge")
                .role(Role::Button)
                .aria_label(text.clone())
                .focusable()
                .tab_index(0)
                .cursor_pointer()
                .px_2()
                .rounded(px(4.))
                .text_size(px(12.))
                .text_color(rgb(colors().on_accent))
                .bg(rgb(colors().mint))
                .hover(|s| s.bg(rgb(colors().accent_hover)))
                .focus_visible(|s| s.border_1().border_color(rgb(colors().text)))
                .child(text)
                .on_click(move |_, _, cx| {
                    updater.update(cx, |_, cx| cx.emit(UpdaterEvent::OpenUpdates));
                })
                .into_any_element(),
            None => div().into_any_element(),
        }
    }
}
