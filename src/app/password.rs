//! The password prompt. A connection whose password is not remembered asks
//! for it when a session opens (a run or a refresh), instead of failing to
//! sign in. The password stays in memory until the application closes, or
//! goes to the system vault when the user asks to remember it.
use super::*;

/// What runs once the password is in memory.
pub(super) type AfterPassword = Box<dyn FnOnce(&mut AppView, &mut Window, &mut Context<AppView>)>;
/// What runs when the user closes the prompt without a password.
pub(super) type WithoutPassword = Box<dyn FnOnce(&mut AppView, &mut Context<AppView>)>;

/// An open prompt and what waits for it.
pub(super) struct PasswordPrompt {
    /// The connection, by index.
    index: usize,
    then: AfterPassword,
    cancel: WithoutPassword,
}

impl AppView {
    /// Asks for the password of the connection at `index`, then runs `then`.
    /// A second request while a prompt is open is turned down (`cancel`),
    /// so one prompt never hides another.
    pub(super) fn ask_password(
        &mut self,
        index: usize,
        then: AfterPassword,
        cancel: WithoutPassword,
        cx: &mut Context<Self>,
    ) {
        if self.password_prompt.is_some() || index >= self.connections.len() {
            cancel(self, cx);
            return;
        }
        self.password_remember = false;
        self.password_prompt = Some(PasswordPrompt {
            index,
            then,
            cancel,
        });
        // The field takes the keyboard on the next frame, where a window is
        // at hand.
        self.password_needs_focus = true;
        cx.notify();
    }

    /// Keeps the typed password for the connection, hands it to its tabs and
    /// runs what waited. An empty password is an answer too (a server that
    /// trusts the client), so the prompt does not come back this session.
    pub(super) fn submit_password(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(prompt) = self.password_prompt.take() else {
            return;
        };
        let password = self.password_input.read(cx).unmask_value().to_string();
        self.password_input
            .update(cx, |input, cx| input.set_value("", window, cx));
        let Some(connection) = self.connections.get_mut(prompt.index) else {
            return;
        };
        let Some(input) = connection.input.as_mut() else {
            return;
        };
        input.credentials.password = Some(SecretString::from(password));
        let (name, input, row_limit) =
            (connection.name.clone(), input.clone(), connection.row_limit);
        for panel in self.panels.values() {
            panel.update(cx, |panel, cx| {
                // The same server: results and sessions stay.
                if panel.connection_name() == name {
                    panel.update_connection(name.clone(), input.clone(), row_limit, cx);
                }
            });
        }
        if self.password_remember {
            self.remember_password(prompt.index, input.credentials, prompt.then, window, cx);
        } else {
            (prompt.then)(self, window, cx);
        }
        cx.notify();
    }

    /// Closes the prompt without a password; what waited does not run.
    pub(super) fn cancel_password(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(prompt) = self.password_prompt.take() else {
            return;
        };
        self.password_input
            .update(cx, |input, cx| input.set_value("", window, cx));
        (prompt.cancel)(self, cx);
        cx.notify();
    }

    /// Writes the password to the vault and turns Remember password on for
    /// the connection, then runs `then`. The save bumps the profile's
    /// revision, so what reads metadata starts only after it, under the new
    /// one. A failure keeps the password in memory and says so.
    fn remember_password(
        &mut self,
        index: usize,
        credentials: Credentials,
        then: AfterPassword,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let connection = &self.connections[index];
        let Some(mut profile) = connection.profile.clone() else {
            then(self, window, cx);
            return;
        };
        profile.remember_password = true;
        let id = profile.id;
        let preferences = connection_store::Preferences {
            row_limit: connection.row_limit,
            scope: connection.scope.clone(),
            revalidate_hours: connection.revalidate_hours,
            appearance: connection.appearance,
        };
        let dir = connection_store::data_dir();
        cx.spawn_in(window, async move |weak, cx| {
            let result = backend::run(async move {
                connection_store::save(&dir, profile, credentials, preferences, &SystemSecretStore)
                    .await
            })
            .await;
            let _ = weak.update_in(cx, |this, window, cx| {
                let found = this
                    .connections
                    .iter()
                    .position(|c| c.profile.as_ref().is_some_and(|p| p.id == id));
                match (result, found) {
                    (Ok(loaded), Some(index)) => {
                        let connection = &mut this.connections[index];
                        connection.profile = Some(loaded.profile);
                        if let Some(warning) = loaded.warning {
                            connection.status = Feedback::new(Tone::Warning, warning);
                        }
                    }
                    (Ok(_), None) => {}
                    (Err(error), _) => {
                        this.app_notice = Some(Feedback::new(
                            Tone::Error,
                            tr!(
                                "The password is kept until the application closes, but it could not be remembered: {0}",
                                error
                            ),
                        ));
                    }
                }
                then(this, window, cx);
                cx.notify();
            });
        })
        .detach();
    }

    /// The modal prompt over the window.
    pub(super) fn password_dialog(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(connection) = self
            .password_prompt
            .as_ref()
            .and_then(|prompt| self.connections.get(prompt.index))
        else {
            return div().into_any_element();
        };
        let title = tr!("Password for {0}", connection.name);
        self.overlay("password-backdrop", cx)
            .child(
                column()
                    .id("password-dialog")
                    .role(Role::Dialog)
                    .aria_label(title.clone())
                    .w(px(440.))
                    .mt(px(140.))
                    .p_5()
                    .gap_3()
                    .rounded(px(10.))
                    .bg(rgb(PANEL))
                    .border_1()
                    .border_color(rgb(0x424c57))
                    .shadow_lg()
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .child(
                        div()
                            .text_size(px(16.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(title),
                    )
                    .child(label(connection.detail.clone()))
                    .child(label(tr!(
                        "The password of this connection is not remembered. It stays in memory until the application closes."
                    )))
                    .child(
                        Input::new(&self.password_input)
                            .w_full()
                            .aria_label(tr!("Password")),
                    )
                    .child(
                        button(
                            "prompt-remember-password",
                            if self.password_remember {
                                I::SquareCheck
                            } else {
                                I::Square
                            },
                            tr!("Remember password in the system vault"),
                        )
                        .role(Role::CheckBox)
                        .aria_toggled(if self.password_remember {
                            Toggled::True
                        } else {
                            Toggled::False
                        })
                        .when(self.password_remember, |s| s.text_color(rgb(MINT)))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.password_remember = !this.password_remember;
                            cx.notify();
                        })),
                    )
                    .child(
                        row()
                            .justify_end()
                            .gap_2()
                            .child(button("password-cancel", I::X, tr!("Cancel")).on_click(
                                cx.listener(|this, _, window, cx| this.cancel_password(window, cx)),
                            ))
                            .child(
                                primary_button("password-connect", I::Play, tr!("Connect"))
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.submit_password(window, cx)
                                    })),
                            ),
                    )
                    // Tab stays inside the prompt while it is open.
                    .focus_trap("password-focus-trap", &self.password_focus),
            )
            .into_any_element()
    }
}
