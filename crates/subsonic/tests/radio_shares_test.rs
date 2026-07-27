use subsonic::{Credentials, SubsonicClient};
use wiremock::matchers::{path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn client(base: &str) -> SubsonicClient {
    SubsonicClient::new(base, Credentials::new("joe", "sesame")).unwrap()
}

fn ok_body(data: &str) -> String {
    format!(r#"{{"subsonic-response":{{"status":"ok","version":"1.16.1",{data}}}}}"#)
}

fn ok_empty() -> String {
    r#"{"subsonic-response":{"status":"ok","version":"1.16.1"}}"#.to_string()
}

#[tokio::test]
async fn get_radio_stations_parses() {
    let server = MockServer::start().await;
    Mock::given(path("/rest/getInternetRadioStations"))
        .respond_with(ResponseTemplate::new(200).set_body_string(ok_body(
            r#""internetRadioStations":{"internetRadioStation":[
                {"id":"1","name":"SomaFM","streamUrl":"https://ice.somafm.com/groovesalad","homePageUrl":"https://somafm.com"},
                {"id":"2","name":"Radio Paradise","streamUrl":"https://stream.radioparadise.com/aac-320"}
            ]}"#,
        )))
        .mount(&server)
        .await;

    let stations = client(&server.uri())
        .get_internet_radio_stations()
        .await
        .unwrap();
    assert_eq!(stations.len(), 2);
    assert_eq!(stations[0].name, "SomaFM");
    assert_eq!(
        stations[1].stream_url,
        "https://stream.radioparadise.com/aac-320"
    );
}

#[tokio::test]
async fn create_radio_station_sends_params() {
    let server = MockServer::start().await;
    Mock::given(path("/rest/createInternetRadioStation"))
        .and(query_param("streamUrl", "https://example.com/stream"))
        .and(query_param("name", "My Station"))
        .respond_with(ResponseTemplate::new(200).set_body_string(ok_empty()))
        .expect(1)
        .mount(&server)
        .await;

    client(&server.uri())
        .create_internet_radio_station("https://example.com/stream", "My Station", None)
        .await
        .unwrap();
}

#[tokio::test]
async fn delete_radio_station() {
    let server = MockServer::start().await;
    Mock::given(path("/rest/deleteInternetRadioStation"))
        .and(query_param("id", "3"))
        .respond_with(ResponseTemplate::new(200).set_body_string(ok_empty()))
        .expect(1)
        .mount(&server)
        .await;

    client(&server.uri())
        .delete_internet_radio_station("3")
        .await
        .unwrap();
}

