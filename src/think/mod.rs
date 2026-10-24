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

use crate::telemetry::{self, Interaction, OllamaMetricData};

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

/// Whether the selected model has been explicitly configured to accept image
/// payloads.  This is a local application contract, not a guess from a model
/// name or a claim about what an Ollama server might support.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImageInputSupport {
    Vision,
    TextOnly,
}

impl ImageInputSupport {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Vision => "vision",
            Self::TextOnly => "text-only",
        }
    }
}

/// Configuration for a loopback-only Ollama endpoint.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OllamaConfig {
    endpoint: Url,
    model: String,
    image_input_support: ImageInputSupport,
}

impl OllamaConfig {
    /// Uses `OLLAMA_URL` and `OLLAMA_MODEL`, or the local defaults.
    pub fn from_environment() -> Result<Self, ThinkError> {
        let endpoint = env::var("OLLAMA_URL").unwrap_or_else(|_| DEFAULT_OLLAMA_URL.to_owned());
        let model = env::var("OLLAMA_MODEL").unwrap_or_else(|_| DEFAULT_OLLAMA_MODEL.to_owned());

        Self::new_with_image_support(&endpoint, model, ImageInputSupport::Vision)
    }

    /// Builds a configuration for tests or an explicitly configured local server.
    pub fn new(endpoint: &str, model: impl Into<String>) -> Result<Self, ThinkError> {
        Self::new_with_image_support(endpoint, model, ImageInputSupport::Vision)
    }

    /// Builds a configuration with an explicit local image-input contract.
    /// Profiles use this to keep a text-only Pi model from receiving a photo
    /// merely because it shares the same Ollama transport as the laptop model.
    pub fn new_with_image_support(
        endpoint: &str,
        model: impl Into<String>,
        image_input_support: ImageInputSupport,
    ) -> Result<Self, ThinkError> {
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

        Ok(Self {
            endpoint,
            model,
            image_input_support,
        })
    }

    fn chat_url(&self) -> Result<Url, ThinkError> {
        self.endpoint
            .join("api/chat")
            .map_err(|error| ThinkError::InvalidEndpoint(error.to_string()))
    }

    pub fn image_input_support(&self) -> ImageInputSupport {
        self.image_input_support
    }

    pub fn model(&self) -> &str {
        &self.model
    }
}

/// A reusable HTTP client for the local Ollama process.
#[derive(Clone)]
pub struct OllamaClient {
    http: Client,
    config: OllamaConfig,
}

/// A complete non-streaming Ollama answer plus optional server-reported
/// execution statistics. These fields are absent on older servers and that is
/// intentionally represented as `None`, never as zero.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct OllamaAnswer {
    pub text: String,
    pub statistics: OllamaStatistics,
}

/// Optional timings/counts supplied by Ollama's `/api/chat` response.
///
/// Durations are stored exactly as Ollama reports them: nanoseconds. Callers
/// use the explicit conversion helpers when rendering or recording metrics.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct OllamaStatistics {
    pub model: Option<String>,
    pub total_duration_ns: Option<u64>,
    pub load_duration_ns: Option<u64>,
    pub prompt_eval_count: Option<u64>,
    pub prompt_eval_cached_count: Option<u64>,
    pub prompt_eval_duration_ns: Option<u64>,
    pub eval_count: Option<u64>,
    pub eval_duration_ns: Option<u64>,
    pub done_reason: Option<String>,
}

impl OllamaStatistics {
    pub fn nanoseconds_to_seconds(value: Option<u64>) -> Option<f64> {
        value.map(|nanoseconds| Duration::from_nanos(nanoseconds).as_secs_f64())
    }

    pub fn nanoseconds_to_milliseconds(value: Option<u64>) -> Option<u64> {
        value.map(|nanoseconds| nanoseconds / 1_000_000)
    }

    /// Generation speed is meaningful only when Ollama supplied a positive
    /// generation duration and a token count. It is unavailable for a zero
    /// duration rather than being rendered as infinity or zero.
    pub fn generation_tokens_per_second(&self) -> Option<f64> {
        let count = self.eval_count?;
        let seconds = Self::nanoseconds_to_seconds(self.eval_duration_ns)?;
        (seconds > 0.0).then_some(count as f64 / seconds)
    }
}

impl OllamaClient {
    pub fn from_environment() -> Result<Self, ThinkError> {
        let client = Self::new(OllamaConfig::from_environment()?)?;
        telemetry::run_event(
            "ollama.client.initialization",
            "configured",
            telemetry::fields([
                ("model", serde_json::json!(client.model())),
                ("stream", serde_json::json!(false)),
            ]),
        );
        Ok(client)
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

    pub fn model(&self) -> &str {
        &self.config.model
    }

    pub fn image_input_support(&self) -> ImageInputSupport {
        self.config.image_input_support()
    }

    pub fn supports_images(&self) -> bool {
        self.image_input_support() == ImageInputSupport::Vision
    }

    /// Requests one complete, non-streaming answer from Ollama.
    ///
    /// This convenience form preserves the transport boundary for callers
    /// that do not own an interaction timeline (including HTTP unit tests).
    pub async fn answer(
        &self,
        history: &[ConversationTurn],
        question: &str,
        images: &[&[u8]],
    ) -> Result<OllamaAnswer, ThinkError> {
        self.answer_with_telemetry(history, question, images, None)
            .await
    }

    /// Same request with optional privacy-preserving timing/correlation.
    /// Text stays inside `ChatRequest` and never enters telemetry attributes.
    pub async fn answer_with_telemetry(
        &self,
        history: &[ConversationTurn],
        question: &str,
        images: &[&[u8]],
        interaction: Option<&Interaction>,
    ) -> Result<OllamaAnswer, ThinkError> {
        if !images.is_empty() && !self.supports_images() {
            return Err(ThinkError::ImagesUnsupported {
                model: self.model().to_owned(),
            });
        }
        let image_bytes = images.iter().map(|bytes| bytes.len()).sum::<usize>();
        if let Some(interaction) = interaction {
            interaction.unavailable(
                "ollama.first_token_latency",
                "current /api/chat request uses stream=false",
            );
            interaction.unavailable(
                "ollama.server_queue_depth",
                "the current Ollama API response does not expose server queue depth",
            );
            interaction.unavailable("ollama.server_version", "not probed on the request path");
            interaction.set_input(telemetry::fields([
                ("model", serde_json::json!(self.config.model)),
                ("request_type", serde_json::json!("chat")),
                ("gen_ai.provider.name", serde_json::json!("ollama")),
                ("gen_ai.operation.name", serde_json::json!("chat")),
                ("gen_ai.request.model", serde_json::json!(self.config.model)),
                ("accepted_history_turns", serde_json::json!(history.len())),
                ("image_count", serde_json::json!(images.len())),
                ("total_image_bytes", serde_json::json!(image_bytes)),
                ("stream", serde_json::json!(false)),
            ]));
        }

        // Base64 encoding happens locally before HTTP dispatch. Keep its
        // timing distinct from the request-shaping step and from Ollama's
        // response time; neither is evidence of a server queue.
        let image_encoding = interaction.and_then(|interaction| {
            (!images.is_empty()).then(|| {
                interaction.stage(
                    "ollama.image.encode",
                    telemetry::fields([
                        ("image_count", serde_json::json!(images.len())),
                        ("total_image_bytes", serde_json::json!(image_bytes)),
                    ]),
                )
            })
        });
        let encoded_images = images
            .iter()
            .map(|bytes| STANDARD.encode(bytes))
            .collect::<Vec<_>>();
        if let Some(stage) = image_encoding {
            let encoded_bytes = encoded_images.iter().map(String::len).sum::<usize>();
            stage.finish(
                "completed",
                telemetry::fields([("encoded_image_bytes", serde_json::json!(encoded_bytes))]),
            );
        }

        let preparation = interaction.map(|interaction| {
            interaction.stage(
                "ollama.request.prepare",
                telemetry::fields([
                    ("history_turns", serde_json::json!(history.len())),
                    ("image_count", serde_json::json!(images.len())),
                    ("total_image_bytes", serde_json::json!(image_bytes)),
                ]),
            )
        });
        let request = ChatRequest::new(&self.config.model, history, question, &encoded_images);
        if let Some(stage) = preparation {
            stage.finish("completed", telemetry::Fields::new());
        }
        let endpoint = self.config.chat_url()?;

        let mut exchange = interaction.map(|interaction| {
            interaction.stage(
                "ollama.http.exchange",
                telemetry::fields([
                    ("http.request.method", serde_json::json!("POST")),
                    (
                        "server.address",
                        serde_json::json!(endpoint.host_str().unwrap_or("loopback")),
                    ),
                    (
                        "server.port",
                        serde_json::json!(endpoint.port_or_known_default()),
                    ),
                ]),
            )
        });
        let response = self
            .http
            .post(endpoint.clone())
            .json(&request)
            .send()
            .await
            .map_err(|source| {
                if let Some(stage) = exchange.take() {
                    stage.finish("connection_failure", telemetry::Fields::new());
                }
                ThinkError::Request { endpoint, source }
            })?;

        let status = response.status();
        if !status.is_success() {
            if let Some(stage) = exchange.take() {
                stage.finish(
                    "http_error",
                    telemetry::fields([(
                        "http.response.status_code",
                        serde_json::json!(status.as_u16()),
                    )]),
                );
            }
            return Err(http_error(status, response).await);
        }
        if let Some(stage) = exchange.take() {
            stage.finish(
                "completed",
                telemetry::fields([(
                    "http.response.status_code",
                    serde_json::json!(status.as_u16()),
                )]),
            );
        }

        let mut decoding = interaction.map(|interaction| {
            interaction.stage("ollama.response.decode", telemetry::Fields::new())
        });
        let response = response.json::<ChatResponse>().await.map_err(|error| {
            if let Some(stage) = decoding.take() {
                stage.finish("decode_failure", telemetry::Fields::new());
            }
            ThinkError::InvalidResponse(error)
        })?;
        if let Some(stage) = decoding.take() {
            stage.finish("completed", telemetry::Fields::new());
        }

        let statistics = response.statistics();
        if let Some(interaction) = interaction {
            interaction.set_ollama(statistics.telemetry_fields());
            interaction.record_ollama_statistics(OllamaMetricData {
                model: &self.config.model,
                total_duration_ns: statistics.total_duration_ns,
                load_duration_ns: statistics.load_duration_ns,
                prompt_eval_duration_ns: statistics.prompt_eval_duration_ns,
                eval_duration_ns: statistics.eval_duration_ns,
                prompt_eval_count: statistics.prompt_eval_count,
                prompt_eval_cached_count: statistics.prompt_eval_cached_count,
                eval_count: statistics.eval_count,
            });
        }

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

        Ok(OllamaAnswer {
            text: answer,
            statistics,
        })
    }
}

impl OllamaStatistics {
    fn telemetry_fields(&self) -> telemetry::Fields {
        telemetry::fields([
            ("response_model", serde_json::json!(self.model)),
            ("gen_ai.response.model", serde_json::json!(self.model)),
            (
                "ollama_total_duration_ms",
                serde_json::json!(Self::nanoseconds_to_milliseconds(self.total_duration_ns)),
            ),
            (
                "ollama_reported_load_duration_ms",
                serde_json::json!(Self::nanoseconds_to_milliseconds(self.load_duration_ns)),
            ),
            (
                "prompt_eval_count",
                serde_json::json!(self.prompt_eval_count),
            ),
            (
                "gen_ai.usage.input_tokens",
                serde_json::json!(self.prompt_eval_count),
            ),
            (
                "prompt_eval_cached_count",
                serde_json::json!(self.prompt_eval_cached_count),
            ),
            (
                "prompt_eval_duration_ms",
                serde_json::json!(Self::nanoseconds_to_milliseconds(
                    self.prompt_eval_duration_ns
                )),
            ),
            ("eval_count", serde_json::json!(self.eval_count)),
            (
                "gen_ai.usage.output_tokens",
                serde_json::json!(self.eval_count),
            ),
            (
                "eval_duration_ms",
                serde_json::json!(Self::nanoseconds_to_milliseconds(self.eval_duration_ns)),
            ),
            (
                "generation_tokens_per_second",
                serde_json::json!(self.generation_tokens_per_second()),
            ),
            ("done_reason", serde_json::json!(self.done_reason)),
        ])
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
    fn new(
        model: &str,
        history: &[ConversationTurn],
        question: &str,
        encoded_images: &[String],
    ) -> Self {
        let mut messages = Vec::with_capacity(2 + history.len() * 2);
        messages.push(ChatMessage::system(TEACHER_INSTRUCTIONS));

        for turn in history {
            messages.push(ChatMessage::user(&turn.user, &[]));
            messages.push(ChatMessage::assistant(&turn.assistant));
        }

        messages.push(ChatMessage::user(question, encoded_images));

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

    fn user(content: &str, images: &[String]) -> Self {
        Self {
            role: "user",
            content: content.to_owned(),
            images: images.to_vec(),
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
    model: Option<String>,
    message: Option<ResponseMessage>,
    done: Option<bool>,
    done_reason: Option<String>,
    total_duration: Option<u64>,
    load_duration: Option<u64>,
    prompt_eval_count: Option<u64>,
    prompt_eval_cached_count: Option<u64>,
    prompt_eval_duration: Option<u64>,
    eval_count: Option<u64>,
    eval_duration: Option<u64>,
}

impl ChatResponse {
    fn statistics(&self) -> OllamaStatistics {
        OllamaStatistics {
            model: self.model.clone(),
            total_duration_ns: self.total_duration,
            load_duration_ns: self.load_duration,
            prompt_eval_count: self.prompt_eval_count,
            prompt_eval_cached_count: self.prompt_eval_cached_count,
            prompt_eval_duration_ns: self.prompt_eval_duration,
            eval_count: self.eval_count,
            eval_duration_ns: self.eval_duration,
            done_reason: self.done_reason.clone(),
        }
    }
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

    #[error(
        "configured model `{model}` is marked text-only; configure a verified vision model before submitting photographs"
    )]
    ImagesUnsupported { model: String },
}

impl ThinkError {
    /// A bounded, content-free classification for telemetry. Detailed server
    /// diagnostics remain in the human-readable terminal log.
    pub fn classification(&self) -> &'static str {
        match self {
            Self::Request { source, .. } if source.is_timeout() => "timeout",
            Self::Request { .. } => "connection_failure",
            Self::Http { .. } => "http_error",
            Self::InvalidResponse(_) => "decode_failure",
            Self::IncompleteResponse => "incomplete_response",
            Self::EmptyAnswer => "empty_answer",
            Self::BuildClient(_)
            | Self::InvalidEndpoint(_)
            | Self::UnsupportedScheme(_)
            | Self::NonLoopbackEndpoint(_)
            | Self::EndpointHasPath(_)
            | Self::EmptyModel
            | Self::ImagesUnsupported { .. } => "configuration",
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{future::Future, time::Duration};

    use serde_json::json;
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{body_json, method, path},
    };

    use super::{
        ConversationTurn, ImageInputSupport, OllamaClient, OllamaConfig, OllamaStatistics,
        TEACHER_INSTRUCTIONS, ThinkError,
    };

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
    async fn a_text_only_configuration_rejects_photo_bytes_before_http() {
        let server = MockServer::start().await;
        let client = OllamaClient::new(
            OllamaConfig::new_with_image_support(
                &server.uri(),
                "text-only-model",
                ImageInputSupport::TextOnly,
            )
            .unwrap(),
        )
        .unwrap();

        let result = client
            .answer(&[], "What is visible?", &[b"not an image"])
            .await;
        assert!(matches!(result, Err(ThinkError::ImagesUnsupported { .. })));
        assert!(server.received_requests().await.unwrap().is_empty());
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
                "model": "test-model",
                "message": {"role": "assistant", "content": "It helps limit water loss and damage."},
                "done": true,
                "done_reason": "stop",
                "total_duration": 3_250_000_000u64,
                "load_duration": 1_000_000_000u64,
                "prompt_eval_count": 42,
                "prompt_eval_cached_count": 12,
                "prompt_eval_duration": 500_000_000u64,
                "eval_count": 10,
                "eval_duration": 2_000_000_000u64
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

        assert_eq!(answer.text, "It helps limit water loss and damage.");
        assert_eq!(answer.statistics.model.as_deref(), Some("test-model"));
        assert_eq!(answer.statistics.total_duration_ns, Some(3_250_000_000));
        assert_eq!(answer.statistics.prompt_eval_cached_count, Some(12));
        assert_eq!(answer.statistics.generation_tokens_per_second(), Some(5.0));
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

        assert_eq!(answer.text, "A test image.");
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

        assert_eq!(answer.text, "They differ.");
    }

    #[test]
    fn ollama_statistic_conversions_keep_missing_and_zero_values_honest() {
        assert_eq!(OllamaStatistics::nanoseconds_to_seconds(None), None);
        assert_eq!(
            OllamaStatistics::nanoseconds_to_milliseconds(Some(2_999_999)),
            Some(2)
        );
        let missing = OllamaStatistics::default();
        assert_eq!(missing.generation_tokens_per_second(), None);
        let zero_duration = OllamaStatistics {
            eval_count: Some(12),
            eval_duration_ns: Some(0),
            ..OllamaStatistics::default()
        };
        assert_eq!(zero_duration.generation_tokens_per_second(), None);
    }
}
