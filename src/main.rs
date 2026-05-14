mod api;
mod app;
mod auth;

use anyhow::Context;
use google_youtube3::{YouTube, api::Playlist, hyper_rustls, hyper_util};
use hyper_util::{client::legacy::Client, rt::TokioExecutor};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let authenticator = auth::build_authenticator().await?;

    let connector = hyper_rustls::HttpsConnectorBuilder::new()
        .with_native_roots()?
        .https_only()
        .enable_http2()
        .build();

    let hub = YouTube::new(
        Client::builder(TokioExecutor::new()).build(connector),
        authenticator,
    );

    let response = api::get_playlists(&hub).await?;
    let titles: Vec<Playlist> = response.items.unwrap_or_default();

    app::App::new(titles).run()
}
