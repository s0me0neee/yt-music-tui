use anyhow::Result;
use google_youtube3::{
    YouTube,
    api::{Playlist, PlaylistItemListResponse},
    hyper_rustls, hyper_util,
};

pub type Hub =
    YouTube<hyper_rustls::HttpsConnector<hyper_util::client::legacy::connect::HttpConnector>>;

pub async fn get_playlists(hub: &Hub) -> Result<Vec<Playlist>> {
    let mut all = Vec::new();
    let mut page_token: Option<String> = None;

    loop {
        let mut call = hub
            .playlists()
            .list(&vec!["snippet".to_string()])
            .add_scope("https://www.googleapis.com/auth/youtube.readonly")
            .mine(true)
            .max_results(50);

        if let Some(ref token) = page_token {
            call = call.page_token(token);
        }

        let (_, response) = call.doit().await?;
        all.extend(response.items.unwrap_or_default());

        match response.next_page_token {
            Some(token) => page_token = Some(token),
            None => break,
        }
    }

    Ok(all)
}

pub async fn get_songs(pl_id: String) -> Result<PlaylistItemListResponse> {
    unimplemented!()
}
