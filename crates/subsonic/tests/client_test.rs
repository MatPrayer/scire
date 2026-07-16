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
    assert_eq!(q["c"], "navidrome-rusty");
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
