//! Right-side play-queue panel: jump / remove / reorder / clear.

use gpui::{
    Context, Entity, EventEmitter, IntoElement, Render, SharedString, UniformListScrollHandle,
    Window, div, prelude::*, px, uniform_list,
};
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::{
    ActiveTheme as _, Disableable as _, Icon, IconName, Sizable as _, StyledExt as _, h_flex,
    v_flex,
};

use crate::config::PlayerBarStyle;
use crate::state::player::PlayerState;
use crate::state::session::Session;
use crate::ui::format_duration;
use crate::ui::player_bar::float_fill;

/// Fixed row height — `uniform_list` requires every row to be the same size.
/// Two lines of text (title over artist) plus the row's vertical padding.
const ROW_H_BASE: f32 = 44.;

/// [`ROW_H_BASE`] at the current UI scale. A function because the panel's own
/// height is a whole number of these — a row and a panel sized from different
/// numbers is a panel that ends mid-row, which is the fault the row-snapping
/// exists to avoid.
fn row_h() -> f32 {
    crate::ui::scaled(ROW_H_BASE)
}

/// One row's pre-formatted contents.
///
/// Built when the queue changes, not per frame. This panel observes
/// `PlayerState`, which notifies on every event — `Event::Position` included,
/// several times a second — and the queue holds a whole album or playlist, so
/// deriving these strings per render copied every title and artist in it for
/// as long as the panel stayed open.
struct Row {
    pos: usize,
    title: SharedString,
    artist: SharedString,
    duration: SharedString,
}

/// What the panel asks the root to do; it does not own its own visibility.
pub enum QueuePanelEvent {
    /// Close the panel. The panel carries its own close button because the
    /// player bar's queue toggle is not always on screen — it is hidden with
    /// the bar itself when nothing is playing
    /// (`Settings::hide_idle_player_bar`), which would otherwise leave a panel
    /// open with no way back out short of the keyboard.
    Close,
}

pub struct QueuePanel {
    player: Entity<PlayerState>,
    /// Read for `player_bar_style` alone: the panel is opened from the bar and
    /// sits against it, so a floating bar takes the panel with it.
    session: Entity<Session>,
    rows: Vec<Row>,
    current: Option<usize>,
    /// Queue revision the rows were built from, so a position tick doesn't
    /// rebuild them.
    revision: u64,
    scroll: UniformListScrollHandle,
}

impl QueuePanel {
    pub fn new(
        player: Entity<PlayerState>,
        session: Entity<Session>,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut this = Self {
            player,
            session,
            rows: Vec::new(),
            current: None,
            // The queue starts at revision 0, so seed the mismatch that makes
            // the first refresh build the rows.
            revision: u64::MAX,
            scroll: UniformListScrollHandle::new(),
        };
        this.refresh(cx);
        cx.observe(&this.player.clone(), |this, _, cx| {
            // Only a real change to the queue is worth a rebuild and a repaint.
            if this.refresh(cx) {
                cx.notify();
            }
        })
        .detach();
        this
    }

    /// Resync the rows from the player. Returns whether anything changed.
    fn refresh(&mut self, cx: &mut Context<Self>) -> bool {
        let p = self.player.read(cx);
        let revision = p.queue.revision();
        let current = p.queue.current_pos();
        // The highlight moves with `current` without the contents changing, so
        // both are part of the signature.
        if revision == self.revision && current == self.current {
            return false;
        }
        if revision != self.revision {
            self.rows = p
                .queue
                .iter_ordered()
                .map(|(pos, s)| Row {
                    pos,
                    title: s.title.clone().into(),
                    artist: s.artist.clone().unwrap_or_default().into(),
                    duration: s
                        .duration
                        .map(|d| format_duration(std::time::Duration::from_secs(u64::from(d))))
                        .unwrap_or_default()
                        .into(),
                })
                .collect();
        }
        self.revision = revision;
        self.current = current;
        true
    }

    fn render_row(&self, entity: &Entity<Self>, ix: usize, cx: &gpui::App) -> gpui::AnyElement {
        let Some(row) = self.rows.get(ix) else {
            return div().h(px(row_h())).into_any_element();
        };
        let pos = row.pos;
        let is_current = self.current == Some(pos);
        let last = pos + 1 == self.rows.len();
        let (jump, up, down, rm) = (
            entity.clone(),
            entity.clone(),
            entity.clone(),
            entity.clone(),
        );

        h_flex()
            .id(("q", pos))
            .group("qrow")
            // `uniform_list` sizes its items to their content, so without this
            // the duration and the controls land at a different x per line.
            .w_full()
            .h(px(row_h()))
            .px_2()
            .border_b_1()
            .border_color(gpui::hsla(0., 0., 0.5, 0.15))
            .gap_2()
            .items_center()
            .rounded_md()
            .cursor_pointer()
            // Accent bar + tinted background marks the playing track;
            // transparent border on the rest keeps rows aligned.
            .border_l_2()
            .border_color(gpui::transparent_black())
            .hover(|s| s.bg(cx.theme().muted))
            .when(is_current, |s| {
                s.bg(cx.theme().primary.opacity(0.12))
                    .border_color(cx.theme().primary)
                    .text_color(cx.theme().primary)
            })
            .on_click(move |_, _, cx: &mut gpui::App| {
                jump.update(cx, |this, cx| {
                    this.player.update(cx, |p, cx| p.jump_to(pos, cx));
                });
            })
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .child(
                        div()
                            .text_sm()
                            .truncate()
                            .when(is_current, |s| s.font_medium())
                            .child(row.title.clone()),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .truncate()
                            .child(row.artist.clone()),
                    ),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(row.duration.clone()),
            )
            // Reorder / remove controls (highlighted on row hover).
            .child(
                h_flex()
                    .gap_0p5()
                    .opacity(0.3)
                    .group_hover("qrow", |s| s.opacity(1.))
                    .when(pos > 0, |this| {
                        this.child(
                            Button::new(("up", pos))
                                .ghost()
                                .xsmall()
                                .icon(Icon::new(IconName::ArrowUp))
                                .on_click(move |_, _, cx: &mut gpui::App| {
                                    up.update(cx, |this, cx| {
                                        this.player.update(cx, |p, cx| {
                                            p.move_queue_item(pos, pos - 1, cx)
                                        });
                                    });
                                    cx.stop_propagation();
                                }),
                        )
                    })
                    .when(!last, |this| {
                        this.child(
                            Button::new(("down", pos))
                                .ghost()
                                .xsmall()
                                .icon(Icon::new(IconName::ArrowDown))
                                .on_click(move |_, _, cx: &mut gpui::App| {
                                    down.update(cx, |this, cx| {
                                        this.player.update(cx, |p, cx| {
                                            p.move_queue_item(pos, pos + 1, cx)
                                        });
                                    });
                                    cx.stop_propagation();
                                }),
                        )
                    })
                    .child(
                        Button::new(("rm", pos))
                            .ghost()
                            .xsmall()
                            .icon(Icon::new(IconName::Close))
                            .on_click(move |_, _, cx: &mut gpui::App| {
                                rm.update(cx, |this, cx| {
                                    this.player.update(cx, |p, cx| p.remove_from_queue(pos, cx));
                                });
                                cx.stop_propagation();
                            }),
                    ),
            )
            .into_any_element()
    }
}

impl EventEmitter<QueuePanelEvent> for QueuePanel {}

impl Render for QueuePanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Straight off the queue: an integer read, and the header is stating
        // the queue's length rather than the row cache's.
        let len = self.player.read(cx).queue.len();
        let entity = cx.entity();
        let list = uniform_list("queue-list", len, move |range, _window, cx| {
            let view = entity.read(cx);
            range
                .map(|ix| view.render_row(&entity, ix, cx))
                .collect::<Vec<_>>()
        })
        .flex_1()
        .min_h_0()
        .px_2()
        .track_scroll(self.scroll.clone());

        // A floating player bar takes the panel with it: the panel is opened
        // from that bar and sits directly above it, so leaving it docked to the
        // window's edge puts a full-height strip beside a card hovering over
        // the page — two designs at once. Same card treatment as the bar and
        // the fullscreen overlay's panels, including the darkened fill and the
        // opt-in show-through, so the two read as one piece.
        let settings = &self.session.read(cx).settings;
        let floating = settings.player_bar_style == PlayerBarStyle::Floating;
        let translucent = settings.player_bar_translucent;

        v_flex()
            .w(px(300.))
            .h_full()
            .map(|this| {
                if floating {
                    this.rounded_2xl()
                        .shadow_xl()
                        .border_1()
                        .border_color(cx.theme().border.opacity(0.6))
                        .overflow_hidden()
                        .occlude()
                        .bg(float_fill(cx.theme().background, translucent))
                } else {
                    this.border_l_1()
                        .border_color(cx.theme().border)
                        .bg(cx.theme().sidebar)
                }
            })
            .child(
                h_flex()
                    .px_3()
                    .py_2()
                    .justify_between()
                    .items_center()
                    .child(div().text_sm().child(format!("Queue ({len})")))
                    .child(
                        h_flex()
                            .gap_1()
                            .items_center()
                            .child(
                                Button::new("clear-queue")
                                    .ghost()
                                    .xsmall()
                                    .label("Clear")
                                    .disabled(len == 0)
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.player.update(cx, |p, cx| p.clear_queue(cx));
                                    })),
                            )
                            .child(
                                Button::new("close-queue")
                                    .ghost()
                                    .xsmall()
                                    .icon(Icon::new(IconName::Close))
                                    .tooltip("Close queue")
                                    .on_click(cx.listener(|_, _, _, cx| {
                                        cx.emit(QueuePanelEvent::Close);
                                    })),
                            ),
                    ),
            )
            .child(list)
    }
}
