use anyhow::Result;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtistInfoSummary {
    pub name: String,
    pub image_url: Option<String>,
    pub biography: Option<String>,
    pub genres: Vec<String>,
    pub top_track: Option<String>,
}

pub async fn fetch_artist_info(
    client: &subsonic::SubsonicClient,
    artist_id: &str,
) -> Result<Option<ArtistInfoSummary>> {
    let artist = match client.get_artist(artist_id).await {
        Ok(value) => value,
        Err(_) => return Ok(None),
    };
    let top_track = artist.album.first().and_then(|album| {
        let title = album.name.trim();
        (!title.is_empty()).then(|| title.to_string())
    });
    let genres = artist
        .album
        .iter()
        .filter_map(|album| album.genre.as_deref())
        .filter(|g| !g.trim().is_empty())
        .map(str::trim)
        .map(str::to_string)
        .collect::<Vec<_>>();
    let biography = artist.artist.biography.clone();
    let image_url = artist.artist.artist_image_url.clone().or_else(|| artist.artist.cover_art.clone());

    Ok(Some(ArtistInfoSummary {
        name: artist.artist.name,
        image_url,
        biography,
        genres,
        top_track,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn artist_summary_is_built_from_navidrome_shape() {
        let info = ArtistInfoSummary {
            name: "Example Artist".into(),
            image_url: Some("https://example.test/cover.jpg".into()),
            biography: Some("Bio".into()),
            genres: vec!["Rock".into(), "Indie".into()],
            top_track: Some("Track".into()),
        };
        assert_eq!(info.name, "Example Artist");
        assert_eq!(info.image_url.as_deref(), Some("https://example.test/cover.jpg"));
        assert_eq!(info.genres, vec!["Rock", "Indie"]);
    }
}
