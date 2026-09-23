//! Temporary layout probe: replicates the fullscreen player's stacked
//! panel-beside row and prints the measured bounds of each piece.
use gpui::prelude::*;
use gpui::{
    App, Application, Bounds, Context, Window, WindowBounds, WindowOptions, canvas, div, point, px,
    size,
};

struct Probe;

impl Render for Probe {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        let tag = |name: &'static str| {
            canvas(
                move |bounds, _, _| {
                    println!("{name}: {:?}", bounds);
                },
                |_, _, _, _| {},
            )
            .absolute()
            .size_full()
        };
        div()
            .id("scroll")
            .size_full()
            .overflow_y_scroll()
            .child(
                div()
                    .flex()
                    .flex_col()
                    .w_full()
                    .min_h(px(1396.))
                    .items_center()
                    .justify_center()
                    .gap_8()
                    .px_10()
                    .pt(px(72.))
                    .pb(px(40.))
                    .child(div().size(px(720.)).flex_none().bg(gpui::red()))
                    .child(
                        div()
                            .relative()
                            .flex()
                            .flex_row()
                            .items_center()
                            .justify_center()
                            .gap_8()
                            .child(tag("row"))
                            .child(
                                div()
                                    .relative()
                                    .flex()
                                    .flex_col()
                                    .flex_none()
                                    .w(px(480.))
                                    .justify_center()
                                    .gap(px(20.))
                                    .p(px(24.))
                                    .bg(gpui::blue())
                                    .child(tag("card"))
                                    .child(div().h(px(310.)).w_full().bg(gpui::green())),
                            )
                            .child(
                                div().relative().h_full().child(
                                    div()
                                        .relative()
                                        .flex()
                                        .flex_col()
                                        .w(px(431.))
                                        .flex_none()
                                        .max_h(px(532.))
                                        .p(px(16.))
                                        .bg(gpui::white())
                                        .child(tag("panel"))
                                        .child(div().h(px(2000.)).w_full()),
                                ),
                            ),
                    ),
            )
            .into_any_element()
    }
}

fn main() {
    Application::new().run(|cx: &mut App| {
        let bounds = Bounds {
            origin: point(px(0.), px(0.)),
            size: size(px(1268.), px(1396.)),
        };
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            |_, cx| cx.new(|_| Probe),
        )
        .unwrap();
        cx.activate(true);
    });
}
