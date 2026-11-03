//! The local Ollama transport boundary.
//!
//! This module knows the HTTP and JSON details of Ollama's `/api/chat`
//! endpoint. It deliberately does not print, read terminal input, or update
//! application history; the controller added later owns those decisions.

pub(crate) mod context;
pub(crate) mod tools;

use std::{
    env,
    sync::{Arc, RwLock},
    time::Duration,
};

use base64::{Engine, engine::general_purpose::STANDARD};
use reqwest::{Client, StatusCode, Url, redirect::Policy};
use serde::{Deserialize, Serialize, ser::Error as _};
use serde_json::Value;
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
///
/// Tool rounds remain attached to the exchange that caused them. This keeps
/// an older history trim from retaining a tool result after dropping the
/// assistant tool call that made the result meaningful.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConversationTurn {
    pub user: String,
    pub tool_rounds: Vec<ToolRound>,
    pub assistant: String,
}

/// One assistant tool-call message and all of its following tool results.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolRound {
    pub assistant_content: String,
    pub calls: Vec<ToolCall>,
    pub results: Vec<ToolResult>,
}

/// A model-requested function call retained in current-session history.
///
/// Arguments stay as compact JSON so they can be sent back to Ollama in their
/// original shape, but are never written to telemetry or ordinary diagnostics.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolCall {
    pub index: Option<u64>,
    pub name: String,
    pub arguments_json: String,
}

impl ToolCall {
    pub(crate) fn from_value(index: Option<u64>, name: String, arguments: Value) -> Self {
        Self {
            index,
            name,
            arguments_json: serde_json::to_string(&arguments)
                .expect("a serde_json::Value always serializes"),
        }
    }

    pub(crate) fn arguments(&self) -> Result<Value, serde_json::Error> {
        serde_json::from_str(&self.arguments_json)
    }
}

impl Serialize for ToolCall {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        #[derive(Serialize)]
        struct Function<'a> {
            #[serde(skip_serializing_if = "Option::is_none")]
            index: Option<u64>,
            name: &'a str,
            arguments: Value,
        }

        #[derive(Serialize)]
        struct WireCall<'a> {
            #[serde(rename = "type")]
            call_type: &'static str,
            function: Function<'a>,
        }

        let arguments = self.arguments().map_err(S::Error::custom)?;
        WireCall {
            call_type: "function",
            function: Function {
                index: self.index,
                name: &self.name,
                arguments,
            },
        }
        .serialize(serializer)
    }
}

/// A bounded textual result returned to the model after a tool call.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolResult {
    pub tool_name: String,
    pub content: String,
}

/// A backend capability that is either affirmatively advertised,
/// affirmatively absent, or unavailable because metadata could not be
/// obtained/interpreted.  The reason is a concise diagnostic, never a prompt
/// or audio/image payload.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CapabilityStatus {
    Supported,
    Unsupported,
    Unknown { reason: String },
}

impl CapabilityStatus {
    pub const fn label(&self) -> &'static str {
        match self {
            Self::Supported => "supported",
            Self::Unsupported => "unsupported",
            Self::Unknown { .. } => "unknown",
        }
    }
}

/// Capability metadata cached for one configured Ollama model/backend pair.
/// The raw advertised names remain available for concise diagnostics, but do
/// not become dynamic application behavior by themselves.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelCapabilities {
    pub image_input: CapabilityStatus,
    pub audio_input: CapabilityStatus,
    pub audio_output: CapabilityStatus,
    pub tool_calling: CapabilityStatus,
    /// The model-advertised theoretical context length, if `/api/show`
    /// supplied a recognized `*.context_length` field. This is not treated as
    /// the actual runtime setting; the context manager owns that explicit
    /// budget separately.
    pub model_context_capacity: Option<usize>,
    advertised: Vec<String>,
}

impl ModelCapabilities {
    fn not_yet_discovered() -> Self {
        Self::unknown("model capabilities have not been discovered yet")
    }

    fn unknown(reason: impl Into<String>) -> Self {
        let reason = reason.into();
        Self {
            image_input: CapabilityStatus::Unknown {
                reason: reason.clone(),
            },
            audio_input: CapabilityStatus::Unknown {
                reason: reason.clone(),
            },
            audio_output: CapabilityStatus::Unknown {
                reason: reason.clone(),
            },
            tool_calling: CapabilityStatus::Unknown { reason },
            model_context_capacity: None,
            advertised: Vec::new(),
        }
    }

    #[cfg(test)]
    fn from_advertised(capabilities: Vec<String>) -> Self {
        Self::from_show(capabilities, None)
    }

    fn from_show(capabilities: Vec<String>, model_context_capacity: Option<usize>) -> Self {
        let has = |name: &str| capabilities.iter().any(|value| value == name);
        Self {
            image_input: if has("vision") {
                CapabilityStatus::Supported
            } else {
                CapabilityStatus::Unsupported
            },
            // Ollama's documented `/api/show` capability list currently
            // advertises `speech` for text-to-speech output. It does not
            // define a standard direct audio-input request capability, so we
            // refuse to invent one from a model family or arbitrary metadata.
            audio_input: CapabilityStatus::Unknown {
                reason: "the current Ollama API does not advertise a documented direct audio-input capability".into(),
            },
            audio_output: if has("speech") {
                CapabilityStatus::Supported
            } else {
                CapabilityStatus::Unsupported
            },
            tool_calling: if has("tools") {
                CapabilityStatus::Supported
            } else {
                CapabilityStatus::Unsupported
            },
            model_context_capacity,
            advertised: capabilities,
        }
    }

    /// Capability strings reported by the backend, sorted deterministically
    /// for logs/tests. They contain no conversation or media data.
    pub fn advertised(&self) -> &[String] {
        &self.advertised
    }
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

    fn show_url(&self) -> Result<Url, ThinkError> {
        self.endpoint
            .join("api/show")
            .map_err(|error| ThinkError::InvalidEndpoint(error.to_string()))
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
    capabilities: Arc<RwLock<Option<ModelCapabilities>>>,
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

        Ok(Self {
            http,
            config,
            capabilities: Arc::new(RwLock::new(None)),
        })
    }

    pub fn model(&self) -> &str {
        &self.config.model
    }

    pub fn supports_images(&self) -> bool {
        matches!(self.capabilities().image_input, CapabilityStatus::Supported)
    }

    /// Whether the selected Ollama model affirmatively advertises native tool
    /// calling through its cached `/api/show` metadata.
    pub fn supports_tools(&self) -> bool {
        matches!(
            self.capabilities().tool_calling,
            CapabilityStatus::Supported
        )
    }

    /// A concise, capability-grounded reason to keep a camera inactive.
    pub fn image_input_restriction(&self) -> String {
        match &self.capabilities().image_input {
            CapabilityStatus::Supported => "image input is supported".into(),
            CapabilityStatus::Unsupported => format!(
                "model `{}` does not advertise vision through Ollama /api/show",
                self.model()
            ),
            CapabilityStatus::Unknown { reason } => format!(
                "model `{}` image capability is unknown: {reason}",
                self.model()
            ),
        }
    }

    /// Returns the once-per-session cached capability record. Before
    /// discovery, all capabilities are explicitly unknown rather than guessed
    /// from a model name.
    pub fn capabilities(&self) -> ModelCapabilities {
        self.capabilities
            .read()
            .expect("Ollama capability cache lock was poisoned")
            .clone()
            .unwrap_or_else(ModelCapabilities::not_yet_discovered)
    }

    /// Reads `/api/show` exactly once for this client and caches either the
    /// advertised capabilities or an explicit unknown state. A caller can log
    /// the error while the rest of the text-only application stays usable.
    pub async fn discover_capabilities(&self) -> Result<ModelCapabilities, ThinkError> {
        if let Some(capabilities) = self
            .capabilities
            .read()
            .expect("Ollama capability cache lock was poisoned")
            .clone()
        {
            return Ok(capabilities);
        }

        let endpoint = self.config.show_url()?;
        let result = async {
            let response = self
                .http
                .post(endpoint.clone())
                .json(&ShowRequest {
                    model: self.config.model.clone(),
                })
                .send()
                .await
                .map_err(|source| ThinkError::Request {
                    endpoint: endpoint.clone(),
                    source,
                })?;
            if !response.status().is_success() {
                return Err(http_error(response.status(), response).await);
            }
            let response = response
                .json::<ShowResponse>()
                .await
                .map_err(ThinkError::InvalidResponse)?;
            let context_capacity = model_context_capacity(&response.model_info);
            let capabilities = match response.capabilities {
                Some(mut capabilities) => {
                    capabilities.sort();
                    ModelCapabilities::from_show(capabilities, context_capacity)
                }
                None => ModelCapabilities::unknown(
                    "the Ollama /api/show response omitted its capabilities field",
                ),
            };
            Ok(capabilities)
        }
        .await;

        match result {
            Ok(capabilities) => {
                telemetry::run_event(
                    "ollama.model.capabilities",
                    "discovered",
                    telemetry::fields([
                        ("model", serde_json::json!(self.model())),
                        (
                            "image_input",
                            serde_json::json!(capabilities.image_input.label()),
                        ),
                        (
                            "audio_input",
                            serde_json::json!(capabilities.audio_input.label()),
                        ),
                        (
                            "audio_output",
                            serde_json::json!(capabilities.audio_output.label()),
                        ),
                        (
                            "tool_calling",
                            serde_json::json!(capabilities.tool_calling.label()),
                        ),
                        (
                            "model_context_capacity",
                            serde_json::json!(capabilities.model_context_capacity),
                        ),
                    ]),
                );
                *self
                    .capabilities
                    .write()
                    .expect("Ollama capability cache lock was poisoned") =
                    Some(capabilities.clone());
                Ok(capabilities)
            }
            Err(error) => {
                let unknown = ModelCapabilities::unknown(format!(
                    "could not read /api/show for model `{}`: {}",
                    self.model(),
                    error.classification()
                ));
                *self
                    .capabilities
                    .write()
                    .expect("Ollama capability cache lock was poisoned") = Some(unknown);
                Err(error)
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn set_capabilities_for_test(&self, advertised: &[&str]) {
        let mut advertised = advertised
            .iter()
            .map(|value| (*value).to_owned())
            .collect::<Vec<_>>();
        advertised.sort();
        *self
            .capabilities
            .write()
            .expect("Ollama capability cache lock was poisoned") =
            Some(ModelCapabilities::from_advertised(advertised));
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
            return Err(match self.capabilities().image_input {
                CapabilityStatus::Unsupported => ThinkError::ImagesUnsupported {
                    model: self.model().to_owned(),
                },
                CapabilityStatus::Unknown { .. } => ThinkError::ImageCapabilitiesUnknown {
                    model: self.model().to_owned(),
                },
                CapabilityStatus::Supported => unreachable!("supports_images checked support"),
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
        let response = self.send_request(request, interaction).await?;
        let answer = response.content;

        if answer.trim().is_empty() {
            return Err(ThinkError::EmptyAnswer);
        }

        Ok(OllamaAnswer {
            text: answer,
            statistics: response.statistics,
        })
    }

    /// Sends one already-assembled non-streaming Ollama chat request. It is
    /// used by the bounded application-owned tool loop; callers retain all
    /// dialogue/tool content locally and pass only redacted measurements to
    /// telemetry.
    pub(crate) async fn chat_messages_with_telemetry(
        &self,
        messages: Vec<ChatMessage>,
        tools: Vec<ToolDefinition>,
        runtime_context_tokens: Option<usize>,
        interaction: Option<&Interaction>,
    ) -> Result<ModelResponse, ThinkError> {
        let request =
            ChatRequest::from_messages(&self.config.model, messages, tools, runtime_context_tokens);
        self.send_request(request, interaction).await
    }

    async fn send_request(
        &self,
        request: ChatRequest,
        interaction: Option<&Interaction>,
    ) -> Result<ModelResponse, ThinkError> {
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

        let message = response.message.unwrap_or(ResponseMessage {
            content: String::new(),
            tool_calls: Vec::new(),
        });
        Ok(ModelResponse {
            content: message.content,
            tool_calls: message
                .tool_calls
                .into_iter()
                .map(|call| {
                    ToolCall::from_value(
                        call.function.index,
                        call.function.name,
                        call.function.arguments,
                    )
                })
                .collect(),
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
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<ToolDefinition>,
    #[serde(skip_serializing_if = "Option::is_none")]
    options: Option<ChatOptions>,
    stream: bool,
}

/// One native Ollama function schema owned by Independence. This is an
/// intentionally small local list, not a generalized plugin registry.
#[derive(Clone, Debug, Serialize)]
pub(crate) struct ToolDefinition {
    #[serde(rename = "type")]
    call_type: &'static str,
    pub(crate) function: ToolFunctionDefinition,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct ToolFunctionDefinition {
    pub(crate) name: &'static str,
    pub(crate) description: &'static str,
    pub(crate) parameters: Value,
}

impl ToolDefinition {
    pub(crate) fn function(
        name: &'static str,
        description: &'static str,
        parameters: Value,
    ) -> Self {
        Self {
            call_type: "function",
            function: ToolFunctionDefinition {
                name,
                description,
                parameters,
            },
        }
    }
}

/// The one runtime generation option used by the context manager. Sending an
/// explicit value means the budget is an actual request setting rather than a
/// guess based only on model metadata.
#[derive(Clone, Copy, Debug, Serialize)]
struct ChatOptions {
    num_ctx: usize,
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
            for round in &turn.tool_rounds {
                messages.push(ChatMessage::assistant_with_tools(
                    &round.assistant_content,
                    &round.calls,
                ));
                for result in &round.results {
                    messages.push(ChatMessage::tool(&result.tool_name, &result.content));
                }
            }
            messages.push(ChatMessage::assistant(&turn.assistant));
        }

        messages.push(ChatMessage::user(question, encoded_images));

        Self {
            model: model.to_owned(),
            messages,
            tools: Vec::new(),
            options: None,
            stream: false,
        }
    }

    fn from_messages(
        model: &str,
        messages: Vec<ChatMessage>,
        tools: Vec<ToolDefinition>,
        runtime_context_tokens: Option<usize>,
    ) -> Self {
        Self {
            model: model.to_owned(),
            messages,
            tools,
            options: runtime_context_tokens.map(|num_ctx| ChatOptions { num_ctx }),
            stream: false,
        }
    }
}

#[derive(Serialize, Clone, Debug)]
pub(crate) struct ChatMessage {
    role: &'static str,
    content: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    images: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tool_calls: Vec<ToolCall>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_name: Option<String>,
}

impl ChatMessage {
    pub(crate) fn system(content: &str) -> Self {
        Self {
            role: "system",
            content: content.to_owned(),
            images: Vec::new(),
            tool_calls: Vec::new(),
            tool_name: None,
        }
    }

    pub(crate) fn user(content: &str, images: &[String]) -> Self {
        Self {
            role: "user",
            content: content.to_owned(),
            images: images.to_vec(),
            tool_calls: Vec::new(),
            tool_name: None,
        }
    }

    pub(crate) fn assistant(content: &str) -> Self {
        Self {
            role: "assistant",
            content: content.to_owned(),
            images: Vec::new(),
            tool_calls: Vec::new(),
            tool_name: None,
        }
    }

    pub(crate) fn assistant_with_tools(content: &str, calls: &[ToolCall]) -> Self {
        Self {
            role: "assistant",
            content: content.to_owned(),
            images: Vec::new(),
            tool_calls: calls.to_vec(),
            tool_name: None,
        }
    }

    pub(crate) fn tool(tool_name: &str, content: &str) -> Self {
        Self {
            role: "tool",
            content: content.to_owned(),
            images: Vec::new(),
            tool_calls: Vec::new(),
            tool_name: Some(tool_name.to_owned()),
        }
    }

    pub(crate) fn conservative_token_estimate(&self, image_token_allowance: usize) -> usize {
        // This deliberately counts every UTF-8 byte as a possible token, plus
        // a small message envelope. It overestimates ordinary prose but avoids
        // pretending that character or word counts are tokenizer-exact.
        self.content
            .len()
            .saturating_add(self.role.len())
            .saturating_add(self.tool_name.as_deref().map_or(0, str::len))
            .saturating_add(
                self.tool_calls
                    .iter()
                    .map(|call| call.name.len().saturating_add(call.arguments_json.len()))
                    .sum::<usize>(),
            )
            .saturating_add(self.images.len().saturating_mul(image_token_allowance))
            .saturating_add(8)
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
    #[serde(default)]
    content: String,
    #[serde(default)]
    tool_calls: Vec<ResponseToolCall>,
}

#[derive(Deserialize)]
struct ResponseToolCall {
    function: ResponseToolFunction,
}

#[derive(Deserialize)]
struct ResponseToolFunction {
    index: Option<u64>,
    name: String,
    arguments: Value,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ModelResponse {
    pub(crate) content: String,
    pub(crate) tool_calls: Vec<ToolCall>,
    pub(crate) statistics: OllamaStatistics,
}

#[derive(Deserialize)]
struct OllamaErrorResponse {
    error: Option<String>,
}

#[derive(Serialize)]
struct ShowRequest {
    model: String,
}

#[derive(Deserialize)]
struct ShowResponse {
    capabilities: Option<Vec<String>>,
    #[serde(default)]
    model_info: serde_json::Map<String, Value>,
}

/// Extracts the documented-style `*.context_length` field without assuming a
/// particular model family prefix. An absent or malformed field remains
/// unknown; the context manager will use its explicit runtime fallback.
fn model_context_capacity(model_info: &serde_json::Map<String, Value>) -> Option<usize> {
    model_info
        .iter()
        .filter(|(key, _)| key == &"context_length" || key.ends_with(".context_length"))
        .filter_map(|(_, value)| value.as_u64())
        .filter_map(|value| usize::try_from(value).ok())
        .filter(|value| *value > 0)
        .max()
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

    #[error("model `{model}` does not advertise vision through Ollama /api/show")]
    ImagesUnsupported { model: String },

    #[error(
        "could not verify whether configured model `{model}` accepts images; camera capture remains off until /api/show succeeds"
    )]
    ImageCapabilitiesUnknown { model: String },
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
            | Self::ImagesUnsupported { .. }
            | Self::ImageCapabilitiesUnknown { .. } => "configuration",
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
        CapabilityStatus, ConversationTurn, OllamaClient, OllamaConfig, OllamaStatistics,
        TEACHER_INSTRUCTIONS, ThinkError,
    };

    fn client(server: &MockServer) -> OllamaClient {
        let config = OllamaConfig::new(&server.uri(), "test-model").unwrap();
        let client = OllamaClient::new(config).unwrap();
        client.set_capabilities_for_test(&["vision"]);
        client
    }

    async fn within_test_timeout<T>(future: impl Future<Output = T>) -> T {
        tokio::time::timeout(Duration::from_secs(3), future)
            .await
            .expect("mock HTTP test exceeded its three-second timeout")
    }

    #[tokio::test]
    async fn show_capabilities_are_cached_for_one_selected_model() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/show"))
            .and(body_json(json!({"model": "verified-model"})))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "capabilities": ["tools", "vision", "completion"],
                "model_info": {"llama.context_length": 32768}
            })))
            .mount(&server)
            .await;
        let client =
            OllamaClient::new(OllamaConfig::new(&server.uri(), "verified-model").unwrap()).unwrap();

        let first = within_test_timeout(client.discover_capabilities())
            .await
            .unwrap();
        let second = within_test_timeout(client.discover_capabilities())
            .await
            .unwrap();

        assert_eq!(first, second);
        assert_eq!(first.advertised(), ["completion", "tools", "vision"]);
        assert!(matches!(first.image_input, CapabilityStatus::Supported));
        assert!(matches!(first.audio_output, CapabilityStatus::Unsupported));
        assert!(matches!(first.tool_calling, CapabilityStatus::Supported));
        assert_eq!(first.model_context_capacity, Some(32_768));
        assert!(matches!(
            first.audio_input,
            CapabilityStatus::Unknown { .. }
        ));
        assert!(client.supports_images());
        assert!(client.supports_tools());
        let requests = server.received_requests().await.unwrap();
        assert_eq!(
            requests
                .iter()
                .filter(|request| request.url.path() == "/api/show")
                .count(),
            1,
            "the session should reuse the first /api/show record"
        );
    }

    #[tokio::test]
    async fn metadata_without_vision_keeps_camera_off() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/show"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "capabilities": ["completion"]
            })))
            .mount(&server)
            .await;
        let client =
            OllamaClient::new(OllamaConfig::new(&server.uri(), "text-model").unwrap()).unwrap();

        let capabilities = within_test_timeout(client.discover_capabilities())
            .await
            .unwrap();

        assert!(matches!(
            capabilities.image_input,
            CapabilityStatus::Unsupported
        ));
        assert!(!client.supports_images());
    }

    #[tokio::test]
    async fn omitted_show_capabilities_remain_unknown_and_keep_camera_off() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/show"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
            .mount(&server)
            .await;
        let client =
            OllamaClient::new(OllamaConfig::new(&server.uri(), "older-model").unwrap()).unwrap();

        let capabilities = within_test_timeout(client.discover_capabilities())
            .await
            .unwrap();

        assert!(matches!(
            capabilities.image_input,
            CapabilityStatus::Unknown { .. }
        ));
        let result = client.answer(&[], "What is visible?", &[b"fixture"]).await;
        assert!(matches!(
            result,
            Err(ThinkError::ImageCapabilitiesUnknown { .. })
        ));
    }

    #[tokio::test]
    async fn unavailable_show_metadata_is_cached_as_unknown_and_keeps_camera_off() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/show"))
            .respond_with(ResponseTemplate::new(503).set_body_json(json!({
                "error": "metadata unavailable"
            })))
            .mount(&server)
            .await;
        let client =
            OllamaClient::new(OllamaConfig::new(&server.uri(), "offline-model").unwrap()).unwrap();

        assert!(
            within_test_timeout(client.discover_capabilities())
                .await
                .is_err()
        );
        assert!(matches!(
            client.capabilities().image_input,
            CapabilityStatus::Unknown { .. }
        ));
        assert!(matches!(
            client.answer(&[], "What is visible?", &[b"fixture"]).await,
            Err(ThinkError::ImageCapabilitiesUnknown { .. })
        ));
        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 1, "unknown capability should be cached");
    }

    #[tokio::test]
    async fn metadata_without_vision_rejects_photo_bytes_before_http() {
        let server = MockServer::start().await;
        let client =
            OllamaClient::new(OllamaConfig::new(&server.uri(), "text-model").unwrap()).unwrap();
        client.set_capabilities_for_test(&[]);

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
                tool_rounds: Vec::new(),
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
