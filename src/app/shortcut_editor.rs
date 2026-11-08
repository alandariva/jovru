//! **Preferences › Shortcuts**: every action whose keys can be changed,
//! with the keys it has now. **Change** listens for the next keys pressed
//! (`Esc` cancels); a key another action or the editor already uses is
//! named before it is taken; **Reset** gives an action its default keys
//! back, and **Reset all** every action. A change applies at once and is
//! saved as `keymap.shortcuts` ([`super::shortcuts`]).
//!
//! While keys are being recorded the keymap is empty, so no action runs
//! instead and the keystroke reaches the recorder; leaving it (`Esc`, a
//! click elsewhere, a key chosen) puts the keymap back.
use super::shortcuts::{self, Group};
use super::ui::*;
use gpui_kit::assets::IconName as I;
use gpui_kit::{prelude::FluentBuilder as _, *};
use jykre::keymap::{self, Assigned, Key};
use jykre::tr;
use std::collections::HashMap;

/// A key that collides with other actions, waiting for the user's answer.
struct Pending {
    action: &'static str,
    key: Key,
    conflicts: Vec<Assigned>,
}

pub(super) struct ShortcutEditor {
    /// The action whose keys are being recorded.
    recording: Option<&'static str>,
    recorder: FocusHandle,
    pending: Option<Pending>,
    /// Each action's **Change** button, where the keyboard goes back to.
    change_focus: HashMap<&'static str, FocusHandle>,
    notice: Option<Feedback>,
    _blur: Option<Subscription>,
}

impl ShortcutEditor {
    pub(super) fn new(cx: &mut Context<Self>) -> Self {
        Self {
            recording: None,
            recorder: cx.focus_handle(),
            pending: None,
            change_focus: HashMap::new(),
            notice: None,
            _blur: None,
        }
    }

    fn change_focus(&mut self, action: &'static str, cx: &mut Context<Self>) -> FocusHandle {
        self.change_focus
            .entry(action)
            .or_insert_with(|| cx.focus_handle())
            .clone()
    }

    fn focus_change(&mut self, action: &'static str, window: &mut Window, cx: &mut Context<Self>) {
        let handle = self.change_focus(action, cx);
        handle.focus(window, cx);
    }

    /// **Change**: waits for the next keys pressed.
    fn start_recording(
        &mut self,
        action: &'static str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.pending = None;
        self.notice = None;
        self.recording = Some(action);
        shortcuts::suspend(cx);
        self.recorder.focus(window, cx);
        self._blur = Some(cx.on_blur(&self.recorder, window, |this, _, cx| {
            this.stop_recording(cx);
        }));
        cx.notify();
    }

    /// Stops listening and puts the keymap back.
    fn stop_recording(&mut self, cx: &mut Context<Self>) {
        self._blur = None;
        if self.recording.take().is_some() {
            shortcuts::apply(cx);
        }
        cx.notify();
    }

    /// A key pressed while recording.
    fn record(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let Some(action) = self.recording else {
            return;
        };
        window.prevent_default();
        cx.stop_propagation();
        let keystroke = &event.keystroke;
        if keystroke.key == "escape" && !keystroke.modifiers.modified() {
            self.stop_recording(cx);
            self.focus_change(action, window, cx);
            return;
        }
        // A modifier alone: the rest of the keys are still to come.
        let Some(key) = shortcuts::recorded_key(keystroke, cx) else {
            return;
        };
        self.stop_recording(cx);
        let assigned = shortcuts::assigned(cx);
        let conflicts: Vec<Assigned> =
            keymap::conflicts(&assigned, action, &shortcuts::scopes(action, cx), &key)
                .into_iter()
                .cloned()
                .collect();
        if conflicts.is_empty() {
            self.choose(action, key, &[], window, cx);
        } else {
            self.pending = Some(Pending {
                action,
                key,
                conflicts,
            });
            self.focus_change(action, window, cx);
            cx.notify();
        }
    }

    /// Gives `action` the key `key`, taking it from `take_from` first.
    fn choose(
        &mut self,
        action: &'static str,
        key: Key,
        take_from: &[Assigned],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let mut user = shortcuts::user_keys(cx);
        for other in take_from {
            let defaults = shortcuts::default_keys(&other.action, cx);
            user.take_key(&other.action, &other.keys, &key, &defaults);
        }
        let defaults = shortcuts::default_keys(action, cx);
        user.set(action, vec![key], &defaults);
        self.save(user, cx);
        self.focus_change(action, window, cx);
    }

    /// Applies the keys at once and writes them.
    fn save(&mut self, user: keymap::UserKeys, cx: &mut Context<Self>) {
        self.pending = None;
        shortcuts::set_user_keys(user.clone(), cx);
        let write = shortcuts::persist_user_keys(user, cx);
        cx.spawn(async move |weak, cx| {
            if let Err(error) = write.await {
                let _ = weak.update(cx, |this, cx| {
                    this.notice = Some(Feedback::new(
                        Tone::Error,
                        tr!(
                            "The shortcuts apply until the window closes, but could not be saved: {0}",
                            error
                        ),
                    ));
                    cx.notify();
                });
            }
        })
        .detach();
        cx.notify();
    }

    fn reset(&mut self, action: &'static str, window: &mut Window, cx: &mut Context<Self>) {
        let mut user = shortcuts::user_keys(cx);
        user.reset(action);
        self.save(user, cx);
        self.focus_change(action, window, cx);
    }

    fn reset_all(&mut self, cx: &mut Context<Self>) {
        let mut user = shortcuts::user_keys(cx);
        user.reset_all();
        self.save(user, cx);
    }

    /// The keys of one action, or what is happening to them.
    fn keys_cell(
        &mut self,
        index: usize,
        action: &'static str,
        title: &'static str,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        if self.recording == Some(action) {
            return div()
                .id("shortcut-recorder")
                .track_focus(&self.recorder)
                .role(Role::Status)
                .aria_label(tr!("Press the new keys for {0}. Esc cancels.", title))
                .px(px(8.))
                .py(px(4.))
                .rounded(px(4.))
                .border_1()
                .border_color(rgb(colors().mint))
                .text_size(px(13.))
                .child(tr!("Press the new keys… Esc cancels."))
                .on_key_down(cx.listener(Self::record))
                .into_any_element();
        }
        if let Some(pending) = self.pending.as_ref().filter(|p| p.action == action) {
            let names = pending
                .conflicts
                .iter()
                .map(|other| shortcuts::title_of(&other.action))
                .collect::<Vec<_>>()
                .join(", ");
            let fixed = pending.conflicts.iter().any(|other| !other.customizable);
            let text = if fixed {
                tr!(
                    "{0} is used by {1}, whose keys cannot be changed here. Choose another key.",
                    pending.key.label(),
                    names
                )
            } else {
                tr!(
                    "{0} is used by {1}. Take it from them?",
                    pending.key.label(),
                    names
                )
            };
            return column()
                .gap_1()
                .max_w(px(360.))
                .child(
                    div()
                        .id("shortcut-conflict")
                        .role(Role::Alert)
                        .text_size(px(13.))
                        .text_color(rgb(colors().amber))
                        .child(text),
                )
                .child(
                    row()
                        .gap_2()
                        .when(!fixed, |s| {
                            s.child(button("shortcut-take", I::Check, tr!("Take it")).on_click(
                                cx.listener(move |this, _, window, cx| {
                                    if let Some(pending) = this.pending.take() {
                                        this.choose(
                                            pending.action,
                                            pending.key,
                                            &pending.conflicts,
                                            window,
                                            cx,
                                        );
                                    }
                                }),
                            ))
                        })
                        .when(fixed, |s| {
                            s.child(
                                button("shortcut-again", I::Keyboard, tr!("Choose another key"))
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.start_recording(action, window, cx)
                                    })),
                            )
                        })
                        .child(button("shortcut-cancel", I::X, tr!("Cancel")).on_click(
                            cx.listener(move |this, _, window, cx| {
                                this.pending = None;
                                this.focus_change(action, window, cx);
                                cx.notify();
                            }),
                        )),
                )
                .into_any_element();
        }
        let keys = shortcuts::keys(action, cx);
        let chosen = shortcuts::user_keys(cx).is_chosen(action);
        let focus = self.change_focus(action, cx);
        row()
            .gap_2()
            .child(shortcut(
                keymap::keys_label(&keys).unwrap_or_else(|| tr!("None").to_owned()),
            ))
            .child(
                button(("shortcut-change", index), I::Keyboard, tr!("Change"))
                    .aria_label(tr!("Change the keys of {0}", title))
                    .track_focus(&focus)
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.start_recording(action, window, cx)
                    })),
            )
            .when(chosen, |s| {
                s.child(
                    button(("shortcut-reset", index), I::RotateCcw, tr!("Reset"))
                        .aria_label(tr!("Give {0} its default keys", title))
                        .on_click(
                            cx.listener(move |this, _, window, cx| this.reset(action, window, cx)),
                        ),
                )
            })
            .into_any_element()
    }
}

impl Render for ShortcutEditor {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let catalog: Vec<shortcuts::Entry> = shortcuts::catalog()
            .into_iter()
            .filter(|entry| shortcuts::is_customizable(entry.action, cx))
            .collect();
        let any_chosen = !shortcuts::user_keys(cx).is_empty();
        let mut page = column()
            .flex_none()
            .gap_4()
            .child(label(tr!(
                "Change an action's keys with Change, then press them. A key acts where the keyboard is, so the same key can do one thing in a SQL editor and another in the explorer, as Ctrl+F does by default."
            )))
            .when_some(self.notice.clone(), |s, notice| {
                s.child(
                    div()
                        .id("shortcut-notice")
                        .role(Role::Alert)
                        .text_size(px(13.))
                        .text_color(rgb(notice.tone.color()))
                        .child(notice.text),
                )
            })
            .when(any_chosen, |s| {
                s.child(
                    div().child(
                        button("shortcuts-reset-all", I::RotateCcw, tr!("Reset all shortcuts"))
                            .on_click(cx.listener(|this, _, _, cx| this.reset_all(cx))),
                    ),
                )
            });
        let mut index = 0;
        for group in Group::ALL {
            let entries: Vec<&shortcuts::Entry> = catalog
                .iter()
                .filter(|entry| entry.group == group)
                .collect();
            if entries.is_empty() {
                continue;
            }
            let mut section = setting_section(group.title());
            if let Some(detail) = group.detail() {
                section = section.child(label(detail));
            }
            let mut rows = column();
            for entry in entries {
                index += 1;
                let cell = self.keys_cell(index, entry.action, entry.title, cx);
                rows = rows.child(
                    row()
                        .flex_none()
                        .justify_between()
                        .gap_3()
                        .py(px(5.))
                        .border_b_1()
                        .border_color(rgb(colors().border))
                        .child(div().text_size(px(14.)).child(entry.title))
                        .child(cell),
                );
            }
            page = page.child(section.child(rows));
        }
        page
    }
}

/// A titled section of the page.
fn setting_section(title: &'static str) -> Div {
    column().flex_none().gap_2().child(
        div()
            .text_size(px(12.))
            .font_weight(FontWeight::SEMIBOLD)
            .text_color(rgb(colors().faint))
            .child(title.to_uppercase()),
    )
}
