//! MCP OAuth authorization-code + PKCE, metadata discovery, registration and refresh.
use anyhow::{Context, Result};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{collections::HashMap, path::PathBuf, sync::OnceLock, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    sync::Mutex,
};
use url::Url;

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct OAuthConfig {
    pub client_id: Option<String>,
    pub authorization_server: Option<String>,
    pub scopes: Option<String>,
}

#[derive(Clone, Serialize, Deserialize)]
struct Token {
    access_token: String,
    refresh_token: Option<String>,
    expires_at: Option<i64>,
    token_endpoint: String,
    client_id: String,
    resource: String,
}

fn directory() -> PathBuf {
    std::env::var_os("AGENT_DATA_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(".agent-data"))
        .join("mcp-oauth")
}
fn token_path(resource: &str) -> PathBuf {
    directory().join(format!(
        "{:x}.token",
        Sha256::digest(resource.trim_end_matches('/').as_bytes())
    ))
}
fn client() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .redirect(reqwest::redirect::Policy::none())
        .build()?)
}
fn checked_url(value: &str) -> Result<Url> {
    let url = Url::parse(value)?;
    anyhow::ensure!(
        url.scheme() == "https"
            || (url.scheme() == "http"
                && matches!(url.host_str(), Some("127.0.0.1" | "localhost" | "[::1]"))),
        "OAuth endpoints require HTTPS (except loopback)"
    );
    anyhow::ensure!(
        url.username().is_empty() && url.password().is_none() && url.fragment().is_none(),
        "invalid OAuth endpoint"
    );
    Ok(url)
}

static REFRESH: OnceLock<Mutex<()>> = OnceLock::new();
pub async fn access_token(resource: &str) -> Result<Option<String>> {
    let _guard = REFRESH.get_or_init(|| Mutex::new(())).lock().await;
    let path = token_path(resource);
    let Some(bytes) = crate::credentials::read(&path)? else {
        return Ok(None);
    };
    let mut token: Token = serde_json::from_slice(&bytes)?;
    if token
        .expires_at
        .is_some_and(|expiry| expiry <= chrono::Utc::now().timestamp() + 60)
    {
        let refresh = token
            .refresh_token
            .as_ref()
            .context("MCP OAuth expired; sign in again")?;
        let response = client()?
            .post(checked_url(&token.token_endpoint)?)
            .form(&[
                ("grant_type", "refresh_token"),
                ("refresh_token", refresh.as_str()),
                ("client_id", token.client_id.as_str()),
                ("resource", token.resource.as_str()),
            ])
            .send()
            .await?
            .error_for_status()
            .context("MCP OAuth refresh rejected; sign in again")?;
        let value: Value = response.json().await?;
        update_token(&mut token, &value)?;
        crate::credentials::write(&path, &serde_json::to_vec(&token)?)?;
    }
    Ok(Some(token.access_token))
}

fn update_token(token: &mut Token, value: &Value) -> Result<()> {
    anyhow::ensure!(
        value
            .get("token_type")
            .and_then(Value::as_str)
            .is_some_and(|t| t.eq_ignore_ascii_case("bearer")),
        "unsupported OAuth token type"
    );
    token.access_token = value["access_token"]
        .as_str()
        .filter(|t| !t.is_empty())
        .context("OAuth response has no access token")?
        .to_string();
    if let Some(refresh) = value["refresh_token"].as_str() {
        token.refresh_token = Some(refresh.to_string());
    }
    token.expires_at = value["expires_in"]
        .as_i64()
        .map(|ttl| chrono::Utc::now().timestamp().saturating_add(ttl));
    Ok(())
}

static LOGINS: OnceLock<Mutex<HashMap<String, Value>>> = OnceLock::new();
fn logins() -> &'static Mutex<HashMap<String, Value>> {
    LOGINS.get_or_init(|| Mutex::new(HashMap::new()))
}
pub async fn status(state: &str) -> Value {
    logins()
        .lock()
        .await
        .get(state)
        .cloned()
        .unwrap_or_else(|| json!({"status":"unknown"}))
}
pub fn logout(resource: &str) -> Result<()> {
    match std::fs::remove_file(token_path(resource)) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}

async fn metadata(client: &reqwest::Client, resource: &Url, config: &OAuthConfig) -> Result<Value> {
    let issuer = if let Some(issuer) = &config.authorization_server {
        checked_url(issuer)?
    } else {
        let challenge = client.get(resource.clone()).send().await?;
        let header = challenge
            .headers()
            .get("www-authenticate")
            .and_then(|h| h.to_str().ok())
            .unwrap_or("");
        let metadata_url = header
            .split("resource_metadata=\"")
            .nth(1)
            .and_then(|s| s.split('"').next())
            .map(str::to_string)
            .unwrap_or_else(|| {
                format!(
                    "{}/.well-known/oauth-protected-resource{}",
                    resource.origin().ascii_serialization(),
                    resource.path().trim_end_matches('/')
                )
            });
        let protected = client.get(checked_url(&metadata_url)?).send().await?;
        if protected.status().is_success() {
            let value: Value = protected.json().await?;
            checked_url(
                value["authorization_servers"][0]
                    .as_str()
                    .context("MCP resource has no authorization server")?,
            )?
        } else {
            checked_url(&resource.origin().ascii_serialization())?
        }
    };
    let urls = [
        format!(
            "{}/.well-known/oauth-authorization-server{}",
            issuer.origin().ascii_serialization(),
            issuer.path().trim_end_matches('/')
        ),
        format!(
            "{}/.well-known/openid-configuration",
            issuer.as_str().trim_end_matches('/')
        ),
    ];
    for url in urls {
        let response = client.get(url).send().await?;
        if response.status().is_success() {
            let data: Value = response.json().await?;
            anyhow::ensure!(
                data["issuer"].as_str().is_some_and(
                    |s| s.trim_end_matches('/') == issuer.as_str().trim_end_matches('/')
                ),
                "OAuth issuer mismatch"
            );
            anyhow::ensure!(
                data["code_challenge_methods_supported"]
                    .as_array()
                    .is_some_and(|a| a.iter().any(|v| v == "S256")),
                "OAuth server must support PKCE S256"
            );
            return Ok(data);
        }
    }
    anyhow::bail!("OAuth authorization metadata unavailable")
}

pub async fn start(resource: &str, config: OAuthConfig) -> Result<Value> {
    let resource = checked_url(resource)?;
    let http = client()?;
    let metadata = metadata(&http, &resource, &config).await?;
    let mut authorization = checked_url(
        metadata["authorization_endpoint"]
            .as_str()
            .context("missing authorization endpoint")?,
    )?;
    let token_endpoint = checked_url(
        metadata["token_endpoint"]
            .as_str()
            .context("missing token endpoint")?,
    )?;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let redirect = format!(
        "http://127.0.0.1:{}/callback",
        listener.local_addr()?.port()
    );
    let client_id = if let Some(id) = config.client_id {
        id
    } else {
        let registration = checked_url(
            metadata["registration_endpoint"]
                .as_str()
                .context("OAuth server requires oauth.client_id in MCP configuration")?,
        )?;
        let value: Value = http.post(registration).json(&json!({
            "client_name":"Wonderland", "redirect_uris":[redirect],
            "grant_types":["authorization_code","refresh_token"], "response_types":["code"], "token_endpoint_auth_method":"none"
        })).send().await?.error_for_status()?.json().await?;
        value["client_id"]
            .as_str()
            .context("registration returned no client_id")?
            .to_string()
    };
    let state = uuid::Uuid::new_v4().simple().to_string();
    let verifier = format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    );
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    authorization.query_pairs_mut().extend_pairs([
        ("response_type", "code"),
        ("client_id", &client_id),
        ("redirect_uri", &redirect),
        ("state", &state),
        ("code_challenge", &challenge),
        ("code_challenge_method", "S256"),
        ("resource", resource.as_str()),
    ]);
    if let Some(scopes) = config.scopes {
        authorization
            .query_pairs_mut()
            .append_pair("scope", &scopes);
    }
    {
        let mut entries = logins().lock().await;
        let now = chrono::Utc::now().timestamp();
        entries.retain(|_, v| v["created_at"].as_i64().unwrap_or(0) + 600 > now);
        anyhow::ensure!(entries.len() < 32, "too many recent MCP logins");
        entries.insert(state.clone(), json!({"status":"pending","created_at":now}));
    }
    let task_state = state.clone();
    tokio::spawn(async move {
        let result: Result<()> = async {
            let deadline = tokio::time::Instant::now() + Duration::from_secs(300);
            loop {
                let (mut socket, _) = tokio::time::timeout_at(deadline, listener.accept()).await.context("OAuth login timed out")??;
                let mut bytes=Vec::new();
                let read=tokio::time::timeout(Duration::from_secs(5),async {
                    let mut chunk=[0u8;1024];
                    while !bytes.windows(4).any(|w|w==b"\r\n\r\n") {
                        let size=socket.read(&mut chunk).await?;
                        if size==0 || bytes.len()+size>8192{return Err(std::io::Error::other("invalid callback headers"));}
                        bytes.extend_from_slice(&chunk[..size]);
                    }
                    Ok(())
                }).await;
                if !matches!(read,Ok(Ok(()))){continue;}
                let request = String::from_utf8_lossy(&bytes);
                let path = request.lines().next().and_then(|s| s.strip_prefix("GET ")).and_then(|s| s.split(' ').next()).unwrap_or("/");
                let url = Url::parse(&format!("http://127.0.0.1{path}"))?;
                let params: HashMap<_,_> = url.query_pairs().into_owned().collect();
                if url.path() != "/callback" || params.get("state") != Some(&task_state) {
                    socket.write_all(b"HTTP/1.1 400 Bad Request\r\nConnection: close\r\nContent-Length: 0\r\n\r\n").await?;
                    continue;
                }
                if params.contains_key("error") { anyhow::bail!("OAuth authorization declined"); }
                let code = params.get("code").context("OAuth callback missing code")?;
                let value: Value = http.post(token_endpoint.clone()).form(&[
                    ("grant_type", "authorization_code"), ("code", code.as_str()), ("code_verifier", &verifier),
                    ("client_id", &client_id), ("redirect_uri", &redirect), ("resource", resource.as_str()),
                ]).send().await?.error_for_status().context("OAuth token exchange rejected")?.json().await?;
                let mut token = Token { access_token: String::new(), refresh_token: None, expires_at: None, token_endpoint: token_endpoint.to_string(), client_id, resource: resource.to_string() };
                update_token(&mut token, &value)?;
                crate::credentials::write(&token_path(resource.as_str()), &serde_json::to_vec(&token)?)?;
                let body="Signed in. Return to Wonderland now.\n";
                socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{body}",body.len()).as_bytes()).await?;
                return Ok(());
            }
        }.await;
        let mut status = match result {
            Ok(()) => json!({"status":"ok"}),
            Err(e) => json!({"status":"error", "error":e.to_string()}),
        };
        status["created_at"] = json!(chrono::Utc::now().timestamp());
        logins().lock().await.insert(task_state, status);
    });
    Ok(json!({"url":authorization.as_str(),"state":state,"status":"pending"}))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn pkce_and_endpoint_rules() {
        assert_eq!(
            URL_SAFE_NO_PAD.encode(Sha256::digest(
                b"dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"
            )),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
        assert!(checked_url("http://example.com/token").is_err());
        assert!(checked_url("https://example.com/token").is_ok());
        assert!(checked_url("http://127.0.0.1:8000/token").is_ok());
    }

    #[tokio::test]
    async fn discovery_pkce_callback_refresh_and_logout() {
        use axum::{
            extract::{Form, State},
            routing::{get, post},
            Json,
        };
        use std::sync::Arc;
        #[derive(Clone)]
        struct Server {
            base: String,
            challenge: Arc<Mutex<String>>,
        }
        async fn discovery(State(s): State<Server>) -> Json<Value> {
            Json(
                json!({"issuer":s.base,"authorization_endpoint":format!("{}/authorize",s.base),"token_endpoint":format!("{}/token",s.base),"registration_endpoint":format!("{}/register",s.base),"code_challenge_methods_supported":["S256"]}),
            )
        }
        async fn register(Json(body): Json<Value>) -> Json<Value> {
            assert!(body["redirect_uris"][0]
                .as_str()
                .unwrap()
                .starts_with("http://127.0.0.1:"));
            Json(json!({"client_id":"test-client"}))
        }
        async fn token(
            State(s): State<Server>,
            Form(body): Form<HashMap<String, String>>,
        ) -> Json<Value> {
            assert_eq!(body["client_id"], "test-client");
            if body["grant_type"] == "authorization_code" {
                assert_eq!(body["code"], "test-code");
                assert_eq!(
                    URL_SAFE_NO_PAD.encode(Sha256::digest(body["code_verifier"].as_bytes())),
                    *s.challenge.lock().await
                );
                Json(
                    json!({"access_token":"first","refresh_token":"refresh","token_type":"Bearer","expires_in":1}),
                )
            } else {
                assert_eq!(body["refresh_token"], "refresh");
                Json(json!({"access_token":"refreshed","token_type":"Bearer","expires_in":3600}))
            }
        }
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let resource = format!("{base}/mcp");
        let challenge = Arc::new(Mutex::new(String::new()));
        let app = axum::Router::new()
            .route("/.well-known/oauth-authorization-server", get(discovery))
            .route("/register", post(register))
            .route("/token", post(token))
            .with_state(Server {
                base: base.clone(),
                challenge: challenge.clone(),
            });
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let started = start(
            &resource,
            OAuthConfig {
                authorization_server: Some(base),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let auth = Url::parse(started["url"].as_str().unwrap()).unwrap();
        let params: HashMap<_, _> = auth.query_pairs().into_owned().collect();
        *challenge.lock().await = params["code_challenge"].clone();
        let mut callback = Url::parse(&params["redirect_uri"]).unwrap();
        callback
            .query_pairs_mut()
            .append_pair("code", "test-code")
            .append_pair("state", "wrong");
        assert_eq!(
            client()
                .unwrap()
                .get(callback.clone())
                .send()
                .await
                .unwrap()
                .status(),
            400
        );
        callback.set_query(None);
        callback
            .query_pairs_mut()
            .append_pair("code", "test-code")
            .append_pair("state", &params["state"]);
        assert!(client()
            .unwrap()
            .get(callback)
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap()
            .contains("Signed in"));
        assert_eq!(
            access_token(&resource).await.unwrap().as_deref(),
            Some("refreshed")
        );
        assert_eq!(status(&params["state"]).await["status"], "ok");
        logout(&resource).unwrap();
        assert!(access_token(&resource).await.unwrap().is_none());
        server.abort();
    }
}
