//! The explorer's Favorites (0.2.0 M9): connections, tables and views kept
//! above the connections, listed from local state and the metadata cache
//! without reading a server ([`jykre::favorites`]).
//!
//! A table or view opens as **View data** does in the explorer, with its
//! safeguards. A favorite whose connection was deleted, or whose object the
//! cache no longer holds, is marked and can be removed; it is never pointed
//! at another object.
use super::*;
use jykre::favorites::{Favorite, Favorites, SETTINGS_KEY};

/// Rows shown before the list scrolls.
const VISIBLE_ROWS: usize = 6;
const ROW_HEIGHT: f32 = 30.;
/// Changes are written once a burst of them settles.
const SAVE_DELAY: std::time::Duration = std::time::Duration::from_millis(300);

/// Why a favorite cannot be used now.
enum Missing {
    Connection,
    Object,
}

impl AppView {
    /// Takes the favorites read at start. A value that cannot be read is
    /// reported and never written over.
    pub(super) fn load_favorites(
        &mut self,
        value: DriverResult<Option<serde_json::Value>>,
    ) -> Option<String> {
        match value.map(Favorites::from_value) {
            Ok(Ok(favorites)) => {
                self.favorites = favorites;
                self.persist_favorites = true;
                None
            }
            Ok(Err(_)) => Some(
                tr!(
                    "The explorer's favorites could not be read. They are kept as they are and nothing is saved over them."
                )
                .to_owned(),
            ),
            Err(error) => Some(tr!("Could not read the explorer's favorites: {0}", error)),
        }
    }

    /// The identity of the connection at `index`, once its profile is read.
    fn connection_id(&self, index: usize) -> Option<ConnectionId> {
        self.connections
            .get(index)?
            .profile
            .as_ref()
            .map(|profile| profile.id)
    }

    /// Whether the connection at `index` (`path` `None`) or its object at
    /// `path` is a favorite.
    pub(super) fn is_favorite(&self, index: usize, path: Option<&ObjectPath>) -> bool {
        self.connection_id(index)
            .is_some_and(|id| self.favorites.contains(id, path))
    }

    /// **Add to favorites** or **Remove from favorites** in the explorer's
    /// menus.
    pub(super) fn toggle_favorite(
        &mut self,
        index: usize,
        path: Option<ObjectPath>,
        stored_at: Option<ObjectPath>,
        cx: &mut Context<Self>,
    ) {
        let Some(id) = self.connection_id(index) else {
            return;
        };
        if !self.favorites.remove(id, path.as_ref()) {
            self.favorites.add(match path {
                Some(path) => Favorite::object(id, path, stored_at),
                None => Favorite::connection(id),
            });
        }
        self.save_favorites_soon(cx);
        cx.notify();
    }

    fn remove_favorite(&mut self, position: usize, cx: &mut Context<Self>) {
        if position < self.favorites.items.len() {
            self.favorites.items.remove(position);
            self.save_favorites_soon(cx);
            cx.notify();
        }
    }

    /// Writes the favorites once changes settle; the last list wins.
    fn save_favorites_soon(&mut self, cx: &mut Context<Self>) {
        if !self.persist_favorites || super::demo::enabled() {
            return;
        }
        self.favorites_revision += 1;
        let revision = self.favorites_revision;
        cx.spawn(async move |weak, cx| {
            cx.background_executor().timer(SAVE_DELAY).await;
            let Ok(Some(value)) = weak.update(cx, |this, _| {
                (this.favorites_revision == revision).then(|| this.favorites.to_value())
            }) else {
                return;
            };
            let dir = connection_store::data_dir();
            let written = backend::run(async move {
                connection_store::write_setting(&dir, SETTINGS_KEY, &value, "favorites").await
            })
            .await;
            if let Err(error) = written {
                let _ = weak.update(cx, |this, cx| {
                    this.app_notice = Some(Feedback::new(
                        Tone::Error,
                        tr!("Could not save the explorer's favorites: {0}", error),
                    ));
                    cx.notify();
                });
            }
        })
        .detach();
    }

    /// The connection a favorite names, by identity.
    fn favorite_connection(&self, favorite: &Favorite) -> Option<usize> {
        self.connections.iter().position(|connection| {
            connection
                .profile
                .as_ref()
                .is_some_and(|profile| profile.id == favorite.connection)
        })
    }

    /// Why a favorite cannot be used, from local state alone: its
    /// connection is gone, or the connection's cache, once read, does not
    /// hold its object.
    fn favorite_missing(&self, favorite: &Favorite) -> Option<Missing> {
        let Some(index) = self.favorite_connection(favorite) else {
            return Some(Missing::Connection);
        };
        let path = favorite.path.as_ref()?;
        let names = self.connections[index].names.as_ref()?;
        (!names.contains(path)).then_some(Missing::Object)
    }

    /// Opens a favorite: a table or view as **View data**; a connection is
    /// expanded and scrolled to in the explorer.
    fn open_favorite(&mut self, position: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(favorite) = self.favorites.items.get(position).cloned() else {
            return;
        };
        let Some(index) = self.favorite_connection(&favorite) else {
            self.app_notice = Some(Feedback::new(
                Tone::Warning,
                tr!("This favorite's connection no longer exists. Remove it from the favorites."),
            ));
            cx.notify();
            return;
        };
        match &favorite.path {
            Some(path) => {
                let stored_at = favorite.stored_at.clone().unwrap_or_else(|| path.clone());
                self.open_relation(
                    index,
                    path,
                    &stored_at,
                    connections::OpenTable::Data,
                    window,
                    cx,
                );
            }
            None => {
                self.connections[index].expanded = true;
                if let Some(at) = self.explorer_entries.iter().position(
                    |entry| matches!(entry, ExplorerEntry::Connection(at) if *at == index),
                ) {
                    self.explorer_list.scroll_to_reveal_item(at);
                }
                cx.notify();
            }
        }
    }

    /// Above the connections: the favorites, virtualized, at most
    /// [`VISIBLE_ROWS`] high before they scroll.
    pub(super) fn favorites_section(&self, cx: &mut Context<Self>) -> Div {
        let count = self.favorites.items.len();
        column()
            .flex_none()
            .pb_1()
            .child(
                row()
                    .px_4()
                    .h(px(26.))
                    .flex_none()
                    .justify_between()
                    .child(
                        div()
                            .text_size(px(12.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(rgb(colors().faint))
                            .child(tr!("Favorites")),
                    )
                    .child(badge(count.to_string(), colors().faint)),
            )
            .child(
                div()
                    .h(px(ROW_HEIGHT * count.min(VISIBLE_ROWS) as f32))
                    .flex_none()
                    .child(
                        uniform_list(
                            "explorer-favorites",
                            count,
                            cx.processor(move |this, range: Range<usize>, _, cx| {
                                range
                                    .map(|position| this.favorite_row(position, cx))
                                    .collect::<Vec<_>>()
                            }),
                        )
                        .size_full()
                        .track_scroll(&self.favorites_scroll),
                    )
                    .when(count > VISIBLE_ROWS, |s| {
                        s.vertical_scrollbar(&self.favorites_scroll)
                    }),
            )
    }

    /// One favorite: its kind, name and where it is; the keyboard reaches it
    /// and its remove button (Enter or Space acts).
    fn favorite_row(&self, position: usize, cx: &mut Context<Self>) -> AnyElement {
        let Some(favorite) = self.favorites.items.get(position) else {
            return div().into_any_element();
        };
        let connection = self
            .favorite_connection(favorite)
            .and_then(|index| self.connections.get(index));
        let connection_name = connection.map_or_else(
            || tr!("Deleted connection").to_owned(),
            |connection| connection.name.clone(),
        );
        let (symbol, name, place) = match &favorite.path {
            None => (I::Server, connection_name.clone(), String::new()),
            Some(path) => {
                let name = path
                    .0
                    .last()
                    .map(|segment| segment.name.clone())
                    .unwrap_or_default();
                let namespace: Vec<&str> = path.0[..path.0.len().saturating_sub(1)]
                    .iter()
                    .map(|segment| segment.name.as_str())
                    .collect();
                let place = if namespace.is_empty() {
                    connection_name.clone()
                } else {
                    format!("{} · {}", connection_name, namespace.join("."))
                };
                let symbol = match favorite.kind() {
                    Some(ObjectKind::View) => I::Eye,
                    Some(ObjectKind::MaterializedView) => I::TableProperties,
                    _ => I::Table2,
                };
                (symbol, name, place)
            }
        };
        let missing = self.favorite_missing(favorite);
        let reason = match missing {
            Some(Missing::Connection) => Some(tr!(
                "Its connection no longer exists. Remove it, or add the object again from another connection."
            )),
            Some(Missing::Object) => Some(tr!(
                "The connection's cache no longer holds this object: it may have been dropped or renamed, or left the metadata you chose. Refresh the metadata or remove it."
            )),
            None => None,
        };
        let label = if place.is_empty() {
            name.clone()
        } else {
            format!("{name}, {place}")
        };
        row()
            .id(("favorite", position))
            .h(px(ROW_HEIGHT))
            .px_3()
            .gap_2()
            .group("favorite")
            .child(
                row()
                    .id(("open-favorite", position))
                    .role(Role::Button)
                    .aria_label(label)
                    .focusable()
                    .tab_index(0)
                    .border_1()
                    .border_color(transparent_black())
                    .focus_visible(|s| s.border_color(rgb(colors().mint)))
                    .rounded(px(5.))
                    .px_1()
                    .flex_1()
                    .min_w_0()
                    .gap_2()
                    .cursor_pointer()
                    .hover(|s| s.bg(rgb(colors().hover)))
                    .child(icon(
                        symbol,
                        if missing.is_some() {
                            colors().faint
                        } else {
                            colors().mint
                        },
                    ))
                    .child(
                        div()
                            .flex_none()
                            .text_size(px(13.))
                            .text_color(rgb(if missing.is_some() {
                                colors().faint
                            } else {
                                colors().text
                            }))
                            .child(name),
                    )
                    .when(!place.is_empty(), |s| {
                        s.child(
                            div()
                                .min_w_0()
                                .truncate()
                                .text_size(px(12.))
                                .text_color(rgb(colors().faint))
                                .child(place),
                        )
                    })
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.open_favorite(position, window, cx)
                    })),
            )
            .when_some(reason, |s, reason| {
                s.child(
                    div()
                        .id(("favorite-missing", position))
                        .role(Role::Status)
                        .aria_label(tr!("Unavailable"))
                        .flex_none()
                        .text_size(px(11.))
                        .text_color(rgb(colors().amber))
                        .child(tr!("Unavailable"))
                        .tooltip(move |window, cx| Tooltip::new(reason).build(window, cx)),
                )
            })
            .child(
                icon_button(
                    ("remove-favorite", position),
                    I::X,
                    tr!("Remove from favorites"),
                )
                .size(px(24.))
                .flex_none()
                .on_click(cx.listener(move |this, _, _, cx| this.remove_favorite(position, cx))),
            )
            .into_any_element()
    }
}
