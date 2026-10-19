//! The local Ollama transport boundary.
//!
//! This module knows the HTTP and JSON details of Ollama's `/api/chat`
//! endpoint. It deliberately does not print, read terminal input, or update
//! application history; the controller added later owns those decisions.

use std::{env, time::Duration};

use base64::{Engine, engine::general_purpose::STANDARD};
use reqwest::{Client, StatusCode, Url, redirect::Policy};
use serde::{Deserialize, Serialize};
use thiserror::Error;

const DEFAULT_OLLAMA_URL: &str = "http://127.0.0.1:11434";
// The Instruct tag is the non-thinking Qwen3-VL variant. It remains a vision
// model, but avoids spending a large part of a short interaction on a hidden
// reasoning pass. Callers can still choose a different local model through
// `OLLAMA_MODEL`.
const DEFAULT_OLLAMA_MODEL: &str = "qwen3-vl:4b-instruct";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);

/// The editable behavioral guidance sent as Ollama's system message.
pub const TEACHER_INSTRUCTIONS: &str = include_str!("teacher.txt");

/// One successfully accepted user/assistant exchange.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConversationTurn {
    pub user: String,
    pub assistant: String,
}

/// Configuration for a loopback-only Ollama endpoint.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OllamaConfig {
    endpoint: Url,
    model: String,
}

impl OllamaConfig {
    /// Uses `OLLAMA_URL` and `OLLAMA_MODEL`, or the local defaults.
    pub fn from_environment() -> Result<Self, ThinkError> {
        let endpoint = env::var("OLLAMA_URL").unwrap_or_else(|_| DEFAULT_OLLAMA_URL.to_owned());
        let model = env::var("OLLAMA_MODEL").unwrap_or_else(|_| DEFAULT_OLLAMA_MODEL.to_owned());

        Self::new(&endpoint, model)
    }

    /// Builds a configuration for tests or an explicitly configured local server.
    pub fn new(endpoint: &str, model: impl Into<String>) -> Result<Self, ThinkError> {
        let endpoint =
            Url::parse(endpoint).map_err(|error| ThinkError::InvalidEndpoint(error.to_string()))?;
        let model = model.into();

        if endpoint.scheme() != "http" {
            return Err(ThinkError::UnsupportedScheme(endpoint.scheme().to_owned()));
        }

        if endpoint.host_str() != Some("127.0.0.1") && endpoint.host_str() != Some("::1") {
            return Err(ThinkError::NonLoopbackEndpoint(endpoint));
        }

        if endpoint.path() != "/" && !endpoint.path().is_empty() {
            return Err(ThinkError::EndpointHasPath(endpoint));
        }

        if model.trim().is_empty() {
            return Err(ThinkError::EmptyModel);
        }

        Ok(Self { endpoint, model })
    }

    fn chat_url(&self) -> Result<Url, ThinkError> {
        self.endpoint
            .join("api/chat")
            .map_err(|error| ThinkError::InvalidEndpoint(error.to_string()))
    }
}

/// A reusable HTTP client for the local Ollama process.
#[derive(Clone)]
pub struct OllamaClient {
    http: Client,
    config: OllamaConfig,
}

impl OllamaClient {
    pub fn from_environment() -> Result<Self, ThinkError> {
        Self::new(OllamaConfig::from_environment()?)
    }

    pub fn new(config: OllamaConfig) -> Result<Self, ThinkError> {
        let http = Client::builder()
            // Local inference should not inherit HTTP(S)_PROXY settings.
            .no_proxy()
            // A local endpoint should never redirect a private request elsewhere.
            .redirect(Policy::none())
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(REQUEST_TIMEOUT)
            .build()
            .map_err(ThinkError::BuildClient)?;

        Ok(Self { http, config })
    }

    /// Requests one complete, non-streaming answer from Ollama.
    pub async fn answer(
        &self,
        history: &[ConversationTurn],
        question: &str,
        images: &[&[u8]],
    ) -> Result<String, ThinkError> {
        let request = ChatRequest::new(&self.config.model, history, question, images);
        let endpoint = self.config.chat_url()?;

        let response = self
            .http
            .post(endpoint.clone())
            .json(&request)
            .send()
            .await
            .map_err(|source| ThinkError::Request { endpoint, source })?;

        if !response.status().is_success() {
            return Err(http_error(response.status(), response).await);
        }

        let response = response
            .json::<ChatResponse>()
            .await
            .map_err(ThinkError::InvalidResponse)?;

        if response.done != Some(true) || response.done_reason.as_deref() == Some("length") {
            return Err(ThinkError::IncompleteResponse);
        }

        let answer = response
            .message
            .map(|message| message.content)
            .unwrap_or_default();

        if answer.trim().is_empty() {
            return Err(ThinkError::EmptyAnswer);
        }

        Ok(answer)
    }
}

async fn http_error(status: StatusCode, response: reqwest::Response) -> ThinkError {
    let detail = response
        .json::<OllamaErrorResponse>()
        .await
        .ok()
        .and_then(|body| body.error)
        .map(|message| format!(": {}", truncate_for_diagnostic(&message)))
        .unwrap_or_default();

    ThinkError::Http { status, detail }
}

fn truncate_for_diagnostic(message: &str) -> String {
    const LIMIT: usize = 240;

    let mut truncated = message.chars().take(LIMIT).collect::<String>();
    if message.chars().nth(LIMIT).is_some() {
        truncated.push('…');
    }
    truncated
}

#[derive(Serialize)]
struct ChatRequest {
    model: String,
    messages: Vec<ChatMessage>,
    stream: bool,
}

impl ChatRequest {
    fn new(model: &str, history: &[ConversationTurn], question: &str, images: &[&[u8]]) -> Self {
        let mut messages = Vec::with_capacity(2 + history.len() * 2);
        messages.push(ChatMessage::system(TEACHER_INSTRUCTIONS));

        for turn in history {
            messages.push(ChatMessage::user(&turn.user, &[]));
            messages.push(ChatMessage::assistant(&turn.assistant));
        }

        messages.push(ChatMessage::user(question, images));

        Self {
            model: model.to_owned(),
            messages,
            stream: false,
        }
    }
}

#[derive(Serialize)]
struct ChatMessage {
    role: &'static str,
    content: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    images: Vec<String>,
}

impl ChatMessage {
    fn system(content: &str) -> Self {
        Self {
            role: "system",
            content: content.to_owned(),
            images: Vec::new(),
        }
    }

    fn user(content: &str, images: &[&[u8]]) -> Self {
        Self {
            role: "user",
            content: content.to_owned(),
            images: images.iter().map(|bytes| STANDARD.encode(bytes)).collect(),
        }
    }

    fn assistant(content: &str) -> Self {
        Self {
            role: "assistant",
            content: content.to_owned(),
            images: Vec::new(),
        }
    }
}

#[derive(Deserialize)]
struct ChatResponse {
    message: Option<ResponseMessage>,
    done: Option<bool>,
    done_reason: Option<String>,
}

#[derive(Deserialize)]
struct ResponseMessage {
    content: String,
}

#[derive(Deserialize)]
struct OllamaErrorResponse {
    error: Option<String>,
}

#[derive(Debug, Error)]
pub enum ThinkError {
    #[error("OLLAMA_URL is not a valid URL: {0}")]
    InvalidEndpoint(String),

    #[error("OLLAMA_URL must use http for this local prototype, not {0}")]
    UnsupportedScheme(String),

    #[error("OLLAMA_URL must use a loopback host (127.0.0.1 or ::1), not {0}")]
    NonLoopbackEndpoint(Url),

    #[error("OLLAMA_URL must be a server root without a path: {0}")]
    EndpointHasPath(Url),

    #[error("OLLAMA_MODEL must not be empty")]
    EmptyModel,

    #[error("could not build the local Ollama HTTP client: {0}")]
    BuildClient(#[source] reqwest::Error),

    #[error("could not reach Ollama at {endpoint}: {source}")]
    Request {
        endpoint: Url,
        #[source]
        source: reqwest::Error,
    },

    #[error("Ollama returned HTTP {status}{detail}")]
    Http { status: StatusCode, detail: String },

    #[error("Ollama returned invalid JSON: {0}")]
    InvalidResponse(#[source] reqwest::Error),

    #[error("Ollama returned an incomplete or length-truncated answer; no answer was accepted")]
    IncompleteResponse,

    #[error("Ollama returned an empty answer; no answer was accepted")]
    EmptyAnswer,
}

#[cfg(test)]
mod tests {
    use std::{future::Future, time::Duration};

    use serde_json::json;
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{body_json, method, path},
    };

    use super::{ConversationTurn, OllamaClient, OllamaConfig, TEACHER_INSTRUCTIONS};

    fn client(server: &MockServer) -> OllamaClient {
        let config = OllamaConfig::new(&server.uri(), "test-model").unwrap();
        OllamaClient::new(config).unwrap()
    }

    async fn within_test_timeout<T>(future: impl Future<Output = T>) -> T {
        tokio::time::timeout(Duration::from_secs(3), future)
            .await
            .expect("mock HTTP test exceeded its three-second timeout")
    }

    #[tokio::test]
    async fn sends_system_history_current_question_and_non_streaming_request() {
        let server = MockServer::start().await;
        let expected = json!({
            "model": "test-model",
            "stream": false,
            "messages": [
                {"role": "system", "content": TEACHER_INSTRUCTIONS},
                {"role": "user", "content": "What is bark made of?"},
                {"role": "assistant", "content": "Bark is the outer protective tissue."},
                {"role": "user", "content": "How does it protect a tree?"}
            ]
        });

        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .and(body_json(expected))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "message": {"role": "assistant", "content": "It helps limit water loss and damage."},
                "done": true,
                "done_reason": "stop"
            })))
            .mount(&server)
            .await;

        let answer = within_test_timeout(client(&server).answer(
            &[ConversationTurn {
                user: "What is bark made of?".into(),
                assistant: "Bark is the outer protective tissue.".into(),
            }],
            "How does it protect a tree?",
            &[],
        ))
        .await
        .unwrap();

        assert_eq!(answer, "It helps limit water loss and damage.");
    }

    #[tokio::test]
    async fn http_failures_remain_errors() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(503).set_body_json(json!({"error": "model is unavailable"})),
            )
            .mount(&server)
            .await;

        let error = within_test_timeout(client(&server).answer(&[], "hello", &[]))
            .await
            .unwrap_err();

        assert!(error.to_string().contains("HTTP 503"));
        assert!(error.to_string().contains("model is unavailable"));
    }

    #[tokio::test]
    async fn malformed_empty_and_incomplete_responses_remain_errors() {
        let cases = [
            ResponseTemplate::new(200).set_body_string("not json"),
            ResponseTemplate::new(200).set_body_json(json!({
                "message": {"role": "assistant", "content": "   "},
                "done": true
            })),
            ResponseTemplate::new(200).set_body_json(json!({
                "message": {"role": "assistant", "content": "partial"},
                "done": false
            })),
            ResponseTemplate::new(200).set_body_json(json!({
                "message": {"role": "assistant", "content": "partial"},
                "done": true,
                "done_reason": "length"
            })),
        ];

        for response in cases {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .respond_with(response)
                .mount(&server)
                .await;

            assert!(
                within_test_timeout(client(&server).answer(&[], "hello", &[]))
                    .await
                    .is_err()
            );
        }
    }

    #[tokio::test]
    async fn sends_base64_image_data_with_the_current_question() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .and(body_json(json!({
                "model": "test-model",
                "stream": false,
                "messages": [
                    {"role": "system", "content": TEACHER_INSTRUCTIONS},
                    {"role": "user", "content": "What can you see?", "images": ["AAECAw=="]}
                ]
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "message": {"role": "assistant", "content": "A test image."},
                "done": true,
                "done_reason": "stop"
            })))
            .mount(&server)
            .await;

        let answer =
            within_test_timeout(client(&server).answer(&[], "What can you see?", &[&[0, 1, 2, 3]]))
                .await
                .unwrap();

        assert_eq!(answer, "A test image.");
    }

    #[tokio::test]
    async fn sends_multiple_image_bytes_in_input_order() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .and(body_json(json!({
                "model": "test-model",
                "stream": false,
                "messages": [
                    {"role": "system", "content": TEACHER_INSTRUCTIONS},
                    {"role": "user", "content": "Compare these.", "images": ["AQI=", "AwQF"]}
                ]
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "message": {"role": "assistant", "content": "They differ."},
                "done": true,
                "done_reason": "stop"
            })))
            .mount(&server)
            .await;

        let answer = within_test_timeout(client(&server).answer(
            &[],
            "Compare these.",
            &[&[1, 2], &[3, 4, 5]],
        ))
        .await
        .unwrap();

        assert_eq!(answer, "They differ.");
    }
}
