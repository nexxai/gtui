use anyhow::{Result, Context};
use keyring::Entry;
use yup_oauth2::{InstalledFlowAuthenticator, InstalledFlowReturnMethod, ApplicationSecret, read_application_secret};
use google_gmail1::oauth2;
use std::path::Path;

use yup_oauth2::storage::{TokenStorage, TokenInfo};
use serde::{Serialize, Deserialize};
use async_trait::async_trait;

const APP_NAME: &str = "gtui";
const TOKEN_KEY: &str = "gmail_token";

#[derive(Debug, Default, Serialize, Deserialize)]
struct TokenData {
    tokens: Vec<TokenInfo>,
}

pub struct RingStorage;

#[async_trait]
impl TokenStorage for RingStorage {
    async fn set(&self, _scopes: &[&str], token: TokenInfo) -> Result<()> {
        let entry = Entry::new(APP_NAME, TOKEN_KEY)
            .map_err(|e| anyhow::anyhow!("Keyring error: {}", e))?;
        
        let mut data = self.get_all().await.unwrap_or_default();
        data.tokens.clear();
        data.tokens.push(token);

        let serialized = serde_json::to_string(&data)
            .context("Failed to serialize tokens")?;
        
        entry.set_password(&serialized)
            .map_err(|e| anyhow::anyhow!("Keyring error: {}", e))?;
        
        Ok(())
    }

    async fn get(&self, _scopes: &[&str]) -> Option<TokenInfo> {
        self.get_all().await.ok().and_then(|data| data.tokens.first().cloned())
    }
}

impl RingStorage {
    async fn get_all(&self) -> Result<TokenData> {
        let entry = Entry::new(APP_NAME, TOKEN_KEY)
            .map_err(|e| anyhow::anyhow!("Keyring error: {}", e))?;
        
        match entry.get_password() {
            Ok(serialized) => serde_json::from_str(&serialized)
                .context("Failed to deserialize tokens"),
            Err(keyring::Error::NoEntry) => Ok(TokenData::default()),
            Err(e) => Err(anyhow::anyhow!("Keyring error: {}", e)),
        }
    }
}

pub struct Authenticator;

impl Authenticator {
    pub async fn load_secret<P: AsRef<Path>>(path: P) -> Result<ApplicationSecret> {
        read_application_secret(path).await.context("Failed to read application secret")
    }

    pub async fn authenticate(secret: ApplicationSecret) -> Result<oauth2::authenticator::Authenticator<hyper_rustls::HttpsConnector<hyper::client::HttpConnector>>> {
        let auth = InstalledFlowAuthenticator::builder(
            secret,
            InstalledFlowReturnMethod::HTTPRedirect,
        )
        .with_storage(Box::new(RingStorage))
        .build()
        .await
        .context("Failed to build authenticator")?;

        Ok(auth)
    }
}
