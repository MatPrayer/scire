//! Left navigation rail: library switcher + sections + playlists.

use gpui::{
    Animation, AnimationExt as _, App, ElementId, IntoElement, SharedString, Window, div,
    ease_out_quint, hsla, prelude::*, px, relative,
};

use crate::ui::root::RefreshStage;
use gpui_component::button::{Button, ButtonVariants as _};
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
    LocalMusic,
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
    /// Collapse/expand the playlist list.
    TogglePlaylistSection,
    /// Rescan local dirs and resync the server catalog now.
    RefreshLibrary,
}

pub struct SidebarModel {
    pub active: Option<NavSection>,
    pub active_playlist: Option<String>,
    pub playlists: Vec<(String, String, bool)>, // (id, name, shared-by-other-user)
    /// Available libraries (id, name); switcher shown only when > 1.
    pub libraries: Vec<(String, String)>,
    /// Selected library ids; empty = all libraries.
    pub selected_libraries: Vec<String>,
    /// Library switcher folded away (header stays clickable).
    pub libraries_collapsed: bool,
    /// Playlist list folded away (header stays clickable).
    pub playlists_collapsed: bool,
    /// A library refresh is running; the row reports it and refuses re-entry.
    pub refreshing: bool,
    /// Which step that refresh is on, for the label and the progress bar.
    pub refresh_stage: RefreshStage,
    /// Section highlighted by vi-mode keyboard cursor.
    pub vi_selected_section: Option<NavSection>,
}

/// Thin progress track under the refresh row.
///
/// Only the catalog import knows how much is left; the server scan and the
/// disk walk report a rising count against no total at all. Those get a dim
/// track with no fill rather than a bar creeping toward a made-up finish —
/// the count in the label is the real signal, and a fake bar that stalls at
/// 90% is worse than no bar.
fn refresh_bar(stage: RefreshStage, cx: &App) -> impl IntoElement {
    let track = cx.theme().muted;
    div()
        .w_full()
        .h(px(3.))
        .rounded_full()
        .bg(track)
        .when_some(stage.fraction(), |s, fraction| {
            s.child(
                div()
                    .h_full()
                    .w(relative(fraction))
                    .rounded_full()
                    .bg(cx.theme().primary),
            )
        })
}

pub fn render_sidebar(
    model: SidebarModel,
    on_action: impl Fn(SidebarAction, &mut Window, &mut App) + Clone + 'static,
    reduced_motion: bool,
    cx: &App,
) -> impl IntoElement {
    let nav_item = |label: &'static str, icon: IconName, section: NavSection| {
        let on_action = on_action.clone();
        let is_active = model.active == Some(section);
        let is_vi_sel = model.vi_selected_section == Some(section);
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
            .when(is_vi_sel, |s| {
                s.border_l_2().border_color(cx.theme().primary)
            })
            .on_click(move |_, window, cx| on_action(SidebarAction::Select(section), window, cx))
            .child(Icon::new(icon).small())
            .child(label)
            .with_animation(
                ElementId::Name(format!("sidebar-nav-{}", label).into()),
                Animation::new(std::time::Duration::from_millis(if reduced_motion {
                    0
                } else {
                    150
                }))
                .with_easing(ease_out_quint()),
                |this, _t| this,
            )
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

    // Collapsible "Playlists" header: label + chevron (click to fold) on the
    // left, a "+" new-playlist button on the right.
    let playlists_header = {
        let on_toggle = on_action.clone();
        let on_new = on_action.clone();
        h_flex()
            .px_3()
            .pt_3()
            .pb_1()
            .items_center()
            .justify_between()
            .child(
                h_flex()
                    .id("pl-header")
                    .gap_1()
                    .items_center()
                    .cursor_pointer()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .on_click(move |_, window, cx| {
                        on_toggle(SidebarAction::TogglePlaylistSection, window, cx)
                    })
                    .child("Playlists")
                    .child(
                        Icon::new(if model.playlists_collapsed {
                            IconName::ChevronRight
                        } else {
                            IconName::ChevronDown
                        })
                        .xsmall(),
                    ),
            )
            .child(
                Button::new("pl-new")
                    .ghost()
                    .xsmall()
                    .icon(Icon::new(IconName::Plus))
                    .on_click(move |_, window, cx| on_new(SidebarAction::NewPlaylist, window, cx)),
            )
    };

    let mut playlist_items: Vec<gpui::AnyElement> = Vec::new();
    if !model.playlists_collapsed {
        for (id, name, shared) in model.playlists.iter() {
            let on_action = on_action.clone();
            let id = id.clone();
            let shared = *shared;
            let is_active = model.active_playlist.as_deref() == Some(id.as_str());
            playlist_items.push(
                h_flex()
                    // Stable per-playlist element id (index would shift on reorder).
                    .id(SharedString::from(format!("sidebar-pl-{id}")))
                    .px_3()
                    .py_1()
                    .gap_1p5()
                    .items_center()
                    .rounded_lg()
                    .cursor_pointer()
                    .text_sm()
                    .when(is_active, |s| s.bg(cx.theme().muted))
                    .when(!is_active, |s| s.text_color(cx.theme().muted_foreground))
                    .hover(|s| s.bg(cx.theme().muted))
                    .on_click(move |_, window, cx| {
                        on_action(SidebarAction::OpenPlaylist(id.clone()), window, cx)
                    })
                    .child(div().flex_1().min_w_0().truncate().child(name.clone()))
                    // Playlists owned by another user get a person marker.
                    .when(shared, |s| {
                        s.child(
                            Icon::new(IconName::User)
                                .xsmall()
                                .text_color(cx.theme().muted_foreground),
                        )
                    })
                    .into_any_element(),
            );
        }
    }

    v_flex()
        .w(px(210.))
        .h_full()
        .px_2()
        .py_2()
        .gap_0p5()
        .border_r_1()
        .border_color(hsla(0., 0., 0.5, 0.15))
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
        .child(nav_item("Local", IconName::Folder, NavSection::LocalMusic))
        .child(div().px_3().child(super::divider()))
        .child(playlists_header)
        .child(
            v_flex()
                .id("sidebar-playlists")
                .flex_1()
                .min_h_0()
                .overflow_y_scroll()
                .gap_0p5()
                .children(playlist_items),
        )
        .child(div().px_3().child(super::divider()))
        // Manual catalog refresh: the local scan and the server sync otherwise
        // only run on their own schedule, so newly added music needed a restart.
        .child({
            let on_refresh = on_action.clone();
            let refreshing = model.refreshing;
            let stage = model.refresh_stage;
            v_flex()
                .id("sidebar-refresh")
                .px_3()
                .py_1p5()
                .gap_1()
                .rounded_lg()
                .text_sm()
                .text_color(cx.theme().muted_foreground)
                .when(!refreshing, |s| {
                    s.cursor_pointer().hover(|s| s.bg(cx.theme().muted))
                })
                .on_click(move |_, window, cx| {
                    if !refreshing {
                        on_refresh(SidebarAction::RefreshLibrary, window, cx);
                    }
                })
                .child(
                    h_flex()
                        .gap_2()
                        .items_center()
                        .when(refreshing, |s| s.opacity(0.6))
                        .child(crate::assets::app_icon(crate::assets::icons::REFRESH).small())
                        .child(if refreshing {
                            SharedString::from(stage.label())
                        } else {
                            SharedString::from("Refresh library")
                        }),
                )
                // A refresh is minutes of work on a big library. Without a bar
                // the row read as hung, which is exactly what it looked like.
                .when(refreshing, |s| s.child(refresh_bar(stage, cx)))
        })
        .child(nav_item(
            "Settings",
            IconName::Settings,
            NavSection::Settings,
        ))
}
