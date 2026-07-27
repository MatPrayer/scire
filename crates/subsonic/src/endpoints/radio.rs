use serde::Deserialize;

use crate::client::SubsonicClient;
use crate::error::Error;
use crate::models::RadioStation;

#[derive(Debug, Deserialize)]
struct StationsWrapper {
    #[serde(rename = "internetRadioStations")]
    stations: StationsInner,
}

#[derive(Debug, Deserialize)]
struct StationsInner {
    #[serde(rename = "internetRadioStation", default)]
    items: Vec<RadioStation>,
}

impl SubsonicClient {
    pub async fn get_internet_radio_stations(&self) -> Result<Vec<RadioStation>, Error> {
        let w: StationsWrapper = self.get("getInternetRadioStations", &[]).await?;
        Ok(w.stations.items)
    }

    pub async fn create_internet_radio_station(
        &self,
        stream_url: &str,
        name: &str,
        homepage_url: Option<&str>,
    ) -> Result<(), Error> {
        let mut params: Vec<(&str, &str)> = vec![("streamUrl", stream_url), ("name", name)];
        if let Some(hp) = homepage_url {
            params.push(("homepageUrl", hp));
        }
        self.get_empty("createInternetRadioStation", &params).await
    }

    pub async fn delete_internet_radio_station(&self, id: &str) -> Result<(), Error> {
        self.get_empty("deleteInternetRadioStation", &[("id", id)])
            .await
    }
}
