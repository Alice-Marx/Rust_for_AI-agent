//! Client for the local CLIProxyAPI sidecar.
//!
//! CLIProxyAPI owns OAuth and subscription credentials. This module only
//! forwards management requests and uses its OpenAI-compatible `/v1` API.

use anyhow::{bail, Context, Result};
use reqwest::{Client, RequestBuilder};
use serde::{Deserialize, Serialize};
use std::env;

const DEFAULT_BASE_URL: &str = "http://127.0.0.1:8317/v1";
const DEFAULT_MANAGEMENT_SUFFIX: &str = "/v0/management";

#[derive(Clone)]
pub struct CliProxyApiClient {
    client: Client,
    base_url: String,
    management_url: String,
    api_key: Option<String>,
    management_key: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CliProxyModel {
    pub id: String,
    #[serde(default)]
    pub object: String,
    #[serde(default)]
    pub owned_by: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CliProxyVerification {
    pub reachable: bool,
    pub model_count: usize,
    pub models: Vec<CliProxyModel>,
    pub selected_model: Option<String>,
    pub selected_model_available: Option<bool>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CliProxyLoginStart {
    pub status: String,
    pub url: Option<String>,
    pub state: Option<String>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CliProxyLoginStatus {
    pub state: String,
    pub status: String,
    pub authenticated: bool,
    pub error: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ModelsResponse {
    #[serde(default)]
    data: Vec<CliProxyModel>,
}

#[derive(Debug, Deserialize)]
struct LoginStatusResponse {
    #[serde(default = "default_status")]
    status: String,
    error: Option<String>,
}

fn default_status() -> String {
    "wait".to_string()
}

impl CliProxyApiClient {
    pub fn new(
        base_url: impl Into<String>,
        api_key: Option<String>,
        management_url: impl Into<String>,
        management_key: Option<String>,
    ) -> Self {
        Self {
            client: Client::new(),
            base_url: normalize_url(base_url.into()),
            management_url: normalize_url(management_url.into()),
            api_key: clean_optional(api_key),
            management_key: clean_optional(management_key),
        }
    }

    /// Build a client when CLIProxyAPI-related environment variables are set.
    pub fn from_env() -> Option<Self> {
        let provider = env::var("AGENT_PROVIDER")
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase();
        let enabled = provider == "cliproxyapi"
            || env::var("CLIPROXYAPI_ENABLED")
                .ok()
                .map(|value| is_truthy(&value))
                .unwrap_or(false)
            || env::var("CLIPROXYAPI_BASE_URL").is_ok()
            || env::var("CLIPROXYAPI_MANAGEMENT_KEY").is_ok();
        if !enabled {
            return None;
        }

        let base_url =
            env::var("CLIPROXYAPI_BASE_URL").unwrap_or_else(|_| DEFAULT_BASE_URL.to_string());
        let management_url = env::var("CLIPROXYAPI_MANAGEMENT_URL")
            .unwrap_or_else(|_| management_url_from_base(&base_url));
        Some(Self::new(
            base_url,
            env::var("CLIPROXYAPI_API_KEY").ok(),
            management_url,
            env::var("CLIPROXYAPI_MANAGEMENT_KEY").ok(),
        ))
    }

    pub async fn list_models(&self) -> Result<Vec<CliProxyModel>> {
        let response = self
            .with_api_key(self.client.get(format!("{}/models", self.base_url)))
            .send()
            .await
            .context("CLIProxyAPI models request failed")?
            .error_for_status()
            .context("CLIProxyAPI models endpoint returned an error")?
            .json::<ModelsResponse>()
            .await
            .context("invalid CLIProxyAPI models response")?;
        Ok(response.data)
    }

    pub async fn verify(&self, selected_model: Option<String>) -> Result<CliProxyVerification> {
        let models = self.list_models().await?;
        let selected_model_available = selected_model.as_ref().map(|selected| {
            models
                .iter()
                .any(|model| model.id.eq_ignore_ascii_case(selected))
        });
        Ok(CliProxyVerification {
            reachable: true,
            model_count: models.len(),
            models,
            selected_model,
            selected_model_available,
        })
    }

    pub async fn start_login(&self, provider: &str) -> Result<CliProxyLoginStart> {
        self.ensure_management_key()?;
        let path = login_path(provider)?;
        self.management_get(path)
            .send()
            .await
            .context("CLIProxyAPI OAuth start request failed")?
            .error_for_status()
            .context("CLIProxyAPI OAuth start endpoint returned an error")?
            .json::<CliProxyLoginStart>()
            .await
            .context("invalid CLIProxyAPI OAuth start response")
    }

    pub async fn login_status(&self, state: &str) -> Result<CliProxyLoginStatus> {
        self.ensure_management_key()?;
        let state = require_non_empty(state, "OAuth state")?;
        let response = self
            .management_get("get-auth-status")
            .query(&[("state", state)])
            .send()
            .await
            .context("CLIProxyAPI OAuth status request failed")?
            .error_for_status()
            .context("CLIProxyAPI OAuth status endpoint returned an error")?
            .json::<LoginStatusResponse>()
            .await
            .context("invalid CLIProxyAPI OAuth status response")?;
        Ok(CliProxyLoginStatus {
            state: state.to_string(),
            authenticated: response.status == "ok",
            status: response.status,
            error: response.error,
        })
    }

    pub async fn cancel_login(&self, state: &str) -> Result<bool> {
        self.ensure_management_key()?;
        let state = require_non_empty(state, "OAuth state")?;
        #[derive(Deserialize)]
        struct CancelResponse {
            #[serde(default)]
            cancelled: bool,
        }

        let response = self
            .management_request(
                self.client
                    .delete(format!("{}/oauth-session", self.management_url)),
            )
            .query(&[("state", state)])
            .send()
            .await
            .context("CLIProxyAPI OAuth cancel request failed")?
            .error_for_status()
            .context("CLIProxyAPI OAuth cancel endpoint returned an error")?
            .json::<CancelResponse>()
            .await
            .context("invalid CLIProxyAPI OAuth cancel response")?;
        Ok(response.cancelled)
    }

    fn management_get(&self, path: &str) -> RequestBuilder {
        self.management_request(self.client.get(format!("{}/{}", self.management_url, path)))
    }

    fn management_request(&self, request: RequestBuilder) -> RequestBuilder {
        match &self.management_key {
            Some(key) => request.bearer_auth(key),
            None => request,
        }
    }

    fn ensure_management_key(&self) -> Result<()> {
        if self.management_key.is_none() {
            bail!("CLIPROXYAPI_MANAGEMENT_KEY is required for OAuth management operations");
        }
        Ok(())
    }

    fn with_api_key(&self, request: RequestBuilder) -> RequestBuilder {
        match &self.api_key {
            Some(key) => request.bearer_auth(key),
            None => request,
        }
    }
}

fn login_path(provider: &str) -> Result<&'static str> {
    match provider.trim().to_ascii_lowercase().as_str() {
        "anthropic" | "claude" => Ok("anthropic-auth-url"),
        "codex" => Ok("codex-auth-url"),
        "antigravity" => Ok("antigravity-auth-url"),
        "kimi" => Ok("kimi-auth-url"),
        "xai" | "grok" => Ok("xai-auth-url"),
        "devin" => Ok("devin-auth-url"),
        "meta" => Ok("meta-auth-url"),
        _ => bail!(
            "unsupported CLIProxyAPI login provider; use codex, claude, antigravity, kimi, xai, devin, or meta"
        ),
    }
}

fn normalize_url(url: String) -> String {
    url.trim_end_matches('/').to_string()
}

fn management_url_from_base(base_url: &str) -> String {
    let base = normalize_url(base_url.to_string());
    base.strip_suffix("/v1")
        .map(|root| format!("{root}{DEFAULT_MANAGEMENT_SUFFIX}"))
        .unwrap_or_else(|| format!("{base}{DEFAULT_MANAGEMENT_SUFFIX}"))
}

fn clean_optional(value: Option<String>) -> Option<String> {
    value.filter(|item| !item.trim().is_empty())
}

fn require_non_empty<'a>(value: &'a str, label: &str) -> Result<&'a str> {
    let value = value.trim();
    if value.is_empty() {
        bail!("{label} must not be empty");
    }
    Ok(value)
}

fn is_truthy(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derives_management_url_from_v1_base() {
        assert_eq!(
            management_url_from_base("http://127.0.0.1:8317/v1/"),
            "http://127.0.0.1:8317/v0/management"
        );
    }

    #[test]
    fn accepts_supported_login_providers() {
        assert_eq!(login_path("claude").unwrap(), "anthropic-auth-url");
        assert!(login_path("unknown").is_err());
    }
}
