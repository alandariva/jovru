//! Run with `cargo run --example grouped_tabs` to inspect overflow and isolation.

use gpui_kit::component::{ActiveTheme, button::Button};
use gpui_kit::*;
use jykre::grouped_tabs::{GroupedTabs, GroupedTabsEvent, TabGroupId};

struct CounterPanel {
    title: SharedString,
    count: usize,
}

impl Render for CounterPanel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .p_6()
            .flex()
            .flex_col()
            .gap_3()
            .child(self.title.clone())
            .child(format!("Clicks in this tab: {}", self.count))
            .child(
                Button::new("increment")
                    .outline()
                    .label("Increment")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.count += 1;
                        cx.notify();
                    })),
            )
    }
}

struct Example {
    upper: Entity<GroupedTabs>,
    lower: Entity<GroupedTabs>,
    _subscriptions: Vec<Subscription>,
}

fn add_counter(tabs: &Entity<GroupedTabs>, group: TabGroupId, cx: &mut App) {
    let number = tabs.read(cx).tab_ids().count() + 1;
    let title: SharedString = format!("Panel {number}").into();
    let content = cx.new(|_| CounterPanel {
        title: title.clone(),
        count: 0,
    });
    tabs.update(cx, |tabs, cx| tabs.add_tab(group, title, content, cx))
        .unwrap();
}

impl Example {
    fn new(cx: &mut Context<Self>) -> Self {
        let upper = cx.new(GroupedTabs::new);
        let lower = cx.new(GroupedTabs::new);
        for (name, color) in [
            ("Group A", 0x35876d),
            ("Group B", 0x477de6),
            ("Group C", 0xc95a70),
        ] {
            let group = upper.update(cx, |tabs, cx| tabs.add_group(name, rgb(color).into(), cx));
            for _ in 0..10 {
                add_counter(&upper, group, cx);
            }
        }
        let first = upper.read(cx).tab_ids().next().unwrap();
        upper
            .update(cx, |tabs, cx| tabs.select_tab(first, cx))
            .unwrap();
        let group = lower.update(cx, |tabs, cx| {
            tabs.add_group("Another instance", rgb(0x9d77c7).into(), cx)
        });
        add_counter(&lower, group, cx);
        add_counter(&lower, group, cx);
        let subscriptions = [&upper, &lower]
            .into_iter()
            .map(|tabs| {
                cx.subscribe(tabs, |_, tabs, event, cx| {
                    if let GroupedTabsEvent::NewTabRequested(group) = event {
                        add_counter(&tabs, *group, cx);
                    }
                })
            })
            .collect();
        Self {
            upper,
            lower,
            _subscriptions: subscriptions,
        }
    }
}

impl Render for Example {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .child(
                div()
                    .p_2()
                    .text_sm()
                    .child("30 tabs · generic content with its own state"),
            )
            .child(div().flex_1().min_h_0().child(self.upper.clone()))
            .child(
                div()
                    .p_2()
                    .border_t_1()
                    .border_color(cx.theme().border)
                    .text_sm()
                    .child("Independent instance · drags between the bars are ignored"),
            )
            .child(div().flex_1().min_h_0().child(self.lower.clone()))
    }
}

fn main() {
    application().with_assets(assets::Assets).run(|cx| {
        init(cx);
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
                None,
                size(px(1000.), px(680.)),
                cx,
            ))),
            titlebar: Some(TitlebarOptions {
                title: Some("GroupedTabs · Generic example".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        open_window(options, cx, |_, cx| cx.new(Example::new)).unwrap();
    });
}
