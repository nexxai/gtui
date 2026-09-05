use anyhow::{Context, Result};
use google_gmail1::oauth2;
use keyring::Entry;
use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use yup_oauth2::authenticator_delegate::InstalledFlowDelegate;
use yup_oauth2::storage::{TokenInfo, TokenStorage};
use yup_oauth2::{
    ApplicationSecret, InstalledFlowAuthenticator, InstalledFlowReturnMethod,
    read_application_secret,
};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

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

#[derive(Clone, Copy)]
pub struct RingStorage;

impl RingStorage {
    fn entry(&self) -> Result<Entry> {
        Entry::new(APP_NAME, TOKEN_KEY).map_err(|e| anyhow::anyhow!("Keyring error: {}", e))
    }

    fn load_data(&self) -> Result<TokenData> {
        let entry = self.entry()?;
        match entry.get_password() {
            Ok(serialized) => {
                serde_json::from_str(&serialized).context("Failed to deserialize tokens")
            }
            Err(keyring::Error::NoEntry) => Ok(TokenData::default()),
            Err(e) => Err(anyhow::anyhow!("Keyring error: {}", e)),
        }
    }

    pub async fn clear_token(&self) -> Result<()> {
        let entry = self.entry()?;
        match entry.delete_password() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(anyhow::anyhow!("Keyring error: {}", e)),
        }
    }
}

#[async_trait]
impl TokenStorage for RingStorage {
    async fn set(&self, _scopes: &[&str], token: TokenInfo) -> Result<()> {
        let entry = self.entry()?;
        let data = TokenData {
            tokens: vec![token],
        };
        let serialized = serde_json::to_string(&data).context("Failed to serialize tokens")?;
        entry
            .set_password(&serialized)
            .map_err(|e| anyhow::anyhow!("Keyring error: {}", e))
    }

    async fn get(&self, _scopes: &[&str]) -> Option<TokenInfo> {
        self.load_data()
            .ok()
            .and_then(|data| data.tokens.into_iter().next())
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
            let _ = open::that(&url);
            Ok(String::new())
        })
    }
}

pub async fn load_secret(path: impl AsRef<Path>) -> Result<ApplicationSecret> {
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
    InstalledFlowAuthenticator::builder(secret, InstalledFlowReturnMethod::HTTPRedirect)
        .with_storage(Box::new(RingStorage))
        .flow_delegate(Box::new(delegate))
        .build()
        .await
        .context("Failed to build authenticator")
}
