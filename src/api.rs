use anyhow::Result;
use google_youtube3::{
    YouTube,
    api::{PlaylistItemListResponse, PlaylistListResponse},
    hyper_rustls, hyper_util,
};

pub type Hub =
    YouTube<hyper_rustls::HttpsConnector<hyper_util::client::legacy::connect::HttpConnector>>;

pub async fn get_playlists(hub: &Hub) -> Result<PlaylistListResponse> {
    let (_, response) = hub
        .playlists()
        .list(&vec!["snippet".to_string()])
        .mine(false)
        .doit()
        .await?;

    Ok(response)
}

pub async fn get_songs(pl_id: String) -> Result<PlaylistItemListResponse> {
    unimplemented!()
}
