use anyhow::{Context, Result};
use google_gmail1::oauth2;
use keyring::Entry;
use std::path::Path;
use yup_oauth2::{
    ApplicationSecret, InstalledFlowAuthenticator, InstalledFlowReturnMethod,
    read_application_secret,
};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::future::Future;
use std::pin::Pin;
use yup_oauth2::authenticator_delegate::InstalledFlowDelegate;
use yup_oauth2::storage::{TokenInfo, TokenStorage};

const APP_NAME: &str = "gtui";
const TOKEN_KEY: &str = "gmail_token";

pub const SCOPES: &[&str] = &[
    "https://www.googleapis.com/auth/gmail.readonly",
    "https://www.googleapis.com/auth/gmail.send",
    "https://www.googleapis.com/auth/gmail.modify",
    "https://www.googleapis.com/auth/gmail.settings.basic",
];

#[derive(Debug, Default, Serialize, Deserialize)]
struct TokenData {
    tokens: Vec<TokenInfo>,
}

pub struct RingStorage;

#[async_trait]
impl TokenStorage for RingStorage {
    async fn set(&self, _scopes: &[&str], token: TokenInfo) -> Result<()> {
        let entry =
            Entry::new(APP_NAME, TOKEN_KEY).map_err(|e| anyhow::anyhow!("Keyring error: {}", e))?;

        let mut data = self.get_all().await.unwrap_or_default();
        data.tokens.clear();
        data.tokens.push(token);

        let serialized = serde_json::to_string(&data).context("Failed to serialize tokens")?;

        entry
            .set_password(&serialized)
            .map_err(|e| anyhow::anyhow!("Keyring error: {}", e))?;

        Ok(())
    }

    async fn get(&self, _scopes: &[&str]) -> Option<TokenInfo> {
        self.get_all()
            .await
            .ok()
            .and_then(|data| data.tokens.first().cloned())
    }
}

impl RingStorage {
    async fn get_all(&self) -> Result<TokenData> {
        let entry =
            Entry::new(APP_NAME, TOKEN_KEY).map_err(|e| anyhow::anyhow!("Keyring error: {}", e))?;

        match entry.get_password() {
            Ok(serialized) => {
                serde_json::from_str(&serialized).context("Failed to deserialize tokens")
            }
            Err(keyring::Error::NoEntry) => Ok(TokenData::default()),
            Err(e) => Err(anyhow::anyhow!("Keyring error: {}", e)),
        }
    }

    pub async fn clear_token(&self) -> Result<()> {
        let entry =
            Entry::new(APP_NAME, TOKEN_KEY).map_err(|e| anyhow::anyhow!("Keyring error: {}", e))?;
        match entry.delete_password() {
            Ok(_) => Ok(()),
            Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(anyhow::anyhow!("Keyring error: {}", e)),
        }
    }
}

pub struct TuiDelegate {
    pub tx: tokio::sync::mpsc::Sender<String>,
}

impl InstalledFlowDelegate for TuiDelegate {
    fn present_user_url<'a>(
        &'a self,
        url: &'a str,
        _need_code: bool,
    ) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send + 'a>> {
        let url = url.to_string();
        let tx = self.tx.clone();
        Box::pin(async move {
            let _ = tx.send(url.clone()).await;
            // Automatically try to open the browser
            let _ = open::that(&url);
            Ok(String::new())
        })
    }
}

pub struct Authenticator;

impl Authenticator {
    pub async fn load_secret<P: AsRef<Path>>(path: P) -> Result<ApplicationSecret> {
        read_application_secret(path)
            .await
            .context("Failed to read application secret")
    }

    pub async fn authenticate(
        secret: ApplicationSecret,
        delegate: TuiDelegate,
    ) -> Result<
        oauth2::authenticator::Authenticator<
            hyper_rustls::HttpsConnector<hyper::client::HttpConnector>,
        >,
    > {
        let auth =
            InstalledFlowAuthenticator::builder(secret, InstalledFlowReturnMethod::HTTPRedirect)
                .with_storage(Box::new(RingStorage))
                .flow_delegate(Box::new(delegate))
                .build()
                .await
                .context("Failed to build authenticator")?;

        Ok(auth)
    }
}
