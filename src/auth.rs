use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use google_gmail1::oauth2;
use hyper::header::AUTHORIZATION;
use keyring::Entry;
use serde::{Deserialize, Serialize};
use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;
use tokio::sync::Mutex;
use yup_oauth2::authenticator_delegate::InstalledFlowDelegate;
use yup_oauth2::storage::{TokenInfo, TokenStorage};
use yup_oauth2::{
    ApplicationSecret, InstalledFlowAuthenticator, InstalledFlowReturnMethod,
    read_application_secret,
};

const APP_NAME: &str = "gtui";
const TOKEN_KEY: &str = "gmail_token";
const USERINFO_ENDPOINT: &str = "https://openidconnect.googleapis.com/v1/userinfo";

pub const GMAIL_MODIFY_SCOPE: &str = "https://www.googleapis.com/auth/gmail.modify";
pub const GMAIL_READONLY_SCOPE: &str = "https://www.googleapis.com/auth/gmail.readonly";
pub const GMAIL_SEND_SCOPE: &str = "https://www.googleapis.com/auth/gmail.send";
pub const GMAIL_SETTINGS_SCOPE: &str = "https://www.googleapis.com/auth/gmail.settings.basic";

pub const CANONICAL_SCOPES: &[&str] = &[
    "email",
    GMAIL_MODIFY_SCOPE,
    GMAIL_READONLY_SCOPE,
    GMAIL_SEND_SCOPE,
    GMAIL_SETTINGS_SCOPE,
    "openid",
];

#[derive(Clone, Serialize, Deserialize)]
struct StoredToken {
    scopes: Vec<String>,
    account_subject: String,
    token: TokenInfo,
}

#[derive(Default, Serialize, Deserialize)]
struct TokenData {
    #[serde(default)]
    entries: Vec<StoredToken>,
    #[serde(default, rename = "tokens", skip_serializing)]
    legacy_tokens: Vec<TokenInfo>,
}

impl TokenData {
    fn find(&self, scopes: &[String], active_subject: Option<&str>) -> Option<&StoredToken> {
        let candidates = self
            .entries
            .iter()
            .filter(|entry| {
                has_identity_scopes(&entry.scopes)
                    && scopes.iter().all(|scope| entry.scopes.contains(scope))
                    && active_subject.is_none_or(|subject| entry.account_subject == subject)
            })
            .collect::<Vec<_>>();
        let first = candidates.first()?;

        if active_subject.is_none()
            && candidates
                .iter()
                .any(|entry| entry.account_subject != first.account_subject)
        {
            return None;
        }

        candidates
            .into_iter()
            .min_by_key(|entry| entry.scopes.len())
    }

    fn upsert(&mut self, scopes: Vec<String>, account_subject: String, token: TokenInfo) {
        self.entries
            .retain(|entry| entry.account_subject != account_subject || entry.scopes != scopes);
        self.entries.push(StoredToken {
            scopes,
            account_subject,
            token,
        });
        self.legacy_tokens.clear();
    }
}

#[derive(Deserialize)]
struct UserInfo {
    sub: Option<String>,
    email: Option<String>,
    email_verified: Option<bool>,
}

struct GoogleSubjectVerifier {
    client: hyper::Client<hyper_rustls::HttpsConnector<hyper::client::HttpConnector>>,
}

impl GoogleSubjectVerifier {
    fn new() -> Result<Self> {
        let connector = hyper_rustls::HttpsConnectorBuilder::new()
            .with_native_roots()
            .context("failed to load native TLS roots")?
            .https_only()
            .enable_http1()
            .build();

        Ok(Self {
            client: hyper::Client::builder().build(connector),
        })
    }

    async fn verify(&self, token: &TokenInfo) -> Result<String> {
        let access_token = token
            .access_token
            .as_deref()
            .context("identity token has no access token")?;
        let request = hyper::Request::get(USERINFO_ENDPOINT)
            .header(AUTHORIZATION, format!("Bearer {access_token}"))
            .body(hyper::Body::empty())
            .context("failed to build identity verification request")?;
        let response = self
            .client
            .request(request)
            .await
            .context("identity verification request failed")?;
        if !response.status().is_success() {
            bail!("identity endpoint rejected the access token");
        }

        let body = hyper::body::to_bytes(response.into_body())
            .await
            .context("failed to read identity verification response")?;
        let user_info = serde_json::from_slice(&body)
            .context("failed to decode identity verification response")?;

        verified_subject(user_info)
    }
}

fn verified_subject(user_info: UserInfo) -> Result<String> {
    let subject = user_info.sub.context("identity response has no subject")?;
    if subject.is_empty() || subject.len() > 255 || !subject.is_ascii() {
        bail!("identity response has an invalid subject");
    }
    if user_info.email_verified != Some(true)
        || user_info
            .email
            .as_deref()
            .is_none_or(|address| address.trim().is_empty())
    {
        bail!("identity response has no verified address");
    }

    Ok(subject)
}

fn normalize_scopes(scopes: &[&str]) -> Vec<String> {
    let mut scopes = scopes
        .iter()
        .map(|scope| (*scope).to_string())
        .collect::<Vec<_>>();
    scopes.sort_unstable();
    scopes.dedup();
    scopes
}

fn has_identity_scopes(scopes: &[String]) -> bool {
    scopes.iter().any(|scope| scope == "openid") && scopes.iter().any(|scope| scope == "email")
}

fn storage_scopes(scopes: &[&str]) -> Vec<String> {
    let requested = normalize_scopes(scopes);
    let canonical = normalize_scopes(CANONICAL_SCOPES);

    if requested.iter().all(|scope| canonical.contains(scope)) {
        canonical
    } else {
        requested
    }
}

fn bind_subject(active_subject: &mut Option<String>, subject: &str) -> Result<()> {
    if active_subject
        .as_deref()
        .is_some_and(|active| active != subject)
    {
        bail!("authenticated account changed while the application was running");
    }
    *active_subject = Some(subject.to_string());
    Ok(())
}

#[derive(Clone)]
pub struct RingStorage {
    verifier: Arc<GoogleSubjectVerifier>,
    active_subject: Arc<Mutex<Option<String>>>,
    keyring_lock: Arc<Mutex<()>>,
}

impl RingStorage {
    fn new() -> Result<Self> {
        Ok(Self {
            verifier: Arc::new(GoogleSubjectVerifier::new()?),
            active_subject: Arc::new(Mutex::new(None)),
            keyring_lock: Arc::new(Mutex::new(())),
        })
    }

    fn entry() -> Result<Entry> {
        Entry::new(APP_NAME, TOKEN_KEY).map_err(|e| anyhow::anyhow!("Keyring error: {}", e))
    }

    fn load_data() -> Result<TokenData> {
        let entry = Self::entry()?;
        match entry.get_password() {
            Ok(serialized) => Ok(serde_json::from_str(&serialized).unwrap_or_default()),
            Err(keyring::Error::NoEntry) => Ok(TokenData::default()),
            Err(e) => Err(anyhow::anyhow!("Keyring error: {}", e)),
        }
    }

    fn save_data(data: &TokenData) -> Result<()> {
        let serialized = serde_json::to_string(data).context("Failed to serialize tokens")?;
        Self::entry()?
            .set_password(&serialized)
            .map_err(|e| anyhow::anyhow!("Keyring error: {}", e))
    }

    pub async fn account_subject(&self) -> Result<String> {
        self.active_subject
            .lock()
            .await
            .clone()
            .context("authenticated token has no verified subject")
    }

    pub async fn clear_token() -> Result<()> {
        let entry = Self::entry()?;
        match entry.delete_password() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(anyhow::anyhow!("Keyring error: {}", e)),
        }
    }
}

#[async_trait]
impl TokenStorage for RingStorage {
    async fn set(&self, scopes: &[&str], token: TokenInfo) -> Result<()> {
        let subject = self.verifier.verify(&token).await?;
        let mut active_subject = self.active_subject.lock().await;
        bind_subject(&mut active_subject, &subject)?;
        drop(active_subject);

        let _guard = self.keyring_lock.lock().await;
        let mut data = Self::load_data()?;
        data.upsert(storage_scopes(scopes), subject, token);
        Self::save_data(&data)
    }

    async fn get(&self, scopes: &[&str]) -> Option<TokenInfo> {
        let requested = normalize_scopes(scopes);
        let active_subject = self.active_subject.lock().await.clone();
        let entry = {
            let _guard = self.keyring_lock.lock().await;
            Self::load_data()
                .ok()?
                .find(&requested, active_subject.as_deref())?
                .clone()
        };

        let subject = if entry.token.is_expired() {
            entry.account_subject.clone()
        } else {
            let verified = self.verifier.verify(&entry.token).await.ok()?;
            if verified != entry.account_subject {
                return None;
            }
            verified
        };
        let mut active_subject = self.active_subject.lock().await;
        bind_subject(&mut active_subject, &subject).ok()?;

        Some(entry.token)
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
) -> Result<(
    oauth2::authenticator::Authenticator<
        hyper_rustls::HttpsConnector<hyper::client::HttpConnector>,
    >,
    RingStorage,
)> {
    let storage = RingStorage::new()?;
    let authenticator =
        InstalledFlowAuthenticator::builder(secret, InstalledFlowReturnMethod::HTTPRedirect)
            .with_storage(Box::new(storage.clone()))
            .flow_delegate(Box::new(delegate))
            .build()
            .await
            .context("Failed to build authenticator")?;

    Ok((authenticator, storage))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_token() -> TokenInfo {
        TokenInfo {
            access_token: Some("fake-access-token".to_string()),
            refresh_token: Some("fake-refresh-token".to_string()),
            expires_at: None,
            id_token: Some("fake-id-token".to_string()),
        }
    }

    #[test]
    fn account_legacy_gmail_token_cannot_satisfy_oidc_request() {
        let legacy = serde_json::to_string(&serde_json::json!({
            "tokens": [fake_token()]
        }))
        .unwrap();
        let data: TokenData = serde_json::from_str(&legacy).unwrap();

        assert!(
            data.find(&normalize_scopes(CANONICAL_SCOPES), None)
                .is_none()
        );
    }

    #[test]
    fn account_scope_lookup_is_normalized_and_superset_aware() {
        let subject = "subject-a".to_string();
        let mut data = TokenData::default();
        data.upsert(
            normalize_scopes(CANONICAL_SCOPES),
            subject.clone(),
            fake_token(),
        );

        let reordered = [GMAIL_READONLY_SCOPE, "openid"];
        let found = data
            .find(&normalize_scopes(&reordered), Some(&subject))
            .unwrap();

        assert_eq!(found.account_subject, subject);
        assert_eq!(
            storage_scopes(&[GMAIL_READONLY_SCOPE]),
            normalize_scopes(CANONICAL_SCOPES)
        );
        assert_eq!(
            storage_scopes(&[
                "openid",
                GMAIL_SETTINGS_SCOPE,
                GMAIL_SEND_SCOPE,
                GMAIL_READONLY_SCOPE,
                GMAIL_MODIFY_SCOPE,
                "email",
            ]),
            normalize_scopes(CANONICAL_SCOPES)
        );
    }

    #[test]
    fn account_different_subject_cannot_replace_active_grant() {
        let mut active = Some("subject-a".to_string());

        assert!(bind_subject(&mut active, "subject-b").is_err());
        assert_eq!(active.as_deref(), Some("subject-a"));
    }

    #[test]
    fn account_identity_requires_verified_address_and_valid_subject() {
        assert!(
            verified_subject(UserInfo {
                sub: Some("subject-a".to_string()),
                email: Some("person@example.test".to_string()),
                email_verified: Some(true),
            })
            .is_ok()
        );
        assert!(
            verified_subject(UserInfo {
                sub: Some("subject-a".to_string()),
                email: Some("person@example.test".to_string()),
                email_verified: Some(false),
            })
            .is_err()
        );
    }
}
