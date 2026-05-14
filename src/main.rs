mod api;
mod app;
mod auth;

use anyhow::Ok;
use google_youtube3::{YouTube, hyper_rustls, hyper_util};
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

    let playlists = api::get_playlists(&hub).await?;
    dbg!(playlists.len());
    app::App::new(playlists).run()
}
