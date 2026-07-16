//! Transport and media-control icons (app-local SVGs).

use gpui::{SharedString, prelude::*};
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::{Icon, IconNamed, Sizable as _};

#[derive(Clone, Copy)]
pub enum TransportIcon {
    Play,
    Pause,
    SkipBack,
    SkipForward,
    Shuffle,
    Repeat,
    RepeatOne,
    Volume,
}

impl IconNamed for TransportIcon {
    fn path(self) -> SharedString {
        SharedString::from(match self {
            Self::Play => "icons/play.svg",
            Self::Pause => "icons/pause.svg",
            Self::SkipBack => "icons/skip-back.svg",
            Self::SkipForward => "icons/skip-forward.svg",
            Self::Shuffle => "icons/shuffle.svg",
            Self::Repeat => "icons/repeat.svg",
            Self::RepeatOne => "icons/repeat-one.svg",
            Self::Volume => "icons/volume.svg",
        })
    }
}

pub fn transport_btn(id: &'static str, icon: TransportIcon, active: bool) -> Button {
    Button::new(id)
        .ghost()
        .xsmall()
        .icon(Icon::new(icon))
        .when(active, |b| b.primary())
}

pub fn transport_btn_small(id: &'static str, icon: TransportIcon, active: bool) -> Button {
    Button::new(id)
        .ghost()
        .small()
        .icon(Icon::new(icon))
        .when(active, |b| b.primary())
}
