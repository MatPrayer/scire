use serde::Deserialize;

use crate::client::SubsonicClient;
use crate::error::Error;
use crate::models::{Album, AlbumListType, LibraryId};

#[derive(Debug, Deserialize)]
struct AlbumList2Wrapper {
    #[serde(rename = "albumList2")]
    list: AlbumList2Inner,
}

#[derive(Debug, Deserialize)]
struct AlbumList2Inner {
    #[serde(default)]
    album: Vec<Album>,
}

impl SubsonicClient {
    /// Paginated album list (ID3). `size` max 500 per the spec.
    pub async fn get_album_list2(
        &self,
        list_type: AlbumListType,
        size: u32,
        offset: u32,
        music_folder_id: Option<&LibraryId>,
    ) -> Result<Vec<Album>, Error> {
        let size_s = size.to_string();
        let offset_s = offset.to_string();
        let mut params: Vec<(&str, &str)> = vec![
            ("type", list_type.as_str()),
            ("size", &size_s),
            ("offset", &offset_s),
        ];
        if let Some(id) = music_folder_id {
            params.push(("musicFolderId", id));
        }
        let w: AlbumList2Wrapper = self.get("getAlbumList2", &params).await?;
        Ok(w.list.album)
    }
}
