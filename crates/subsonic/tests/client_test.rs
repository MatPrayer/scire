use subsonic::{AlbumListType, ApiErrorCode, Credentials, Error, SubsonicClient};
use wiremock::matchers::{path, query_param};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

fn client(base: &str) -> SubsonicClient {
    SubsonicClient::new(base, Credentials::new("joe", "sesame")).unwrap()
}

fn ok_body(data: &str) -> String {
    format!(
        r#"{{"subsonic-response":{{"status":"ok","version":"1.16.1","type":"navidrome","serverVersion":"0.58.0",{data}}}}}"#
    )
}

#[tokio::test]
async fn ping_sends_token_auth_params() {
    let server = MockServer::start().await;
    Mock::given(path("/rest/ping"))
        .respond_with(ResponseTemplate::new(200).set_body_string(ok_body(r#""openSubsonic":true"#)))
        .expect(1)
        .mount(&server)
        .await;

    let c = client(&server.uri());
    let info = c.ping().await.unwrap();
    assert_eq!(info.server_version.as_deref(), Some("0.58.0"));

    // Verify auth params: t must equal md5(password + salt) for the sent salt.
    let requests = server.received_requests().await.unwrap();
    let req: &Request = &requests[0];
    let q: std::collections::HashMap<_, _> = req.url.query_pairs().collect();
    assert_eq!(q["u"], "joe");
    assert_eq!(q["v"], "1.16.1");
    assert_eq!(q["c"], "Scirè");
    assert_eq!(q["f"], "json");
    let salt = q["s"].to_string();
    let expected = format!("{:x}", md5_of(&format!("sesame{salt}")));
    assert_eq!(q["t"], expected);
}

// Tiny md5 helper reusing the crate's dependency.
fn md5_of(input: &str) -> md5::digest::Output<md5::Md5> {
    use md5::{Digest, Md5};
    let mut h = Md5::new();
    h.update(input.as_bytes());
    h.finalize()
}

#[tokio::test]
async fn wrong_credentials_maps_to_error_40() {
    let server = MockServer::start().await;
    Mock::given(path("/rest/ping"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            r#"{"subsonic-response":{"status":"failed","version":"1.16.1","error":{"code":40,"message":"Wrong username or password"}}}"#,
        ))
        .mount(&server)
        .await;

    let err = client(&server.uri()).ping().await.unwrap_err();
    match &err {
        Error::Api { code, message } => {
            assert_eq!(*code, ApiErrorCode::WrongCredentials);
            assert_eq!(message, "Wrong username or password");
        }
        other => panic!("expected Api error, got {other:?}"),
    }
    assert!(err.is_auth_failure());
}

#[tokio::test]
async fn album_list2_parses_and_paginates() {
    let server = MockServer::start().await;
    Mock::given(path("/rest/getAlbumList2"))
        .and(query_param("type", "alphabeticalByName"))
        .and(query_param("size", "2"))
        .and(query_param("offset", "4"))
        .and(query_param("musicFolderId", "lib1"))
        .respond_with(ResponseTemplate::new(200).set_body_string(ok_body(
            r#""albumList2":{"album":[
                {"id":"al-1","name":"Abbey Road","artist":"The Beatles","artistId":"ar-1","coverArt":"al-1","songCount":17,"duration":2830,"year":1969},
                {"id":"al-2","name":"Animals","artist":"Pink Floyd","songCount":5}
            ]}"#,
        )))
        .mount(&server)
        .await;

    let albums = client(&server.uri())
        .get_album_list2(
            AlbumListType::AlphabeticalByName,
            2,
            4,
            Some(&"lib1".to_string()),
        )
        .await
        .unwrap();
    assert_eq!(albums.len(), 2);
    assert_eq!(albums[0].name, "Abbey Road");
    assert_eq!(albums[0].year, Some(1969));
    assert_eq!(albums[1].artist.as_deref(), Some("Pink Floyd"));
    assert_eq!(albums[1].year, None);
}

#[tokio::test]
async fn get_album_parses_songs() {
    let server = MockServer::start().await;
    Mock::given(path("/rest/getAlbum"))
        .and(query_param("id", "al-1"))
        .respond_with(ResponseTemplate::new(200).set_body_string(ok_body(
            r#""album":{"id":"al-1","name":"Abbey Road","artist":"The Beatles","song":[
                {"id":"s-1","title":"Come Together","track":1,"duration":259,"suffix":"flac"},
                {"id":"s-2","title":"Something","track":2,"duration":183}
            ]}"#,
        )))
        .mount(&server)
        .await;

    let album = client(&server.uri()).get_album("al-1").await.unwrap();
    assert_eq!(album.album.name, "Abbey Road");
    assert_eq!(album.song.len(), 2);
    assert_eq!(album.song[0].title, "Come Together");
    assert_eq!(album.song[0].duration, Some(259));
}

#[tokio::test]
async fn get_artists_flattens_index() {
    let server = MockServer::start().await;
    Mock::given(path("/rest/getArtists"))
        .respond_with(ResponseTemplate::new(200).set_body_string(ok_body(
            r#""artists":{"index":[
                {"name":"B","artist":[{"id":"ar-1","name":"The Beatles","albumCount":13}]},
                {"name":"P","artist":[{"id":"ar-2","name":"Pink Floyd","albumCount":15}]}
            ]}"#,
        )))
        .mount(&server)
        .await;

    let index = client(&server.uri()).get_artists(None).await.unwrap();
    assert_eq!(index.len(), 2);
    assert_eq!(index[0].artist[0].name, "The Beatles");
    assert_eq!(index[1].artist[0].album_count, Some(15));
}

#[tokio::test]
async fn stream_and_cover_urls_carry_auth() {
    let c = client("https://demo.example.com/music");
    let url = c
        .stream_url("s-1", &subsonic::StreamOptions::default())
        .unwrap();
    assert!(url.path().ends_with("/music/rest/stream"));
    let q: std::collections::HashMap<_, _> = url.query_pairs().collect();
    assert_eq!(q["id"], "s-1");
    assert!(q.contains_key("t") && q.contains_key("s") && q.contains_key("u"));

    let art = c.cover_art_url("al-1", Some(300)).unwrap();
    assert!(art.path().ends_with("/music/rest/getCoverArt"));
    let q: std::collections::HashMap<_, _> = art.query_pairs().collect();
    assert_eq!(q["size"], "300");
}

#[tokio::test]
async fn scrobble_sends_submission_flag() {
    let server = MockServer::start().await;
    Mock::given(path("/rest/scrobble"))
        .and(query_param("id", "s-1"))
        .and(query_param("submission", "true"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(r#"{"subsonic-response":{"status":"ok","version":"1.16.1"}}"#),
        )
        .expect(1)
        .mount(&server)
        .await;

    client(&server.uri()).scrobble("s-1", true).await.unwrap();
}

#[tokio::test]
async fn get_lyrics_sends_params_and_parses() {
    let server = MockServer::start().await;
    Mock::given(path("/rest/getLyrics"))
        .and(query_param("artist", "Muse"))
        .and(query_param("title", "Uprising"))
        .respond_with(ResponseTemplate::new(200).set_body_string(ok_body(
            r#""lyrics":{"artist":"Muse","title":"Uprising","value":"Paranoia is in bloom\nThe PR transmissions will resume"}"#,
        )))
        .expect(1)
        .mount(&server)
        .await;

    let lyrics = client(&server.uri())
        .get_lyrics(Some("Muse"), Some("Uprising"))
        .await
        .unwrap();
    assert_eq!(lyrics.artist.as_deref(), Some("Muse"));
    assert!(lyrics.value.unwrap().starts_with("Paranoia"));
}

#[tokio::test]
async fn get_lyrics_missing_is_empty() {
    let server = MockServer::start().await;
    Mock::given(path("/rest/getLyrics"))
        .respond_with(ResponseTemplate::new(200).set_body_string(ok_body(r#""lyrics":{}"#)))
        .mount(&server)
        .await;

    let lyrics = client(&server.uri())
        .get_lyrics(Some("Nobody"), Some("Nothing"))
        .await
        .unwrap();
    assert!(lyrics.value.is_none());
}

#[tokio::test]
async fn get_artist_info2_parses_bio_and_images() {
    let server = MockServer::start().await;
    Mock::given(path("/rest/getArtistInfo2"))
        .and(query_param("id", "ar-1"))
        .respond_with(ResponseTemplate::new(200).set_body_string(ok_body(
            r#""artistInfo2":{"biography":"Legendary band. <a target='_blank' href=\"https://last.fm/x\" rel=\"nofollow\">Read more on Last.fm</a>","musicBrainzId":"mbid-1","lastFmUrl":"https://last.fm/x","smallImageUrl":"https://img/s.jpg","mediumImageUrl":"https://img/m.jpg","largeImageUrl":"https://img/l.jpg"}"#,
        )))
        .expect(1)
        .mount(&server)
        .await;

    let info = client(&server.uri())
        .get_artist_info2("ar-1")
        .await
        .unwrap();
    assert!(
        info.biography
            .as_ref()
            .unwrap()
            .starts_with("Legendary band.")
    );
    assert_eq!(info.image_url(), Some("https://img/l.jpg"));
}

#[tokio::test]
async fn get_album_info2_parses_notes_from_album_info_element() {
    let server = MockServer::start().await;
    // The ID3 call answers under `albumInfo`, not `albumInfo2`.
    Mock::given(path("/rest/getAlbumInfo2"))
        .and(query_param("id", "al-1"))
        .respond_with(ResponseTemplate::new(200).set_body_string(ok_body(
            r#""albumInfo":{"notes":"Recorded in <b>1973</b>.","musicBrainzId":"mbid-al","lastFmUrl":"https://last.fm/al","largeImageUrl":"https://img/l.jpg"}"#,
        )))
        .expect(1)
        .mount(&server)
        .await;

    let info = client(&server.uri()).get_album_info2("al-1").await.unwrap();
    assert_eq!(info.notes.as_deref(), Some("Recorded in <b>1973</b>."));
    assert_eq!(info.music_brainz_id.as_deref(), Some("mbid-al"));
    assert_eq!(info.last_fm_url.as_deref(), Some("https://last.fm/al"));
}

#[tokio::test]
async fn get_album_info2_missing_element_defaults() {
    let server = MockServer::start().await;
    Mock::given(path("/rest/getAlbumInfo2"))
        .respond_with(ResponseTemplate::new(200).set_body_string(ok_body(r#""dummy":1"#)))
        .mount(&server)
        .await;

    let info = client(&server.uri()).get_album_info2("al-2").await.unwrap();
    assert!(info.notes.is_none());
    assert!(info.music_brainz_id.is_none());
}

#[tokio::test]
async fn get_artist_info2_missing_fields_default() {
    let server = MockServer::start().await;
    Mock::given(path("/rest/getArtistInfo2"))
        .respond_with(ResponseTemplate::new(200).set_body_string(ok_body(r#""artistInfo2":{}"#)))
        .mount(&server)
        .await;

    let info = client(&server.uri())
        .get_artist_info2("ar-2")
        .await
        .unwrap();
    assert!(info.biography.is_none());
    assert!(info.image_url().is_none());
}

#[tokio::test]
async fn start_scan_parses_scan_status() {
    let server = MockServer::start().await;
    Mock::given(path("/rest/startScan"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(ok_body(r#""scanStatus":{"scanning":true,"count":1234}"#)),
        )
        .expect(1)
        .mount(&server)
        .await;

    let status = client(&server.uri()).start_scan().await.unwrap();
    assert!(status.scanning);
    assert_eq!(status.count, Some(1234));
}

#[tokio::test]
async fn get_scan_status_parses_idle_without_count() {
    let server = MockServer::start().await;
    Mock::given(path("/rest/getScanStatus"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(ok_body(r#""scanStatus":{"scanning":false}"#)),
        )
        .expect(1)
        .mount(&server)
        .await;

    let status = client(&server.uri()).get_scan_status().await.unwrap();
    assert!(!status.scanning);
    assert!(status.count.is_none());
}

#[tokio::test]
async fn start_scan_maps_not_authorized_error() {
    let server = MockServer::start().await;
    Mock::given(path("/rest/startScan"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            r#"{"subsonic-response":{"status":"failed","version":"1.16.1","error":{"code":50,"message":"forbidden"}}}"#,
        ))
        .mount(&server)
        .await;

    let err = client(&server.uri()).start_scan().await.unwrap_err();
    assert!(matches!(err, Error::Api { .. }));
}
