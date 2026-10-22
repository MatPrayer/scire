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
            self.error = None;
            cx.notify();
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
                    Ok(stations) => {
                        state.stations = stations;
                        state.error = None;
                    }
                    Err(e) => state.error = Some(crate::errors::error_text(&e)),
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub fn create(
        &mut self,
        name: String,
        stream_url: String,
        homepage_url: Option<String>,
        cx: &mut Context<Self>,
    ) {
        self.mutate(cx, move |client| async move {
            client
                .create_internet_radio_station(&stream_url, &name, homepage_url.as_deref())
                .await
        });
    }

    pub fn update(
        &mut self,
        id: String,
        name: String,
        stream_url: String,
        homepage_url: Option<String>,
        cx: &mut Context<Self>,
    ) {
        self.mutate(cx, move |client| async move {
            client
                .update_internet_radio_station(&id, &stream_url, &name, homepage_url.as_deref())
                .await
        });
    }

    pub fn delete(&mut self, id: String, cx: &mut Context<Self>) {
        self.mutate(cx, move |client| async move {
            client.delete_internet_radio_station(&id).await
        });
    }

    /// Run one station edit on the IO runtime, then reload the list (or show
    /// why it failed). The previous failure is cleared as the new edit starts,
    /// so it does not sit on screen while this one is in flight.
    fn mutate<F, Fut>(&mut self, cx: &mut Context<Self>, op: F)
    where
        F: FnOnce(SubsonicClient) -> Fut + Send + 'static,
        Fut: Future<Output = Result<(), subsonic::Error>> + Send + 'static,
    {
        let Some(client) = self.client(cx) else {
            return;
        };
        if self.error.take().is_some() {
            cx.notify();
        }
        cx.spawn(async move |this, cx| {
            let result =
                runtime::spawn_io(async move { op(client).await.map_err(anyhow::Error::from) })
                    .await;
            let _ = this.update(cx, |state, cx| match result {
                Ok(()) => state.reload(cx),
                Err(e) => {
                    state.error = Some(crate::errors::error_text(&e));
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
