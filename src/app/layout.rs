use super::ui::colors;
use gpui_kit::base::ResizeHandleRenderer;
use gpui_kit::component::resizable::ResizableState;
use gpui_kit::{prelude::FluentBuilder as _, *};
use std::{ops::Range, rc::Rc};

pub const EXPLORER_WIDTH: f32 = 280.;
pub const ASSISTANT_WIDTH: f32 = 340.;
pub const EDITOR_MIN_WIDTH: f32 = 480.;

/// The standard component owns pointer dragging and its hit area. Its visible
/// divider also participates in keyboard navigation and exposes its value.
pub fn splitter(
    state: &Entity<ResizableState>,
    panel: usize,
    name: &'static str,
    range: Range<f32>,
) -> ResizeHandleRenderer {
    let state = state.clone();
    Rc::new(move |handle, _, cx| {
        let axis = handle.axis();
        let horizontal = axis == Axis::Horizontal;
        let current = state
            .read(cx)
            .sizes()
            .get(panel)
            .copied()
            .unwrap_or(px(range.start));
        let value = f32::from(current);
        let state = state.clone();
        let range = range.clone();
        let line = div()
            .id("keyboard-splitter")
            .role(Role::Splitter)
            .aria_label(name)
            .aria_orientation(if horizontal {
                accesskit::Orientation::Vertical
            } else {
                accesskit::Orientation::Horizontal
            })
            .aria_numeric_value(value as f64)
            .aria_min_numeric_value(range.start as f64)
            .aria_max_numeric_value(range.end as f64)
            .focusable()
            .tab_index(0)
            .flex_none()
            .bg(rgb(if handle.is_active() {
                colors().mint
            } else {
                colors().border
            }))
            .focus_visible(|s| s.bg(rgb(colors().mint)))
            .map(|s| {
                if horizontal {
                    s.w(px(1.)).h_full()
                } else {
                    s.h(px(1.)).w_full()
                }
            })
            .on_key_down(move |event, window, cx| {
                let key = event.keystroke.key.as_str();
                let step = if event.keystroke.modifiers.shift {
                    40.
                } else {
                    16.
                };
                let reverse = horizontal && panel == 1;
                let delta = match key {
                    "left" if horizontal => Some(-step),
                    "right" if horizontal => Some(step),
                    "up" if !horizontal => Some(-step),
                    "down" if !horizontal => Some(step),
                    _ => None,
                };
                let target = match key {
                    "home" => Some(range.start),
                    "end" => Some(range.end),
                    _ => delta.map(|delta| value + if reverse { -delta } else { delta }),
                };
                if let Some(target) = target {
                    state.update(cx, |state, cx| {
                        state.resize_panel(
                            panel,
                            px(target.clamp(range.start, range.end)),
                            window,
                            cx,
                        )
                    });
                    window.prevent_default();
                    cx.stop_propagation();
                }
            });
        Some(line.into_any_element())
    })
}
