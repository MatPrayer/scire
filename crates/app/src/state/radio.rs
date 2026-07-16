//! Internet radio state: station list + CRUD, and playing a live stream.

use gpui::{AppContext as _, Context, Entity};
use subsonic::{RadioStation, SubsonicClient};

use crate::services::runtime;
use crate::state::session::Session;

pub struct RadioState {
    session: Entity<Session>,
    pub stations: Vec<RadioStation>,
    pub error: Option<String>,
}

impl RadioState {
    pub fn new(session: Entity<Session>, cx: &mut Context<Self>) -> Self {
        cx.observe(&session, |this: &mut Self, _, cx| this.reload(cx))
            .detach();
        let mut this = Self {
            session,
            stations: Vec::new(),
            error: None,
        };
        this.reload(cx);
        this
    }

    fn client(&self, cx: &Context<Self>) -> Option<SubsonicClient> {
        self.session.read(cx).client.clone()
    }

    pub fn reload(&mut self, cx: &mut Context<Self>) {
        let Some(client) = self.client(cx) else {
            self.stations.clear();
            return;
        };
        cx.spawn(async move |this, cx| {
            let result = runtime::spawn_io(async move {
                client
                    .get_internet_radio_stations()
                    .await
                    .map_err(anyhow::Error::from)
            })
            .await;
            let _ = this.update(cx, |state, cx| {
                match result {
                    Ok(stations) => state.stations = stations,
                    Err(e) => state.error = Some(format!("{e:#}")),
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub fn create(&mut self, name: String, stream_url: String, cx: &mut Context<Self>) {
        let Some(client) = self.client(cx) else {
            return;
        };
        cx.spawn(async move |this, cx| {
            let result = runtime::spawn_io(async move {
                client
                    .create_internet_radio_station(&stream_url, &name, None)
                    .await
                    .map_err(anyhow::Error::from)
            })
            .await;
            let _ = this.update(cx, |state, cx| match result {
                Ok(()) => state.reload(cx),
                Err(e) => {
                    state.error = Some(format!("{e:#}"));
                    cx.notify();
                }
            });
        })
        .detach();
    }

    pub fn delete(&mut self, id: String, cx: &mut Context<Self>) {
        let Some(client) = self.client(cx) else {
            return;
        };
        cx.spawn(async move |this, cx| {
            let result = runtime::spawn_io(async move {
                client
                    .delete_internet_radio_station(&id)
                    .await
                    .map_err(anyhow::Error::from)
            })
            .await;
            let _ = this.update(cx, |state, cx| match result {
                Ok(()) => state.reload(cx),
                Err(e) => {
                    state.error = Some(format!("{e:#}"));
                    cx.notify();
                }
            });
        })
        .detach();
    }
}

pub fn init(session: Entity<Session>, cx: &mut gpui::App) -> Entity<RadioState> {
    cx.new(|cx| RadioState::new(session, cx))
}
