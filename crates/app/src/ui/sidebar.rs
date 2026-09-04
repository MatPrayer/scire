//! Left navigation rail: library switcher + sections + playlists.

use std::rc::Rc;

use gpui::{App, IntoElement, SharedString, Window, div, hsla, prelude::*, px, relative};

use crate::assets::{app_icon, icons};
use crate::ui::root::RefreshStage;
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::checkbox::Checkbox;
use gpui_component::popover::Popover;
use gpui_component::tooltip::Tooltip;
use gpui_component::{ActiveTheme as _, Icon, IconName, Sizable as _, h_flex, v_flex};

/// What a row does *after* its action has been dispatched.
///
/// The rail's dropdowns have to close themselves once something in them has
/// been picked, while the same rows in the expanded sidebar are already where
/// the user left them and must not move. Both build the row through the same
/// helper and differ only in this hook.
type AfterPick = Rc<dyn Fn(&mut Window, &mut App)>;

/// A row that does nothing once its action has run — the expanded sidebar.
fn stay_put() -> AfterPick {
    Rc::new(|_, _| {})
}

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
    /// Fold the whole rail down to icons (or unfold it).
    ToggleSidebar,
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
    /// Rail folded down to icons: no labels, no library switcher, no
    /// playlists. The nav sections and the two footer rows survive as icons
    /// with tooltips, since those are what the rail is for.
    pub collapsed: bool,
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

/// Row icon sizing. Collapsed, the icon *is* the row — it carries the weight
/// the label used to, and the 14px `small()` mark the labelled rows use reads
/// as a speck in a 52px rail.
fn row_icon(icon: Icon, collapsed: bool) -> Icon {
    if collapsed {
        icon.with_size(px(18.))
    } else {
        icon.small()
    }
}

/// One library row: an unlabeled checkbox plus muted text.
///
/// Checkbox's own label renders in full foreground, which is too loud for the
/// sidebar. The action is wired to BOTH the checkbox and the row: the checkbox
/// prevents default on mouse-down, which suppresses the row's click handler,
/// so each click fires exactly once.
///
/// Shared by the expanded switcher and the collapsed rail's dropdown so the
/// two cannot drift apart.
fn library_row(
    key: SharedString,
    label: String,
    checked: bool,
    action: SidebarAction,
    on_action: impl Fn(SidebarAction, &mut Window, &mut App) + Clone + 'static,
    cx: &App,
) -> impl IntoElement {
    let on_row = on_action.clone();
    let on_check = on_action;
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
            Checkbox::new(key)
                .checked(checked)
                .xsmall()
                .on_click(move |_, window, cx| on_check(action.clone(), window, cx)),
        )
        .child(div().truncate().child(label))
}

/// One playlist row. `key_prefix` keeps the expanded list's ids apart from the
/// rail dropdown's copies of the same playlists.
fn playlist_row(
    key_prefix: &str,
    playlist: &(String, String, bool),
    is_active: bool,
    on_action: impl Fn(SidebarAction, &mut Window, &mut App) + Clone + 'static,
    after: AfterPick,
    cx: &App,
) -> impl IntoElement {
    let (id, name, shared) = playlist.clone();
    h_flex()
        // Stable per-playlist element id (index would shift on reorder).
        .id(SharedString::from(format!("{key_prefix}-{id}")))
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
            on_action(SidebarAction::OpenPlaylist(id.clone()), window, cx);
            after(window, cx);
        })
        .child(div().flex_1().min_w_0().truncate().child(name))
        // Playlists owned by another user get a person marker.
        .when(shared, |s| {
            s.child(
                Icon::new(IconName::User)
                    .xsmall()
                    .text_color(cx.theme().muted_foreground),
            )
        })
}

pub fn render_sidebar(
    model: SidebarModel,
    on_action: impl Fn(SidebarAction, &mut Window, &mut App) + Clone + 'static,
    cx: &App,
) -> impl IntoElement {
    let collapsed = model.collapsed;
    let nav_item = |label: &'static str, icon: IconName, section: NavSection| {
        let on_action = on_action.clone();
        let is_active = model.active == Some(section);
        let is_vi_sel = model.vi_selected_section == Some(section);
        h_flex()
            .id(SharedString::from(label))
            .py_1p5()
            .items_center()
            .rounded_lg()
            .cursor_pointer()
            .text_sm()
            // Collapsed the label is gone, so the icon centres in the rail and
            // the name moves into a tooltip — an icon nobody can name is not
            // navigation.
            .when(collapsed, |s| s.justify_center().px_0())
            .when(!collapsed, |s| s.px_3().gap_2())
            .when(is_active, |s| {
                s.bg(cx.theme().muted).text_color(cx.theme().foreground)
            })
            .when(!is_active, |s| s.text_color(cx.theme().muted_foreground))
            .hover(|s| s.bg(cx.theme().muted))
            .when(is_vi_sel, |s| {
                s.border_l_2().border_color(cx.theme().primary)
            })
            .when(collapsed, |s| {
                s.tooltip(move |window, cx| Tooltip::new(label).build(window, cx))
            })
            .on_click(move |_, window, cx| on_action(SidebarAction::Select(section), window, cx))
            .child(row_icon(Icon::new(icon), collapsed))
            .when(!collapsed, |s| s.child(label))
    };

    // Fold handle: a grey icon row-mate, not a row. Expanded it rides the
    // first line that is already there — the Libraries header when the
    // switcher is shown, the first nav row otherwise — so folding costs no
    // vertical space. Collapsed it gets the top of the rail, the usual place
    // to find the way back out.
    let fold_handle = {
        let on_action = on_action.clone();
        let tip = if collapsed {
            "Expand sidebar"
        } else {
            "Collapse sidebar"
        };
        div()
            .id("sidebar-fold")
            .p_1()
            .rounded_md()
            .cursor_pointer()
            .text_color(cx.theme().muted_foreground)
            .hover(|s| s.bg(cx.theme().muted))
            .tooltip(move |window, cx| Tooltip::new(tip).build(window, cx))
            .on_click(move |_, window, cx| on_action(SidebarAction::ToggleSidebar, window, cx))
            .child(row_icon(
                Icon::new(if collapsed {
                    IconName::PanelLeftOpen
                } else {
                    IconName::PanelLeftClose
                }),
                collapsed,
            ))
    };
    let mut fold_slot = Some(fold_handle);
    let rail_fold = if collapsed { fold_slot.take() } else { None };

    // Library switcher (only when the user can access more than one).
    // Checkboxes so several libraries can be browsed at once; checking
    // none (or all) means "all libraries". The section header folds it away.
    let mut library_switcher = v_flex().gap_0p5();
    let show_libraries = !collapsed && model.libraries.len() > 1;
    if show_libraries {
        // Header: label + chevron, click to collapse/expand.
        {
            let on_action = on_action.clone();
            library_switcher = library_switcher.child(
                h_flex()
                    .w_full()
                    .pr_1()
                    .pt_2()
                    .pb_1()
                    .items_center()
                    .justify_between()
                    .child(
                        h_flex()
                            .id("lib-header")
                            .pl_3()
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
                    )
                    .when_some(fold_slot.take(), |s, fold| s.child(fold)),
            );
        }
        if !model.libraries_collapsed {
            library_switcher = library_switcher.child(library_row(
                "lib-all".into(),
                "All libraries".into(),
                model.selected_libraries.is_empty(),
                SidebarAction::AllLibraries,
                on_action.clone(),
                cx,
            ));
            for (id, name) in model.libraries.iter() {
                let checked = model.selected_libraries.contains(id);
                library_switcher = library_switcher.child(library_row(
                    SharedString::from(format!("lib-{id}")),
                    name.clone(),
                    checked,
                    SidebarAction::ToggleLibrary(id.clone()),
                    on_action.clone(),
                    cx,
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
                div()
                    .id("pl-new")
                    .p_1()
                    .rounded_md()
                    .cursor_pointer()
                    .text_color(cx.theme().muted_foreground)
                    .hover(|s| s.bg(cx.theme().muted))
                    .on_click(move |_, window, cx| on_new(SidebarAction::NewPlaylist, window, cx))
                    .child(Icon::new(IconName::Plus).xsmall()),
            )
    };

    let mut playlist_items: Vec<gpui::AnyElement> = Vec::new();
    if !collapsed && !model.playlists_collapsed {
        for playlist in model.playlists.iter() {
            let is_active = model.active_playlist.as_deref() == Some(playlist.0.as_str());
            playlist_items.push(
                playlist_row(
                    "sidebar-pl",
                    playlist,
                    is_active,
                    on_action.clone(),
                    stay_put(),
                    cx,
                )
                .into_any_element(),
            );
        }
    }

    // Collapsed, the switcher and the playlist list have nowhere to live: both
    // are lists of names and the rail is 52px wide. Each becomes an icon that
    // opens the very same rows in a dropdown, so folding the sidebar hides the
    // labels rather than the features. The rows come from the shared builders
    // above — a library still toggles a checkbox and leaves the menu open,
    // since selecting several is the point, while opening a playlist is a
    // navigation and closes it.
    let rail_libraries = {
        let on_action = on_action.clone();
        let libraries = model.libraries.clone();
        let selected = model.selected_libraries.clone();
        Popover::new("rail-libraries")
            .trigger(
                Button::new("rail-libraries-btn")
                    .ghost()
                    .icon(row_icon(app_icon(icons::LIBRARY), true))
                    .tooltip("Libraries"),
            )
            .content(move |_, _, cx| {
                let mut menu = v_flex()
                    .id("rail-lib-menu")
                    .gap_0p5()
                    .min_w(px(180.))
                    .max_h(px(320.))
                    .overflow_y_scroll()
                    .child(library_row(
                        "rail-lib-all".into(),
                        "All libraries".into(),
                        selected.is_empty(),
                        SidebarAction::AllLibraries,
                        on_action.clone(),
                        cx,
                    ));
                for (id, name) in libraries.iter() {
                    menu = menu.child(library_row(
                        SharedString::from(format!("rail-lib-{id}")),
                        name.clone(),
                        selected.contains(id),
                        SidebarAction::ToggleLibrary(id.clone()),
                        on_action.clone(),
                        cx,
                    ));
                }
                menu
            })
    };

    let rail_playlists = {
        let on_action = on_action.clone();
        let playlists = model.playlists.clone();
        let active = model.active_playlist.clone();
        Popover::new("rail-playlists")
            .trigger(
                Button::new("rail-playlists-btn")
                    .ghost()
                    .icon(row_icon(app_icon(icons::LIST_MUSIC), true))
                    .tooltip("Playlists"),
            )
            .content(move |_, _, cx| {
                // Closing is the popover state's own job, so the rows reach it
                // through its entity: a row's click handler is handed an
                // `&mut App`, which cannot dismiss anything on its own.
                let state = cx.entity();
                let dismiss: AfterPick = Rc::new(move |window, cx| {
                    state.update(cx, |state, cx| state.dismiss(window, cx));
                });
                let on_new = on_action.clone();
                let after_new = dismiss.clone();
                let mut menu = v_flex()
                    .id("rail-pl-menu")
                    .gap_0p5()
                    .min_w(px(200.))
                    .max_h(px(360.))
                    .overflow_y_scroll()
                    .child(
                        h_flex()
                            .id("rail-pl-new")
                            .px_3()
                            .py_1()
                            .gap_1p5()
                            .items_center()
                            .rounded_lg()
                            .cursor_pointer()
                            .text_sm()
                            .text_color(cx.theme().muted_foreground)
                            .hover(|s| s.bg(cx.theme().muted))
                            .on_click(move |_, window, cx| {
                                on_new(SidebarAction::NewPlaylist, window, cx);
                                after_new(window, cx);
                            })
                            .child(Icon::new(IconName::Plus).xsmall())
                            .child("New playlist"),
                    );
                if playlists.is_empty() {
                    menu = menu.child(
                        div()
                            .px_3()
                            .py_1()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child("No playlists yet"),
                    );
                }
                for playlist in playlists.iter() {
                    menu = menu.child(playlist_row(
                        "rail-pl",
                        playlist,
                        active.as_deref() == Some(playlist.0.as_str()),
                        on_action.clone(),
                        dismiss.clone(),
                        cx,
                    ));
                }
                menu
            })
    };

    // Expanded, the fold handle rides the first row that exists; only the
    // collapsed rail spends a line on it.
    let albums = nav_item("Albums", IconName::LayoutDashboard, NavSection::Albums);
    let albums_row = match fold_slot.take() {
        Some(fold) => h_flex()
            .w_full()
            .items_center()
            .gap_1()
            .child(div().flex_1().min_w_0().child(albums))
            .child(fold)
            .into_any_element(),
        None => albums.into_any_element(),
    };

    v_flex()
        // Collapsed: wide enough for the icon rows' hover/active pill and
        // nothing else.
        .w(px(if collapsed { 52. } else { 210. }))
        .h_full()
        .when(collapsed, |s| s.px_1())
        .when(!collapsed, |s| s.px_2())
        .py_2()
        .gap_0p5()
        .border_r_1()
        .border_color(hsla(0., 0., 0.5, 0.15))
        .bg(cx.theme().sidebar)
        .when_some(rail_fold, |this, fold| {
            this.child(h_flex().w_full().justify_center().child(fold))
        })
        .when(show_libraries, |this| this.child(library_switcher))
        // The rail's stand-in for the switcher, in the same place the
        // switcher occupies expanded, and under the same "more than one
        // library" condition: with a single library there is nothing to pick.
        .when(collapsed && model.libraries.len() > 1, |this| {
            this.child(h_flex().w_full().justify_center().child(rail_libraries))
        })
        .child(albums_row)
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
        .child(nav_item(
            "Recent",
            IconName::GalleryVerticalEnd,
            NavSection::Recent,
        ))
        .child(nav_item("Radio", IconName::Globe, NavSection::Radio))
        .child(nav_item("Local", IconName::Folder, NavSection::LocalMusic))
        .child(div().px_3().child(super::divider()))
        .when(!collapsed, |this| this.child(playlists_header))
        // Same place the playlists header sits expanded, so the fold does not
        // move it.
        .when(collapsed, |this| {
            this.child(h_flex().w_full().justify_center().child(rail_playlists))
        })
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
            let label = if refreshing {
                SharedString::from(stage.label())
            } else {
                SharedString::from("Refresh library")
            };
            let tip = label.clone();
            v_flex()
                .id("sidebar-refresh")
                .py_1p5()
                .gap_1()
                .rounded_lg()
                .text_sm()
                .text_color(cx.theme().muted_foreground)
                .when(collapsed, |s| s.px_0())
                .when(!collapsed, |s| s.px_3())
                .when(!refreshing, |s| {
                    s.cursor_pointer().hover(|s| s.bg(cx.theme().muted))
                })
                // Collapsed the stage label has nowhere to go, so the tooltip
                // carries it — that text is the only sign the refresh moved.
                .when(collapsed, |s| {
                    s.tooltip(move |window, cx| Tooltip::new(tip.clone()).build(window, cx))
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
                        .when(collapsed, |s| s.justify_center())
                        .when(refreshing, |s| s.opacity(0.6))
                        .child(row_icon(
                            crate::assets::app_icon(crate::assets::icons::REFRESH),
                            collapsed,
                        ))
                        .when(!collapsed, |s| s.child(label)),
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
