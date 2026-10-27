use subsonic::{Credentials, SubsonicClient};
use wiremock::matchers::{method, path, query_param};
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
async fn get_playlists_parses_list() {
    let server = MockServer::start().await;
    Mock::given(path("/rest/getPlaylists"))
        .respond_with(ResponseTemplate::new(200).set_body_string(ok_body(
            r#""playlists":{"playlist":[
                {"id":"pl-1","name":"Road Trip","songCount":24,"duration":5400,"owner":"joe","public":false},
                {"id":"pl-2","name":"Focus","songCount":10}
            ]}"#,
        )))
        .mount(&server)
        .await;

    let playlists = client(&server.uri()).get_playlists().await.unwrap();
    assert_eq!(playlists.len(), 2);
    assert_eq!(playlists[0].name, "Road Trip");
    assert_eq!(playlists[0].song_count, Some(24));
}

#[tokio::test]
async fn get_playlist_parses_entries() {
    let server = MockServer::start().await;
    Mock::given(path("/rest/getPlaylist"))
        .and(query_param("id", "pl-1"))
        .respond_with(ResponseTemplate::new(200).set_body_string(ok_body(
            r#""playlist":{"id":"pl-1","name":"Road Trip","songCount":2,"entry":[
                {"id":"s-1","title":"Song A","duration":100},
                {"id":"s-2","title":"Song B","duration":200}
            ]}"#,
        )))
        .mount(&server)
        .await;

    let pl = client(&server.uri()).get_playlist("pl-1").await.unwrap();
    assert_eq!(pl.playlist.name, "Road Trip");
    assert_eq!(pl.songs.len(), 2);
    assert_eq!(pl.songs[1].title, "Song B");
}

#[tokio::test]
async fn create_playlist_sends_song_ids() {
    let server = MockServer::start().await;
    Mock::given(path("/rest/createPlaylist"))
        .and(query_param("name", "New List"))
        .respond_with(ResponseTemplate::new(200).set_body_string(ok_body(
            r#""playlist":{"id":"pl-9","name":"New List","songCount":2,"entry":[
                {"id":"s-1","title":"A"},{"id":"s-2","title":"B"}
            ]}"#,
        )))
        .expect(1)
        .mount(&server)
        .await;

    let pl = client(&server.uri())
        .create_playlist("New List", &["s-1", "s-2"])
        .await
        .unwrap();
    assert_eq!(pl.playlist.id, "pl-9");

    // Both songId params present.
    let reqs = server.received_requests().await.unwrap();
    let ids: Vec<_> = reqs[0]
        .url
        .query_pairs()
        .filter(|(k, _)| k == "songId")
        .map(|(_, v)| v.to_string())
        .collect();
    assert_eq!(ids, vec!["s-1", "s-2"]);
}

#[tokio::test]
async fn update_playlist_sends_indices_and_additions() {
    let server = MockServer::start().await;
    Mock::given(path("/rest/updatePlaylist"))
        .and(query_param("playlistId", "pl-1"))
        .and(query_param("name", "Renamed"))
        .and(query_param("comment", "desc"))
        .respond_with(ResponseTemplate::new(200).set_body_string(ok_empty()))
        .expect(1)
        .mount(&server)
        .await;

    client(&server.uri())
        .update_playlist("pl-1", Some("Renamed"), Some("desc"), &["s-9"], &[0, 3])
        .await
        .unwrap();

    let reqs = server.received_requests().await.unwrap();
    let q: Vec<(String, String)> = reqs[0]
        .url
        .query_pairs()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    assert!(q.contains(&("songIdToAdd".into(), "s-9".into())));
    assert!(q.contains(&("songIndexToRemove".into(), "0".into())));
    assert!(q.contains(&("songIndexToRemove".into(), "3".into())));
}

#[tokio::test]
async fn delete_playlist_hits_endpoint() {
    let server = MockServer::start().await;
    Mock::given(path("/rest/deletePlaylist"))
        .and(query_param("id", "pl-1"))
        .respond_with(ResponseTemplate::new(200).set_body_string(ok_empty()))
        .expect(1)
        .mount(&server)
        .await;

    client(&server.uri()).delete_playlist("pl-1").await.unwrap();
}

#[tokio::test]
async fn set_playlist_public_sends_flag() {
    let server = MockServer::start().await;
    Mock::given(path("/rest/updatePlaylist"))
        .and(query_param("playlistId", "pl-1"))
        .and(query_param("public", "true"))
        .respond_with(ResponseTemplate::new(200).set_body_string(ok_empty()))
        .expect(1)
        .mount(&server)
        .await;

    client(&server.uri())
        .set_playlist_public("pl-1", true)
        .await
        .unwrap();
}

#[tokio::test]
async fn replace_playlist_songs_overwrites_in_order() {
    let server = MockServer::start().await;
    Mock::given(path("/rest/createPlaylist"))
        .and(query_param("playlistId", "pl-1"))
        .respond_with(ResponseTemplate::new(200).set_body_string(ok_empty()))
        .expect(1)
        .mount(&server)
        .await;

    client(&server.uri())
        .replace_playlist_songs("pl-1", &["s-3", "s-1", "s-2"])
        .await
        .unwrap();

    let reqs = server.received_requests().await.unwrap();
    let ids: Vec<_> = reqs[0]
        .url
        .query_pairs()
        .filter(|(k, _)| k == "songId")
        .map(|(_, v)| v.to_string())
        .collect();
    assert_eq!(ids, vec!["s-3", "s-1", "s-2"]);
    // No name: the server keeps the playlist's own.
    assert!(!reqs[0].url.query_pairs().any(|(k, _)| k == "name"));
}

#[tokio::test]
async fn a_long_song_list_goes_as_a_form_body() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/rest/createPlaylist"))
        .and(query_param("u", "joe"))
        .respond_with(ResponseTemplate::new(200).set_body_string(ok_empty()))
        .expect(1)
        .mount(&server)
        .await;

    let ids: Vec<String> = (0..500).map(|i| format!("song-id-{i:05}")).collect();
    let refs: Vec<&str> = ids.iter().map(String::as_str).collect();
    client(&server.uri())
        .replace_playlist_songs("pl-1", &refs)
        .await
        .unwrap();

    let reqs = server.received_requests().await.unwrap();
    assert!(!reqs[0].url.query_pairs().any(|(k, _)| k == "songId"));
    // Plain ASCII ids: nothing in the body needs percent-decoding.
    let body: Vec<(String, String)> = String::from_utf8(reqs[0].body.clone())
        .unwrap()
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    assert!(body.contains(&("playlistId".into(), "pl-1".into())));
    let sent: Vec<_> = body.iter().filter(|(k, _)| k == "songId").collect();
    assert_eq!(sent.len(), 500);
    assert_eq!(sent[0].1, "song-id-00000");
    assert_eq!(sent[499].1, "song-id-00499");
}

#[tokio::test]
async fn star_uses_target_specific_param() {
    let server = MockServer::start().await;
    Mock::given(path("/rest/star"))
        .and(query_param("albumId", "al-1"))
        .respond_with(ResponseTemplate::new(200).set_body_string(ok_empty()))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(path("/rest/unstar"))
        .and(query_param("id", "s-1"))
        .respond_with(ResponseTemplate::new(200).set_body_string(ok_empty()))
        .expect(1)
        .mount(&server)
        .await;

    let c = client(&server.uri());
    c.star("albumId", "al-1").await.unwrap();
    c.unstar("id", "s-1").await.unwrap();
}

#[tokio::test]
async fn set_rating_clamps_to_five() {
    let server = MockServer::start().await;
    Mock::given(path("/rest/setRating"))
        .and(query_param("id", "s-1"))
        .and(query_param("rating", "5"))
        .respond_with(ResponseTemplate::new(200).set_body_string(ok_empty()))
        .expect(1)
        .mount(&server)
        .await;

    client(&server.uri()).set_rating("s-1", 9).await.unwrap();
}

#[tokio::test]
async fn get_starred2_parses_sections() {
    let server = MockServer::start().await;
    Mock::given(path("/rest/getStarred2"))
        .respond_with(ResponseTemplate::new(200).set_body_string(ok_body(
            r#""starred2":{
                "artist":[{"id":"ar-1","name":"The Beatles"}],
                "album":[{"id":"al-1","name":"Abbey Road"}],
                "song":[{"id":"s-1","title":"Come Together"}]
            }"#,
        )))
        .mount(&server)
        .await;

    let starred = client(&server.uri()).get_starred2(None).await.unwrap();
    assert_eq!(starred.artist.len(), 1);
    assert_eq!(starred.album.len(), 1);
    assert_eq!(starred.song[0].title, "Come Together");
}
