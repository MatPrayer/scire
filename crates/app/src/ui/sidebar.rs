//! Left navigation rail: library switcher + sections + playlists.

use gpui::{App, IntoElement, SharedString, Window, div, prelude::*, px};
use gpui_component::checkbox::Checkbox;
use gpui_component::{ActiveTheme as _, Icon, IconName, Sizable as _, h_flex, v_flex};

/// Top-level nav sections shown in the sidebar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NavSection {
    Albums,
    Artists,
    Favorites,
    Recent,
    Radio,
    Settings,
}

/// What the sidebar reports back to the root view.
#[derive(Debug, Clone)]
pub enum SidebarAction {
    Select(NavSection),
    OpenPlaylist(String),
    NewPlaylist,
    /// Toggle one music library in the selection.
    ToggleLibrary(String),
    /// Reset the selection to all libraries.
    AllLibraries,
    /// Collapse/expand the library switcher.
    ToggleLibrarySection,
}

pub struct SidebarModel {
    pub active: Option<NavSection>,
    pub active_playlist: Option<String>,
    pub playlists: Vec<(String, String)>, // (id, name)
    /// Available libraries (id, name); switcher shown only when > 1.
    pub libraries: Vec<(String, String)>,
    /// Selected library ids; empty = all libraries.
    pub selected_libraries: Vec<String>,
    /// Library switcher folded away (header stays clickable).
    pub libraries_collapsed: bool,
}

fn section_label(text: &'static str, cx: &App) -> impl IntoElement {
    div()
        .px_3()
        .pt_3()
        .pb_1()
        .text_xs()
        .text_color(cx.theme().muted_foreground)
        .child(text)
}

pub fn render_sidebar(
    model: SidebarModel,
    on_action: impl Fn(SidebarAction, &mut Window, &mut App) + Clone + 'static,
    cx: &App,
) -> impl IntoElement {
    let nav_item = |label: &'static str, icon: IconName, section: NavSection| {
        let on_action = on_action.clone();
        let is_active = model.active == Some(section);
        h_flex()
            .id(SharedString::from(label))
            .px_3()
            .py_1p5()
            .gap_2()
            .items_center()
            .rounded_lg()
            .cursor_pointer()
            .text_sm()
            .when(is_active, |s| {
                s.bg(cx.theme().muted).text_color(cx.theme().foreground)
            })
            .when(!is_active, |s| s.text_color(cx.theme().muted_foreground))
            .hover(|s| s.bg(cx.theme().muted))
            .on_click(move |_, window, cx| on_action(SidebarAction::Select(section), window, cx))
            .child(Icon::new(icon).small())
            .child(label)
    };

    // Library switcher (only when the user can access more than one).
    // Checkboxes so several libraries can be browsed at once; checking
    // none (or all) means "all libraries". The section header folds it away.
    let mut library_switcher = v_flex().gap_0p5();
    let show_libraries = model.libraries.len() > 1;
    if show_libraries {
        // Header: label + chevron, click to collapse/expand.
        {
            let on_action = on_action.clone();
            library_switcher = library_switcher.child(
                h_flex()
                    .id("lib-header")
                    .px_3()
                    .pt_3()
                    .pb_1()
                    .gap_1()
                    .items_center()
                    .cursor_pointer()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .on_click(move |_, window, cx| {
                        on_action(SidebarAction::ToggleLibrarySection, window, cx)
                    })
                    .child("Libraries")
                    .child(
                        Icon::new(if model.libraries_collapsed {
                            IconName::ChevronRight
                        } else {
                            IconName::ChevronDown
                        })
                        .xsmall(),
                    ),
            );
        }
        if !model.libraries_collapsed {
            // Quiet rows: unlabeled checkbox + muted text (Checkbox's own
            // label renders in full foreground — too loud for the sidebar).
            // The action is wired to BOTH the checkbox and the row: the
            // checkbox prevents default on mouse-down, which suppresses the
            // row's click handler, so each click fires exactly once.
            let lib_row =
                |key: SharedString, label: String, checked: bool, action: SidebarAction| {
                    let on_row = on_action.clone();
                    let on_check = on_action.clone();
                    let row_action = action.clone();
                    h_flex()
                        .id(key.clone())
                        .px_3()
                        .py_0p5()
                        .gap_2()
                        .items_center()
                        .rounded_lg()
                        .cursor_pointer()
                        .text_sm()
                        .text_color(cx.theme().muted_foreground)
                        .hover(|s| s.bg(cx.theme().muted))
                        .on_click(move |_, window, cx| on_row(row_action.clone(), window, cx))
                        .child(
                            Checkbox::new(key).checked(checked).xsmall().on_click(
                                move |_, window, cx| on_check(action.clone(), window, cx),
                            ),
                        )
                        .child(div().truncate().child(label))
                };
            library_switcher = library_switcher.child(lib_row(
                "lib-all".into(),
                "All libraries".into(),
                model.selected_libraries.is_empty(),
                SidebarAction::AllLibraries,
            ));
            for (id, name) in model.libraries.iter() {
                let checked = model.selected_libraries.contains(id);
                library_switcher = library_switcher.child(lib_row(
                    SharedString::from(format!("lib-{id}")),
                    name.clone(),
                    checked,
                    SidebarAction::ToggleLibrary(id.clone()),
                ));
            }
        }
    }

    let mut playlist_items: Vec<gpui::AnyElement> = Vec::new();
    for (id, name) in model.playlists.iter() {
        let on_action = on_action.clone();
        let id = id.clone();
        let is_active = model.active_playlist.as_deref() == Some(id.as_str());
        playlist_items.push(
            div()
                // Stable per-playlist element id (index would shift on reorder).
                .id(SharedString::from(format!("sidebar-pl-{id}")))
                .px_3()
                .py_1()
                .rounded_lg()
                .cursor_pointer()
                .text_sm()
                .truncate()
                .when(is_active, |s| s.bg(cx.theme().muted))
                .when(!is_active, |s| s.text_color(cx.theme().muted_foreground))
                .hover(|s| s.bg(cx.theme().muted))
                .on_click(move |_, window, cx| {
                    on_action(SidebarAction::OpenPlaylist(id.clone()), window, cx)
                })
                .child(name.clone())
                .into_any_element(),
        );
    }

    let on_new = on_action.clone();
    v_flex()
        .w(px(210.))
        .h_full()
        .px_2()
        .py_2()
        .gap_0p5()
        .border_r_1()
        .border_color(cx.theme().border)
        .bg(cx.theme().sidebar)
        .when(show_libraries, |this| this.child(library_switcher))
        .child(nav_item(
            "Recent",
            IconName::GalleryVerticalEnd,
            NavSection::Recent,
        ))
        .child(nav_item(
            "Albums",
            IconName::LayoutDashboard,
            NavSection::Albums,
        ))
        .child(nav_item(
            "Artists",
            IconName::CircleUser,
            NavSection::Artists,
        ))
        .child(nav_item(
            "Favorites",
            IconName::Heart,
            NavSection::Favorites,
        ))
        .child(nav_item("Radio", IconName::Globe, NavSection::Radio))
        .child(section_label("Playlists", cx))
        .child(
            v_flex()
                .id("sidebar-playlists")
                .flex_1()
                .min_h_0()
                .overflow_y_scroll()
                .gap_0p5()
                .children(playlist_items),
        )
        .child(
            h_flex()
                .id("new-playlist")
                .px_3()
                .py_1p5()
                .gap_2()
                .items_center()
                .rounded_lg()
                .cursor_pointer()
                .text_sm()
                .text_color(cx.theme().muted_foreground)
                .hover(|s| s.bg(cx.theme().muted))
                .on_click(move |_, window, cx| on_new(SidebarAction::NewPlaylist, window, cx))
                .child(Icon::new(IconName::Plus).small())
                .child("New playlist"),
        )
        .child(nav_item(
            "Settings",
            IconName::Settings,
            NavSection::Settings,
        ))
}
