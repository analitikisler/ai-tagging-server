//! Google access tokens for the BigQuery export. The server fetches and keeps
//! them itself and adds them to BigQuery requests on the way out, so a
//! template never sees a token and can't send one anywhere else.
//!
//! Two sources: the metadata server of the Cloud Run service (or GCE VM) the
//! server runs on, or a service account key file for hosts outside Google
//! Cloud. The metadata server is reached by a client of its own, since the
//! templates' transport refuses internal addresses on purpose.

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use ring::rand::SystemRandom;
use ring::signature::{RSA_PKCS1_SHA256, RsaKeyPair};
use serde::Deserialize;
use tokio::sync::Mutex;

/// What a token is asked for: writing rows into tables, nothing else.
const SCOPE: &str = "https://www.googleapis.com/auth/bigquery.insertdata";

/// The address is used instead of metadata.google.internal so no DNS lookup
/// is needed.
const METADATA_TOKEN_URL: &str =
    "http://169.254.169.254/computeMetadata/v1/instance/service-accounts/default/token";

/// A token is replaced this long before it expires.
const REFRESH_EARLY: Duration = Duration::from_secs(300);

pub struct GoogleAuth {
    source: Source,
    client: reqwest::Client,
    /// The current token and when it expires. Held while a new one is fetched,
    /// so concurrent sends share one fetch.
    cached: Mutex<Option<(String, Instant)>>,
}

enum Source {
    Metadata,
    Key(Box<ServiceAccount>),
}

struct ServiceAccount {
    email: String,
    token_uri: String,
    key_id: String,
    key: RsaKeyPair,
}

/// The fields of a service account key file (`gcloud iam service-accounts
/// keys create`) that are used.
#[derive(Deserialize)]
struct KeyFile {
    #[serde(rename = "type")]
    kind: String,
    client_email: String,
    private_key: String,
    #[serde(default)]
    private_key_id: String,
    #[serde(default = "default_token_uri")]
    token_uri: String,
}

fn default_token_uri() -> String {
    "https://oauth2.googleapis.com/token".into()
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default = "default_expiry")]
    expires_in: u64,
}

fn default_expiry() -> u64 {
    3600
}

impl GoogleAuth {
    /// Tokens from the metadata server of the machine the server runs on.
    pub fn metadata() -> Self {
        Self::new(Source::Metadata)
    }

    /// Tokens for the service account in a key file's JSON.
    pub fn from_key_json(json: &str) -> Result<Self, String> {
        let f: KeyFile = serde_json::from_str(json).map_err(|e| format!("not a key file: {e}"))?;
        if f.kind != "service_account" {
            return Err(format!("a service_account key is needed, not {}", f.kind));
        }
        if !f.token_uri.starts_with("https://") {
            return Err("token_uri must be an https:// address".into());
        }
        let key = RsaKeyPair::from_pkcs8(&pem_der(&f.private_key)?)
            .map_err(|e| format!("private_key: {e}"))?;
        Ok(Self::new(Source::Key(Box::new(ServiceAccount {
            email: f.client_email,
            token_uri: f.token_uri,
            key_id: f.private_key_id,
            key,
        }))))
    }

    fn new(source: Source) -> Self {
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(10))
            .build()
            .expect("http client");
        GoogleAuth {
            source,
            client,
            cached: Mutex::new(None),
        }
    }

    /// Which credentials are used, for the startup log line.
    pub fn describe(&self) -> String {
        match &self.source {
            Source::Metadata => "the metadata server's service account".into(),
            Source::Key(sa) => format!("the key of {}", sa.email),
        }
    }

    /// A valid access token, fetched again shortly before the last one expires.
    pub async fn token(&self) -> Result<String, String> {
        let mut cached = self.cached.lock().await;
        if let Some((token, expires)) = cached.as_ref()
            && Instant::now() + REFRESH_EARLY < *expires
        {
            return Ok(token.clone());
        }
        let fetched = self
            .fetch()
            .await
            .map_err(|e| format!("google auth: {e}"))?;
        let expires = Instant::now() + Duration::from_secs(fetched.expires_in);
        *cached = Some((fetched.access_token.clone(), expires));
        Ok(fetched.access_token)
    }

    async fn fetch(&self) -> Result<TokenResponse, String> {
        let req = match &self.source {
            Source::Metadata => self
                .client
                .get(METADATA_TOKEN_URL)
                .header("metadata-flavor", "Google"),
            Source::Key(sa) => {
                let assertion = sa.assertion(unix_now())?;
                let form = form_urlencoded::Serializer::new(String::new())
                    .append_pair("grant_type", "urn:ietf:params:oauth:grant-type:jwt-bearer")
                    .append_pair("assertion", &assertion)
                    .finish();
                self.client
                    .post(&sa.token_uri)
                    .header("content-type", "application/x-www-form-urlencoded")
                    .body(form)
            }
        };
        let res = req.send().await.map_err(|e| e.to_string())?;
        let status = res.status();
        let body = res.text().await.unwrap_or_default();
        if !status.is_success() {
            let start: String = body.trim().chars().take(200).collect();
            return Err(format!("token request answered {status}: {start}"));
        }
        serde_json::from_str(&body).map_err(|e| format!("odd token answer: {e}"))
    }
}

impl ServiceAccount {
    /// A signed JWT that Google's token endpoint trades for an access token.
    fn assertion(&self, now: u64) -> Result<String, String> {
        let mut header = serde_json::json!({ "alg": "RS256", "typ": "JWT" });
        if !self.key_id.is_empty() {
            header["kid"] = self.key_id.clone().into();
        }
        let claims = serde_json::json!({
            "iss": self.email,
            "scope": SCOPE,
            "aud": self.token_uri,
            "iat": now,
            "exp": now + 3600,
        });
        let signed = format!(
            "{}.{}",
            URL_SAFE_NO_PAD.encode(header.to_string()),
            URL_SAFE_NO_PAD.encode(claims.to_string())
        );
        let mut sig = vec![0; self.key.public().modulus_len()];
        self.key
            .sign(
                &RSA_PKCS1_SHA256,
                &SystemRandom::new(),
                signed.as_bytes(),
                &mut sig,
            )
            .map_err(|_| "could not sign the token request")?;
        Ok(format!("{signed}.{}", URL_SAFE_NO_PAD.encode(sig)))
    }
}

/// The bytes of a PEM block such as "-----BEGIN PRIVATE KEY-----".
fn pem_der(pem: &str) -> Result<Vec<u8>, String> {
    let body: String = pem
        .lines()
        .map(str::trim)
        .filter(|l| !l.starts_with("-----"))
        .collect();
    STANDARD
        .decode(body)
        .map_err(|_| "private_key is not a PEM key".to_string())
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ring::signature::{RSA_PKCS1_2048_8192_SHA256, UnparsedPublicKey};

    // A key made for these tests only (`openssl genpkey -algorithm RSA`); it
    // belongs to no account.
    const TEST_KEY: &str = include_str!("testdata/test_service_account_key.pem");

    fn key_json() -> String {
        serde_json::json!({
            "type": "service_account",
            "client_email": "export@example-project.iam.gserviceaccount.com",
            "private_key_id": "abc123",
            "private_key": TEST_KEY,
            "token_uri": "https://oauth2.googleapis.com/token",
        })
        .to_string()
    }

    #[test]
    fn signs_a_jwt_google_accepts() {
        let auth = GoogleAuth::from_key_json(&key_json()).unwrap();
        let Source::Key(sa) = &auth.source else {
            panic!("expected a key")
        };
        let jwt = sa.assertion(1_700_000_000).unwrap();
        let parts: Vec<&str> = jwt.split('.').collect();
        assert_eq!(parts.len(), 3);
        let header: serde_json::Value =
            serde_json::from_slice(&URL_SAFE_NO_PAD.decode(parts[0]).unwrap()).unwrap();
        assert_eq!(header["alg"], "RS256");
        assert_eq!(header["kid"], "abc123");
        let claims: serde_json::Value =
            serde_json::from_slice(&URL_SAFE_NO_PAD.decode(parts[1]).unwrap()).unwrap();
        assert_eq!(
            claims["iss"],
            "export@example-project.iam.gserviceaccount.com"
        );
        assert_eq!(claims["scope"], SCOPE);
        assert_eq!(claims["aud"], "https://oauth2.googleapis.com/token");
        assert_eq!(claims["exp"], 1_700_003_600u64);

        let public = UnparsedPublicKey::new(&RSA_PKCS1_2048_8192_SHA256, sa.key.public().as_ref());
        let signed = format!("{}.{}", parts[0], parts[1]);
        let sig = URL_SAFE_NO_PAD.decode(parts[2]).unwrap();
        public.verify(signed.as_bytes(), &sig).unwrap();
    }

    #[test]
    fn rejects_files_that_are_not_service_account_keys() {
        let mut v: serde_json::Value = serde_json::from_str(&key_json()).unwrap();
        v["type"] = "authorized_user".into();
        assert!(GoogleAuth::from_key_json(&v.to_string()).is_err());
        let mut v: serde_json::Value = serde_json::from_str(&key_json()).unwrap();
        v["private_key"] =
            "-----BEGIN PRIVATE KEY-----\nnot base64!\n-----END PRIVATE KEY-----".into();
        assert!(GoogleAuth::from_key_json(&v.to_string()).is_err());
        let mut v: serde_json::Value = serde_json::from_str(&key_json()).unwrap();
        v["token_uri"] = "http://169.254.169.254/token".into();
        assert!(GoogleAuth::from_key_json(&v.to_string()).is_err());
        assert!(GoogleAuth::from_key_json("{}").is_err());
    }
}
