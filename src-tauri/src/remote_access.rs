//! Identity enforcement for the browser-facing HTTPS origin served through Cloudflare Tunnel.
use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode, decode_header, jwk::{AlgorithmParameters, JwkSet, KeyAlgorithm}};
use reqwest::{Client, Url, redirect};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    sync::{Arc, RwLock},
    time::{Duration, Instant},
};
use tokio::sync::Notify;

pub const ORIGIN_PORT: u16 = 14347;
const REFRESH_INTERVAL: Duration = Duration::from_secs(5 * 60);
const MIN_REFRESH_INTERVAL: Duration = Duration::from_secs(30);
const MAX_KEY_AGE: Duration = Duration::from_secs(60 * 60);
const MAX_CERT_BYTES: usize = 64 * 1024;

#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AccessSettings {
    pub public_url: String,
    pub team_name: String,
    pub audience: String,
    pub email: String,
}

struct Policy {
    settings: AccessSettings,
    public: Url,
    issuer: String,
    validation: Validation,
}

fn dns_label(label: &str) -> bool {
    !label.is_empty() && label.len() <= 63 && !label.starts_with('-') && !label.ends_with('-')
        && label.bytes().all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
}

impl Policy {
    fn from_settings(settings: AccessSettings) -> Option<Self> {
        let public = Url::parse(settings.public_url.trim()).ok()?;
        let host = public.domain()?;
        if public.scheme() != "https" || !host.contains('.') || host.len() > 253
            || !host.split('.').all(dns_label) || host.ends_with(".local") || host.ends_with(".localhost")
            || public.port().is_some() || public.path() != "/" || public.query().is_some()
            || public.fragment().is_some() || !public.username().is_empty() || public.password().is_some() {
            return None;
        }
        let team_name = settings.team_name.trim().to_ascii_lowercase();
        let audience = settings.audience.trim().to_owned();
        let email = settings.email.trim().to_ascii_lowercase();
        let (mailbox, domain) = email.split_once('@')?;
        if !dns_label(&team_name) || audience.len() != 64 || !audience.bytes().all(|byte| byte.is_ascii_hexdigit())
            || mailbox.is_empty() || domain.is_empty() || domain.contains('@') || email.len() > 254
            || email.chars().any(|ch| ch.is_whitespace() || ch.is_control()) {
            return None;
        }
        let issuer = format!("https://{team_name}.cloudflareaccess.com");
        let mut validation = Validation::new(Algorithm::RS256);
        validation.leeway = 0;
        validation.validate_nbf = true;
        validation.set_required_spec_claims(&["exp", "iss", "aud", "sub"]);
        validation.set_audience(&[&audience]);
        validation.set_issuer(&[&issuer]);
        let settings = AccessSettings { public_url: public.origin().ascii_serialization(), team_name, audience, email };
        Some(Self { settings, public, issuer, validation })
    }
}

struct CachedKeys {
    by_id: HashMap<String, DecodingKey>,
    refreshed_at: Instant,
}

impl CachedKeys {
    fn new(set: JwkSet) -> Option<Self> {
        let by_id: HashMap<_, _> = set.keys.into_iter().filter_map(|jwk| {
            if jwk.common.key_algorithm.is_some_and(|alg| alg != KeyAlgorithm::RS256) { return None; }
            let id = jwk.common.key_id?;
            let AlgorithmParameters::RSA(rsa) = jwk.algorithm else { return None; };
            Some((id, DecodingKey::from_rsa_components(&rsa.n, &rsa.e).ok()?))
        }).collect();
        (!by_id.is_empty()).then(|| Self { by_id, refreshed_at: Instant::now() })
    }
}

#[derive(Deserialize)]
struct Identity {
    email: String,
    exp: i64,
}

pub struct AccessVerifier {
    policy: Policy,
    keys: RwLock<CachedKeys>,
    refresh: Arc<Notify>,
}

impl AccessVerifier {
    pub async fn prepare(settings: AccessSettings) -> Result<Arc<Self>, &'static str> {
        let policy = Policy::from_settings(settings).ok_or("invalid_access_config")?;
        let url = format!("{}/cdn-cgi/access/certs", policy.issuer);
        let keys = Self::fetch_keys(&url).await.ok_or("access_keys_unavailable")?;
        let verifier = Arc::new(Self { policy, keys: RwLock::new(keys), refresh: Arc::new(Notify::new()) });
        verifier.refresh_keys(url);
        Ok(verifier)
    }

    pub fn settings(&self) -> &AccessSettings { &self.policy.settings }

    pub fn matches_host(&self, host: &str) -> bool {
        self.policy.public.host_str().is_some_and(|configured| {
            host.strip_suffix(":443").unwrap_or(host).eq_ignore_ascii_case(configured)
        })
    }

    pub fn is_same_origin(&self, url: &Url) -> bool { url.origin() == self.policy.public.origin() }

    /// Returns the assertion's expiry in epoch seconds. The caller also enforces it for open SSE streams.
    pub fn authenticate(&self, assertion: &str) -> Option<i64> {
        if assertion.len() > 8 * 1024 { return None; }
        let header = decode_header(assertion).ok()?;
        if header.alg != Algorithm::RS256 { return None; }
        let kid = header.kid?;
        let keys = self.keys.read().unwrap();
        if keys.refreshed_at.elapsed() >= MAX_KEY_AGE {
            self.refresh.notify_one();
            return None;
        }
        let Some(key) = keys.by_id.get(&kid) else {
            self.refresh.notify_one();
            return None;
        };
        let result = decode::<Identity>(assertion, key, &self.policy.validation).ok()?.claims;
        (result.email.eq_ignore_ascii_case(&self.policy.settings.email) && result.exp > chrono::Utc::now().timestamp())
            .then_some(result.exp)
    }

    async fn fetch_keys(url: &str) -> Option<CachedKeys> {
        let mut builder = Client::builder()
            .redirect(redirect::Policy::none())
            .timeout(Duration::from_secs(8));
        let proxy = crate::net::proxy();
        if !proxy.is_empty() {
            builder = builder.proxy(reqwest::Proxy::all(proxy).ok()?);
        }
        let mut response = builder.build().ok()?.get(url).send().await.ok()?;
        if !response.status().is_success() { return None; }
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.ok()? {
            if bytes.len() + chunk.len() > MAX_CERT_BYTES { return None; }
            bytes.extend_from_slice(&chunk);
        }
        CachedKeys::new(serde_json::from_slice(&bytes).ok()?)
    }

    fn refresh_keys(self: &Arc<Self>, url: String) {
        let verifier = Arc::downgrade(self);
        let refresh = self.refresh.clone();
        tauri::async_runtime::spawn(async move {
            let mut last = Instant::now();
            loop {
                tokio::select! {
                    _ = tokio::time::sleep(REFRESH_INTERVAL) => {},
                    _ = refresh.notified() => {},
                }
                tokio::time::sleep(MIN_REFRESH_INTERVAL.saturating_sub(last.elapsed())).await;
                let Some(verifier) = verifier.upgrade() else { break; };
                last = Instant::now();
                if let Some(keys) = Self::fetch_keys(&url).await {
                    *verifier.keys.write().unwrap() = keys;
                }
            }
        });
    }

    #[cfg(test)]
    pub fn with_test_keys(settings: AccessSettings, jwks: serde_json::Value) -> Arc<Self> {
        let policy = Policy::from_settings(settings).unwrap();
        let keys = CachedKeys::new(serde_json::from_value(jwks).unwrap()).unwrap();
        Arc::new(Self { policy, keys: RwLock::new(keys), refresh: Arc::new(Notify::new()) })
    }
}


#[cfg(test)]
pub mod tests {
    use super::*;
    use serde_json::Value;

    pub fn fixture() -> Value {
        serde_json::from_str(include_str!("../testdata/phone-access.json")).unwrap()
    }

    pub fn settings() -> AccessSettings {
        AccessSettings {
            public_url: "https://subcast.example.com".into(), team_name: "subcast-test".into(),
            audience: "a".repeat(64), email: "tester@example.net".into(),
        }
    }

    #[test]
    fn accepts_only_signed_access_for_the_configured_app_and_identity() {
        let data = fixture();
        let token = data["valid"].as_str().unwrap();
        let access = AccessVerifier::with_test_keys(settings(), data["jwks"].clone());
        assert!(access.authenticate(token).is_some());
        assert!(access.authenticate(data["expired"].as_str().unwrap()).is_none());
        assert!(access.authenticate(data["future"].as_str().unwrap()).is_none());
        assert!(access.authenticate(&format!("{token}broken")).is_none());

        let mut wrong_app = settings();
        wrong_app.audience = "b".repeat(64);
        let mut wrong_team = settings();
        wrong_team.team_name = "a-different-team".into();
        let mut wrong_owner = settings();
        wrong_owner.email = "someone-else@example.net".into();
        for setting in [wrong_app, wrong_team, wrong_owner] {
            let access = AccessVerifier::with_test_keys(setting, data["jwks"].clone());
            assert!(access.authenticate(token).is_none());
        }
    }

    #[test]
    fn accepts_only_a_bare_https_hostname_and_a_team_label() {
        for public_url in [
            "http://subcast.example.com", "https://127.0.0.1", "https://user@subcast.example.com",
            "https://subcast.example.com:9999", "https://subcast.example.com/episode", "https://subcast.example.com/?code=1",
        ] {
            let mut config = settings();
            config.public_url = public_url.into();
            assert!(Policy::from_settings(config).is_none());
        }
        let mut config = settings();
        config.team_name = "subcast-test.cloudflareaccess.com/".into();
        assert!(Policy::from_settings(config).is_none());
    }
}
