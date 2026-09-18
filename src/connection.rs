//! Desktop/CLI connection settings, shared by all providers and persisted privately.
use crate::provider::{ModelProvider, ModelRequest, ModelResponse, StreamSink};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    path::Path,
    sync::{Arc, RwLock},
};

pub fn service_client() -> reqwest::Client {
    let mut headers = reqwest::header::HeaderMap::new();
    if let Ok(token) = std::env::var("WONDERLAND_SERVER_TOKEN") {
        if let Ok(value) = reqwest::header::HeaderValue::from_str(&format!("Bearer {token}")) {
            headers.insert(reqwest::header::AUTHORIZATION, value);
        }
    }
    reqwest::Client::builder()
        .default_headers(headers)
        .connect_timeout(std::time::Duration::from_secs(10))
        .build()
        .expect("HTTP client")
}

/// Bound connection and stalled reads while allowing long reasoning responses.
pub fn model_client() -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(15))
        .read_timeout(std::time::Duration::from_secs(180))
        .timeout(std::time::Duration::from_secs(1800))
        .build()
        .expect("model HTTP client")
}

#[derive(Clone, Default, Serialize, Deserialize)]
pub struct ConnectionSettings {
    pub provider: String,
    #[serde(default)]
    pub base_url: String,
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub api_key: String,
    #[serde(default)]
    pub wire: Option<crate::model_profile::WireProtocol>,
}
impl ConnectionSettings {
    pub fn public(&self) -> serde_json::Value {
        serde_json::json!({"provider":self.provider,"base_url":self.base_url,"model":self.model,"wire":self.wire,"has_api_key":!self.api_key.is_empty()})
    }
    pub fn load(data_dir: &Path) -> Result<Option<Self>> {
        crate::credentials::read(&data_dir.join("connection.bin"))?
            .map(|bytes| serde_json::from_slice(&bytes).map_err(Into::into))
            .transpose()
    }
    pub fn save(&self, data_dir: &Path) -> Result<()> {
        crate::credentials::write(&data_dir.join("connection.bin"), &serde_json::to_vec(self)?)
    }
    pub async fn build(&self) -> Result<Arc<dyn ModelProvider>> {
        if self.provider == "offline" {
            return Ok(Arc::new(crate::provider::RuleBasedModel));
        }
        if matches!(self.provider.as_str(), "subscription" | "cliproxyapi") {
            return crate::provider::ensure_subscription_with_options(
                (!self.model.trim().is_empty()).then_some(self.model.trim()),
                self.wire,
            )
            .await;
        }
        let preset = crate::vendors::vendor_preset(&self.provider);
        let base = if self.base_url.trim().is_empty() {
            match self.provider.as_str() {
                "openai" => "https://api.openai.com/v1",
                "anthropic" | "claude" => "https://api.anthropic.com/v1",
                _ => preset.context("unknown provider")?.default_base_url,
            }
        } else {
            self.base_url.trim()
        };
        let url = url::Url::parse(base)?;
        anyhow::ensure!(
            url.scheme() == "https"
                || (url.scheme() == "http"
                    && matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "[::1]"))),
            "API endpoint requires HTTPS except localhost"
        );
        anyhow::ensure!(
            url.password().is_none()
                && url.username().is_empty()
                && url.query().is_none()
                && url.fragment().is_none(),
            "invalid API base URL"
        );
        let model = if self.model.trim().is_empty() {
            preset
                .map(|p| p.default_model)
                .unwrap_or(if self.provider == "openai" {
                    "gpt-5"
                } else {
                    "claude-sonnet-4-5"
                })
        } else {
            self.model.trim()
        };
        anyhow::ensure!(
            !self.api_key.trim().is_empty() || self.provider == "ollama",
            "API key is required"
        );
        let chat = Arc::new(
            crate::provider::OpenAiCompatibleModel::new_with_optional_key(
                base,
                Some(self.api_key.clone()),
                model,
                preset.map(|p| p.names[0]).unwrap_or("openai"),
            ),
        );
        if matches!(self.provider.as_str(), "anthropic" | "claude") {
            return Ok(Arc::new(crate::anthropic::AnthropicModel::new(
                base,
                &self.api_key,
                model,
            )));
        }
        if self.provider == "openai" || self.wire.is_some() {
            let router = crate::router::ProtocolRouter::new(chat)
                .with_responses(Arc::new(crate::responses::ResponsesModel::new(
                    base,
                    Some(self.api_key.clone()),
                    model,
                    "openai-responses",
                )))
                .with_anthropic(Arc::new(crate::anthropic::AnthropicModel::new(
                    base,
                    &self.api_key,
                    model,
                )))
                .with_forced(self.wire);
            return Ok(Arc::new(router));
        }
        Ok(chat)
    }
}

pub struct LiveProvider(RwLock<Arc<dyn ModelProvider>>);
impl LiveProvider {
    pub fn new(provider: Arc<dyn ModelProvider>) -> Self {
        Self(RwLock::new(provider))
    }
    pub fn snapshot(&self) -> Arc<dyn ModelProvider> {
        self.0.read().unwrap_or_else(|e| e.into_inner()).clone()
    }
    pub fn replace(&self, provider: Arc<dyn ModelProvider>) {
        *self.0.write().unwrap_or_else(|e| e.into_inner()) = provider;
    }
}
#[async_trait::async_trait]
impl ModelProvider for LiveProvider {
    fn name(&self) -> &'static str {
        self.snapshot().name()
    }
    fn default_model(&self) -> Option<String> {
        self.snapshot().default_model()
    }
    fn available_protocols(&self) -> Vec<&'static str> {
        self.snapshot().available_protocols()
    }
    async fn list_models(&self) -> Result<Vec<crate::cliproxy::CliProxyModel>> {
        self.snapshot().list_models().await
    }
    async fn complete(&self, request: &ModelRequest) -> Result<ModelResponse> {
        self.snapshot().complete(request).await
    }
    async fn complete_stream(
        &self,
        request: &ModelRequest,
        sink: Option<&StreamSink>,
    ) -> Result<ModelResponse> {
        self.snapshot().complete_stream(request, sink).await
    }
}
