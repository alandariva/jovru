//! The **Go to line** prompt of a SQL editor: `Ctrl G` (the `GoToLine`
//! action) opens a small field over the editor's top right corner; `Enter`
//! moves the cursor to the line (and column) typed, `Esc` gives the keyboard
//! back to the editor, and leaving the field closes it. What is typed and
//! where it leads are read by `jykre::go_to_line`; the editor's own
//! search bar closes first, since it sits in the same place.
use super::SqlEditor;
use crate::app::ui::{colors, column, row};
use gpui_kit::component::Sizable;
use gpui_kit::component::input::{Escape, Input, InputEvent, InputState};
use gpui_kit::{prelude::FluentBuilder as _, *};
use jykre::{go_to_line, tr};

/// The prompt of one editor; closed until `Ctrl G`. The host embeds it next
/// to the editor, in a parent that is `relative`.
pub struct GoToLine {
    sql_editor: WeakEntity<SqlEditor>,
    /// The field while the prompt is open.
    input: Option<Entity<InputState>>,
    /// The text last tried was not a line.
    invalid: bool,
    _subscriptions: Vec<Subscription>,
}

impl GoToLine {
    pub fn new(sql_editor: &Entity<SqlEditor>) -> Self {
        Self {
            sql_editor: sql_editor.downgrade(),
            input: None,
            invalid: false,
            _subscriptions: Vec::new(),
        }
    }

    /// Opens the prompt, empty, with the keyboard in it. Called again while
    /// open, it starts over.
    pub fn open(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(sql_editor) = self.sql_editor.upgrade() else {
            return;
        };
        let lines = sql_editor.update(cx, |editor, cx| {
            editor.close_search(cx);
            editor.line_count(cx)
        });
        let input = cx.new(|cx| InputState::new(window, cx).placeholder(go_to_line::hint(lines)));
        self._subscriptions =
            vec![
                cx.subscribe_in(&input, window, |this, _, event: &InputEvent, window, cx| {
                    match event {
                        InputEvent::PressEnter { .. } => this.go(window, cx),
                        InputEvent::Change if this.invalid => {
                            this.invalid = false;
                            cx.notify();
                        }
                        // A click elsewhere: the keyboard stays where it went.
                        InputEvent::Blur => this.close(cx),
                        _ => {}
                    }
                }),
            ];
        input.update(cx, |input, cx| input.focus(window, cx));
        self.input = Some(input);
        self.invalid = false;
        cx.notify();
    }

    /// Moves the editor's cursor to what was typed, or says it is not a line.
    fn go(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(input) = &self.input else {
            return;
        };
        let Some(target) = go_to_line::parse(&input.read(cx).value()) else {
            self.invalid = true;
            cx.notify();
            return;
        };
        self.close(cx);
        if let Some(sql_editor) = self.sql_editor.upgrade() {
            sql_editor.update(cx, |editor, cx| editor.go_to(target, window, cx));
        }
    }

    /// `Esc`: closes the prompt and gives the keyboard back to the editor.
    fn cancel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.close(cx);
        if let Some(sql_editor) = self.sql_editor.upgrade() {
            sql_editor.update(cx, |editor, cx| editor.focus(window, cx));
        }
    }

    fn close(&mut self, cx: &mut Context<Self>) {
        if self.input.take().is_some() {
            self._subscriptions.clear();
            self.invalid = false;
            cx.notify();
        }
    }
}

impl Render for GoToLine {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(input) = self.input.clone() else {
            return div().into_any_element();
        };
        column()
            .id("go-to-line")
            .occlude()
            .absolute()
            .top(px(6.))
            .right(px(18.))
            .w(px(280.))
            .p(px(6.))
            .gap_1()
            .rounded(px(6.))
            .border_1()
            .border_color(rgb(colors().border))
            .bg(rgb(colors().panel))
            .shadow_lg()
            .on_action(cx.listener(|this, _: &Escape, window, cx| this.cancel(window, cx)))
            .child(
                row()
                    .gap_2()
                    .child(
                        div()
                            .flex_none()
                            .text_size(px(12.))
                            .text_color(rgb(colors().faint))
                            .child(tr!("Go to line")),
                    )
                    .child(
                        div().flex_1().min_w_0().child(
                            Input::new(&input)
                                .small()
                                .aria_label(tr!("Line to go to, or line:column")),
                        ),
                    ),
            )
            .when(self.invalid, |s| {
                s.child(
                    div()
                        .id("go-to-line-invalid")
                        .role(Role::Alert)
                        .text_size(px(12.))
                        .text_color(rgb(colors().red))
                        .child(tr!("Type a line number, or a line and a column as 12:5.")),
                )
            })
            .into_any_element()
    }
}
