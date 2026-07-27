//! Right-side play-queue panel: jump / remove / reorder / clear.

use gpui::{Context, Entity, IntoElement, Render, Window, div, prelude::*, px};
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::{
    ActiveTheme as _, Disableable as _, Icon, IconName, Sizable as _, StyledExt as _, h_flex,
    v_flex,
};

use crate::state::player::PlayerState;
use crate::ui::format_duration;

pub struct QueuePanel {
    player: Entity<PlayerState>,
}

impl QueuePanel {
    pub fn new(player: Entity<PlayerState>, cx: &mut Context<Self>) -> Self {
        cx.observe(&player, |_, _, cx| cx.notify()).detach();
        Self { player }
    }
}

impl Render for QueuePanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let (rows, len, current) = {
            let p = self.player.read(cx);
            let current = p.queue.current_pos();
            let rows: Vec<(usize, String, String, Option<u32>)> = p
                .queue
                .iter_ordered()
                .map(|(pos, s)| {
                    (
                        pos,
                        s.title.clone(),
                        s.artist.clone().unwrap_or_default(),
                        s.duration,
                    )
                })
                .collect();
            (rows, p.queue.len(), current)
        };

        let items: Vec<_> = rows
            .into_iter()
            .map(|(pos, title, artist, dur)| {
                let is_current = current == Some(pos);
                let last = pos + 1 == len;
                h_flex()
                    .id(gpui::SharedString::from(format!("q-{pos}")))
                    .group("qrow")
                    .px_2()
                    .py_1()
                    .border_b_1()
                    .border_color(gpui::hsla(0., 0., 0.5, 0.15))
                    .gap_2()
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
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.player.update(cx, |p, cx| p.jump_to(pos, cx));
                    }))
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .child(
                                div()
                                    .text_sm()
                                    .truncate()
                                    .when(is_current, |s| s.font_medium())
                                    .child(title),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(cx.theme().muted_foreground)
                                    .truncate()
                                    .child(artist),
                            ),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(
                                dur.map(|s| {
                                    format_duration(std::time::Duration::from_secs(s as u64))
                                })
                                .unwrap_or_default(),
                            ),
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
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            this.player.update(cx, |p, cx| {
                                                p.move_queue_item(pos, pos - 1, cx);
                                            });
                                            cx.stop_propagation();
                                        })),
                                )
                            })
                            .when(!last, |this| {
                                this.child(
                                    Button::new(("down", pos))
                                        .ghost()
                                        .xsmall()
                                        .icon(Icon::new(IconName::ArrowDown))
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            this.player.update(cx, |p, cx| {
                                                p.move_queue_item(pos, pos + 1, cx);
                                            });
                                            cx.stop_propagation();
                                        })),
                                )
                            })
                            .child(
                                Button::new(("rm", pos))
                                    .ghost()
                                    .xsmall()
                                    .icon(Icon::new(IconName::Close))
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.player
                                            .update(cx, |p, cx| p.remove_from_queue(pos, cx));
                                        cx.stop_propagation();
                                    })),
                            ),
                    )
                    .into_any_element()
            })
            .collect();

        v_flex()
            .w(px(300.))
            .h_full()
            .border_l_1()
            .border_color(cx.theme().border)
            .bg(cx.theme().sidebar)
            .child(
                h_flex()
                    .px_3()
                    .py_2()
                    .justify_between()
                    .items_center()
                    .child(div().text_sm().child(format!("Queue ({len})")))
                    .child(
                        Button::new("clear-queue")
                            .ghost()
                            .xsmall()
                            .label("Clear")
                            .disabled(len == 0)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.player.update(cx, |p, cx| p.clear_queue(cx));
                            })),
                    ),
            )
            .child(
                v_flex()
                    .id("queue-scroll")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .px_2()
                    .pb_2()
                    .gap_0p5()
                    .children(items),
            )
    }
}
