use anyhow::Result;
use google_youtube3::{hyper_rustls, hyper_util, yup_oauth2};
use hyper_rustls::HttpsConnector;
use hyper_util::client::legacy::connect::HttpConnector;
use std::path::PathBuf;
use std::pin::Pin;
use yup_oauth2::{
    InstalledFlowAuthenticator, InstalledFlowReturnMethod,
    authenticator::Authenticator,
    authenticator_delegate::{DefaultInstalledFlowDelegate, InstalledFlowDelegate},
};

pub type Auth = Authenticator<HttpsConnector<HttpConnector>>;

struct BrowserDelegate;

impl InstalledFlowDelegate for BrowserDelegate {
    fn present_user_url<'a>(
        &'a self,
        url: &'a str,
        need_code: bool,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<String, String>> + Send + 'a>> {
        Box::pin(async move {
            if let Err(e) = open::that(url) {
                eprintln!("Could not open browser: {e}. Visit manually:\n{url}");
            }
            if need_code {
                DefaultInstalledFlowDelegate
                    .present_user_url(url, need_code)
                    .await
            } else {
                Ok(String::new())
            }
        })
    }
}

fn secret_path() -> Result<PathBuf> {
    let client_id_path = std::env::current_dir()?.join("clientid");
    let client_id = std::fs::read_to_string(client_id_path)?.trim().to_string();
    Ok(std::env::current_dir()?.join(format!("secret/client_secret_{client_id}.json")))
}

pub async fn build_authenticator() -> Result<Auth> {
    let secret = yup_oauth2::read_application_secret(secret_path()?).await?;
    let auth = InstalledFlowAuthenticator::builder(secret, InstalledFlowReturnMethod::HTTPRedirect)
        .flow_delegate(Box::new(BrowserDelegate))
        .persist_tokens_to_disk("tokencache.json")
        .build()
        .await?;
    Ok(auth)
}

#[test]
fn test_secret_path() {
    dbg!(secret_path());
}
