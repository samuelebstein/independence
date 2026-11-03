//! Bounded native Ollama tool execution for Independence-owned tools.
//!
//! This module owns model-tool protocol, argument validation, progress events,
//! cancellation, and prompt continuity. The reusable `independence-memory`
//! crate owns only SQLite persistence and never sees a model/client/UI type.

use std::{
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use base64::{Engine, engine::general_purpose::STANDARD};
use independence_memory::{CorrectInput, Evidence, MemoryId, MemoryInput, MemoryStore};
use serde_json::{Value, json};
use thiserror::Error;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::{
    protocol::{Event, ProgressEvent, TurnId},
    telemetry::{self, Interaction},
};

use super::{
    ChatMessage, ConversationTurn, OllamaClient, ThinkError, ToolCall, ToolDefinition, ToolResult,
    ToolRound,
    context::{ContextBudget, ContextError, estimate_tool_definitions, prepare},
};

const MAX_TOOL_CALL_ITERATIONS: usize = 4;
const MAX_TOOL_RESULT_BYTES: usize = 24 * 1024;
const MAX_TOOL_ARGUMENT_BYTES: usize = 32 * 1024;
/// SQLite operations are local and bounded by input limits, but a separate
/// deadline prevents a wedged filesystem or connection gate from holding a
/// conversation turn forever. A running blocking task cannot be forcibly
/// rolled back; its late result is never accepted after this deadline.
const DEFAULT_MEMORY_TOOL_TIMEOUT: Duration = Duration::from_secs(10);

/// A cloneable application boundary around one local memory database. The
/// mutex serializes SQLite access; it does not introduce a second queue or
/// persistence layer.
#[derive(Clone)]
pub(crate) struct MemoryService {
    store: Arc<Mutex<MemoryStore>>,
    work: Arc<MemoryWorkState>,
}

#[derive(Default)]
struct MemoryWorkState {
    pending: AtomicUsize,
    active: AtomicUsize,
}

struct MemoryExecution {
    gate_wait: std::time::Duration,
    pending_depth: usize,
    active_work: usize,
    execution_duration: std::time::Duration,
    result: Result<Value, ToolExecutionError>,
}

impl MemoryService {
    pub(crate) fn open(
        data_directory: impl AsRef<Path>,
    ) -> Result<Self, independence_memory::MemoryError> {
        Ok(Self {
            store: Arc::new(Mutex::new(MemoryStore::open(data_directory)?)),
            work: Arc::new(MemoryWorkState::default()),
        })
    }

    pub(crate) fn database_path(&self) -> Result<std::path::PathBuf, ToolHarnessError> {
        self.store
            .lock()
            .map(|store| store.database_path().to_path_buf())
            .map_err(|_| ToolHarnessError::MemoryWorkerUnavailable)
    }

    fn execute(&self, call: ValidToolCall) -> MemoryExecution {
        let pending_depth = self.work.pending.fetch_add(1, Ordering::AcqRel);
        let gate_started = Instant::now();
        let mut store = match self.store.lock() {
            Ok(store) => store,
            Err(_) => {
                self.work.pending.fetch_sub(1, Ordering::AcqRel);
                return MemoryExecution {
                    gate_wait: gate_started.elapsed(),
                    pending_depth,
                    active_work: self.work.active.load(Ordering::Acquire),
                    execution_duration: std::time::Duration::ZERO,
                    result: Err(ToolExecutionError::StoreUnavailable),
                };
            }
        };
        let gate_wait = gate_started.elapsed();
        self.work.pending.fetch_sub(1, Ordering::AcqRel);
        let active_work = self.work.active.fetch_add(1, Ordering::AcqRel) + 1;
        let execution_started = Instant::now();
        let result = (|| match call {
            ValidToolCall::Remember(input) => {
                let memory = store.remember(input).map_err(ToolExecutionError::Store)?;
                Ok(json!({
                    "ok": true,
                    "operation": "remember",
                    "memory": {
                        "id": memory.id.value(),
                        "status": "current",
                        "created_at_ms": memory.created_at_ms,
                    }
                }))
            }
            ValidToolCall::Recall { query, limit } => {
                let memories = store
                    .recall(&query, limit)
                    .map_err(ToolExecutionError::Store)?;
                Ok(json!({
                    "ok": true,
                    "operation": "recall",
                    "memories": memories.into_iter().map(memory_to_value).collect::<Vec<_>>(),
                }))
            }
            ValidToolCall::Correct(input) => {
                let corrected = store.correct(input).map_err(ToolExecutionError::Store)?;
                Ok(json!({
                    "ok": true,
                    "operation": "correct",
                    "previous_memory_id": corrected.previous.id.value(),
                    "memory": {
                        "id": corrected.current.id.value(),
                        "status": "current",
                        "supersedes_memory_id": corrected.current.supersedes.map(MemoryId::value),
                        "updated_at_ms": corrected.current.updated_at_ms,
                    }
                }))
            }
        })();
        let execution_duration = execution_started.elapsed();
        self.work.active.fetch_sub(1, Ordering::AcqRel);
        MemoryExecution {
            gate_wait,
            pending_depth,
            active_work,
            execution_duration,
            result,
        }
    }
}

/// Runtime configuration selected by the application. It intentionally has a
/// fixed bound; tools cannot make an unbounded agent loop.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ToolLoopConfig {
    pub(crate) context_budget: ContextBudget,
    pub(crate) max_iterations: usize,
    pub(crate) memory_tool_timeout: Duration,
}

impl Default for ToolLoopConfig {
    fn default() -> Self {
        Self {
            context_budget: ContextBudget::default(),
            max_iterations: MAX_TOOL_CALL_ITERATIONS,
            memory_tool_timeout: DEFAULT_MEMORY_TOOL_TIMEOUT,
        }
    }
}

/// A final textual answer plus the internal tool exchanges that must become
/// part of accepted current-session history with that answer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ToolTurnOutput {
    pub(crate) answer: String,
    pub(crate) tool_rounds: Vec<ToolRound>,
}

/// The controller-authorized data for one model/tool turn. Keeping these
/// fields together makes it clear that the tool loop receives one immutable
/// snapshot rather than authority to mutate session history.
pub(crate) struct ToolTurnRequest<'images> {
    pub(crate) history: Vec<ConversationTurn>,
    pub(crate) question: String,
    pub(crate) image_bytes: &'images [&'images [u8]],
    pub(crate) turn_id: TurnId,
}

#[derive(Debug, Error)]
pub(crate) enum ToolHarnessError {
    #[error("local model request failed: {0}")]
    Model(#[from] ThinkError),

    #[error("context preparation failed: {0}")]
    Context(#[from] ContextError),

    #[error("tool execution was cancelled")]
    Cancelled,

    #[error("the tool progress channel closed before the current turn finished")]
    ProgressChannelClosed,

    #[error("the memory worker became unavailable")]
    MemoryWorkerUnavailable,

    #[error("the model exceeded the maximum of {maximum} tool-call iterations")]
    ToolIterationLimit { maximum: usize },

    #[error("the local memory operation exceeded its {timeout_ms} ms deadline")]
    ToolExecutionTimedOut { timeout_ms: u64 },

    #[error("the model returned neither an answer nor a tool call")]
    EmptyFinalAnswer,
}

/// Runs one complete model turn. `memory` is `None` for ordinary Ephemeral
/// Mode, so there are no memory tools, database calls, or persistence paths.
pub(crate) async fn run(
    client: &OllamaClient,
    memory: Option<MemoryService>,
    request: ToolTurnRequest<'_>,
    config: ToolLoopConfig,
    event_sender: mpsc::Sender<Event>,
    cancellation: CancellationToken,
    interaction: &Interaction,
) -> Result<ToolTurnOutput, ToolHarnessError> {
    if cancellation.is_cancelled() {
        return Err(ToolHarnessError::Cancelled);
    }
    let ToolTurnRequest {
        history,
        question,
        image_bytes,
        turn_id,
    } = request;
    let tool_definitions = memory
        .as_ref()
        .map(|_| memory_tool_definitions())
        .unwrap_or_default();
    let tool_definition_json = serde_json::to_vec(&tool_definitions)
        .expect("static Independence tool definitions serialize");
    let tool_definition_tokens = estimate_tool_definitions(&tool_definition_json);
    let image_total_bytes = image_bytes.iter().map(|image| image.len()).sum::<usize>();
    let encoded_images = encode_images(image_bytes, interaction);
    let system = ChatMessage::system(super::TEACHER_INSTRUCTIONS);
    let mut active_messages = vec![ChatMessage::user(&question, &encoded_images)];
    let mut tool_rounds = Vec::new();

    interaction.set_input(telemetry::fields([
        ("model", json!(client.model())),
        (
            "request_type",
            json!(if memory.is_some() {
                "chat_with_memory_tools"
            } else {
                "chat"
            }),
        ),
        ("gen_ai.provider.name", json!("ollama")),
        ("gen_ai.operation.name", json!("chat")),
        ("gen_ai.request.model", json!(client.model())),
        ("accepted_history_turns", json!(history.len())),
        ("image_count", json!(image_bytes.len())),
        ("total_image_bytes", json!(image_total_bytes)),
        (
            "tool_definition_tokens_estimate",
            json!(tool_definition_tokens),
        ),
        (
            "runtime_context_capacity",
            json!(config.context_budget.runtime_context_tokens),
        ),
        (
            "reserved_output_tokens",
            json!(config.context_budget.reserved_output_tokens),
        ),
        (
            "context_estimator",
            json!("conservative_utf8_byte_estimate"),
        ),
        ("stream", json!(false)),
    ]));
    interaction.unavailable(
        "ollama.first_token_latency",
        "current /api/chat request uses stream=false",
    );
    interaction.unavailable(
        "ollama.server_queue_depth",
        "the current Ollama API response does not expose server queue depth",
    );

    for iteration in 0..=config.max_iterations {
        if cancellation.is_cancelled() {
            return Err(ToolHarnessError::Cancelled);
        }
        let prepared = prepare_context(
            &system,
            &history,
            &active_messages,
            tool_definition_tokens,
            config.context_budget,
            interaction,
        )?;
        if prepared.removed_exchanges != 0 {
            send_progress(
                &event_sender,
                &cancellation,
                turn_id,
                ProgressEvent::ReducingHistory {
                    exchanges: prepared.removed_exchanges,
                },
            )
            .await?;
        }
        let messages = assemble_messages(&system, &prepared.history, &active_messages);
        interaction.event(
            "ollama.tool_iteration",
            "requesting",
            telemetry::fields([
                ("iteration", json!(iteration)),
                ("included_message_count", json!(prepared.included_messages)),
                (
                    "estimated_input_tokens",
                    json!(prepared.estimated_tokens_after_trim),
                ),
            ]),
        );
        let _in_flight = interaction.in_flight_guard("ollama");
        let response = tokio::select! {
            biased;
            _ = cancellation.cancelled() => return Err(ToolHarnessError::Cancelled),
            response = client.chat_messages_with_telemetry(
                messages,
                tool_definitions.clone(),
                Some(config.context_budget.runtime_context_tokens),
                Some(interaction),
            ) => response?,
        };
        if cancellation.is_cancelled() {
            return Err(ToolHarnessError::Cancelled);
        }
        if response.tool_calls.is_empty() {
            if response.content.trim().is_empty() {
                return Err(ToolHarnessError::EmptyFinalAnswer);
            }
            return Ok(ToolTurnOutput {
                answer: response.content,
                tool_rounds,
            });
        }
        if memory.is_none() {
            // No tool definitions were supplied for Ephemeral Mode. A model
            // cannot gain a side effect by fabricating a tool call.
            return Err(ToolHarnessError::EmptyFinalAnswer);
        }
        if iteration == config.max_iterations {
            return Err(ToolHarnessError::ToolIterationLimit {
                maximum: config.max_iterations,
            });
        }

        let calls = response.tool_calls;
        let mut results = Vec::with_capacity(calls.len());
        for call in &calls {
            let result = execute_call(
                memory.as_ref().expect("memory presence was checked"),
                call,
                turn_id,
                &event_sender,
                &cancellation,
                interaction,
                config.memory_tool_timeout,
            )
            .await?;
            active_messages.push(ChatMessage::tool(&result.tool_name, &result.content));
            results.push(result);
        }
        active_messages.insert(
            active_messages.len().saturating_sub(results.len()),
            ChatMessage::assistant_with_tools(&response.content, &calls),
        );
        tool_rounds.push(ToolRound {
            assistant_content: response.content,
            calls,
            results,
        });
    }
    Err(ToolHarnessError::ToolIterationLimit {
        maximum: config.max_iterations,
    })
}

fn encode_images(image_bytes: &[&[u8]], interaction: &Interaction) -> Vec<String> {
    if image_bytes.is_empty() {
        return Vec::new();
    }
    let total_image_bytes = image_bytes.iter().map(|image| image.len()).sum::<usize>();
    let stage = interaction.stage(
        "ollama.image.encode",
        telemetry::fields([
            ("image_count", json!(image_bytes.len())),
            ("total_image_bytes", json!(total_image_bytes)),
        ]),
    );
    let images = image_bytes
        .iter()
        .map(|image| STANDARD.encode(image))
        .collect::<Vec<_>>();
    let encoded_image_bytes = images.iter().map(String::len).sum::<usize>();
    stage.finish(
        "completed",
        telemetry::fields([("encoded_image_bytes", json!(encoded_image_bytes))]),
    );
    images
}

fn prepare_context(
    system: &ChatMessage,
    history: &[ConversationTurn],
    active_messages: &[ChatMessage],
    tool_definition_tokens: usize,
    budget: ContextBudget,
    interaction: &Interaction,
) -> Result<super::context::PreparedContext, ToolHarnessError> {
    let stage = interaction.stage(
        "context.prepare",
        telemetry::fields([
            (
                "effective_context_capacity",
                json!(budget.runtime_context_tokens),
            ),
            (
                "reserved_output_tokens",
                json!(budget.reserved_output_tokens),
            ),
            ("safety_margin_tokens", json!(budget.safety_margin_tokens)),
            (
                "tool_definition_tokens_estimate",
                json!(tool_definition_tokens),
            ),
            ("history_exchange_count", json!(history.len())),
            ("estimator", json!("conservative_utf8_byte_estimate")),
        ]),
    );
    match prepare(
        system,
        history,
        active_messages,
        tool_definition_tokens,
        budget,
    ) {
        Ok(prepared) => {
            let trim_outcome = if prepared.removed_exchanges == 0 {
                "completed"
            } else {
                "trimmed"
            };
            stage.finish(
                trim_outcome,
                telemetry::fields([
                    (
                        "available_input_budget",
                        json!(prepared.input_budget_tokens),
                    ),
                    (
                        "estimated_tokens_before_trim",
                        json!(prepared.estimated_tokens_before_trim),
                    ),
                    (
                        "estimated_tokens_after_trim",
                        json!(prepared.estimated_tokens_after_trim),
                    ),
                    ("removed_message_count", json!(prepared.removed_messages)),
                    ("removed_exchange_count", json!(prepared.removed_exchanges)),
                    ("included_message_count", json!(prepared.included_messages)),
                ]),
            );
            if prepared.removed_exchanges != 0 {
                interaction.event(
                    "context.trim",
                    "completed",
                    telemetry::fields([
                        ("removed_message_count", json!(prepared.removed_messages)),
                        ("removed_exchange_count", json!(prepared.removed_exchanges)),
                        (
                            "estimated_tokens_after_trim",
                            json!(prepared.estimated_tokens_after_trim),
                        ),
                    ]),
                );
            }
            Ok(prepared)
        }
        Err(error) => {
            stage.finish(
                "overflow",
                telemetry::fields([("failure_class", json!(context_error_class(&error)))]),
            );
            interaction.event(
                "context.overflow",
                "rejected",
                telemetry::fields([("failure_class", json!(context_error_class(&error)))]),
            );
            Err(ToolHarnessError::Context(error))
        }
    }
}

fn context_error_class(error: &ContextError) -> &'static str {
    match error {
        ContextError::InvalidBudget { .. } => "invalid_budget",
        ContextError::RequiredContextTooLarge { .. } => "required_context_too_large",
    }
}

fn assemble_messages(
    system: &ChatMessage,
    history: &[ConversationTurn],
    active_messages: &[ChatMessage],
) -> Vec<ChatMessage> {
    let history_message_count = history
        .iter()
        .map(super::context::conversation_turn_message_count)
        .sum::<usize>();
    let mut messages = Vec::with_capacity(
        1usize
            .saturating_add(history_message_count)
            .saturating_add(active_messages.len()),
    );
    messages.push(system.clone());
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
    messages.extend_from_slice(active_messages);
    messages
}

async fn execute_call(
    memory: &MemoryService,
    call: &ToolCall,
    turn_id: TurnId,
    event_sender: &mpsc::Sender<Event>,
    cancellation: &CancellationToken,
    interaction: &Interaction,
    timeout: Duration,
) -> Result<ToolResult, ToolHarnessError> {
    let valid = match validate_tool_call(call) {
        Ok(valid) => valid,
        Err(error) => {
            interaction.event(
                "tool.execute",
                "rejected",
                telemetry::fields([
                    ("tool_name", json!(bounded_tool_name(&call.name))),
                    ("failure_class", json!(error.classification())),
                ]),
            );
            return Ok(ToolResult {
                tool_name: call.name.clone(),
                content: tool_error_result(error.classification()),
            });
        }
    };
    let progress = valid.progress();
    send_progress(event_sender, cancellation, turn_id, progress).await?;
    let tool_name = valid.name();
    let stage = interaction.stage(
        "tool.execute",
        telemetry::fields([("tool_name", json!(tool_name))]),
    );
    interaction.event(
        "tool.execute",
        "started",
        telemetry::fields([("tool_name", json!(tool_name))]),
    );
    let _memory_in_flight = interaction.in_flight_guard("memory");
    let memory = memory.clone();
    let queued_at = Instant::now();
    let mut task = tokio::task::spawn_blocking(move || {
        let worker_started = Instant::now();
        let execution = memory.execute(valid);
        (
            worker_started.saturating_duration_since(queued_at),
            execution,
        )
    });
    let timeout_ms = u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX);
    let timeout_sleep = tokio::time::sleep(timeout);
    tokio::pin!(timeout_sleep);
    let (worker_schedule_wait, execution) = tokio::select! {
        biased;
        _ = cancellation.cancelled() => {
            task.abort();
            stage.finish("cancelled", telemetry::Fields::new());
            return Err(ToolHarnessError::Cancelled);
        }
        _ = &mut timeout_sleep => {
            task.abort();
            stage.finish(
                "timeout",
                telemetry::fields([("timeout_ms", json!(timeout_ms))]),
            );
            interaction.event(
                "tool.execute",
                "timeout",
                telemetry::fields([
                    ("tool_name", json!(tool_name)),
                    ("timeout_ms", json!(timeout_ms)),
                ]),
            );
            return Err(ToolHarnessError::ToolExecutionTimedOut { timeout_ms });
        }
        result = &mut task => result.map_err(|_| ToolHarnessError::MemoryWorkerUnavailable)?,
    };
    // Tokio's shared blocking pool does not expose a reliable queue depth.
    // Record its measured scheduling delay separately and reserve queue depth
    // reporting for the actual SQLite mutex gate below.
    interaction.event(
        "memory.worker_schedule_wait",
        "completed",
        telemetry::fields([
            ("wait_ms", json!(worker_schedule_wait.as_millis() as u64)),
            ("pending_depth", json!("unavailable")),
        ]),
    );
    interaction.queue_wait(
        "memory",
        execution.gate_wait,
        execution.pending_depth,
        execution.active_work,
    );
    let stage_fields = telemetry::fields([
        (
            "worker_schedule_wait_ms",
            json!(worker_schedule_wait.as_millis() as u64),
        ),
        (
            "sqlite_gate_wait_ms",
            json!(execution.gate_wait.as_millis() as u64),
        ),
        (
            "sqlite_execution_duration_ms",
            json!(execution.execution_duration.as_millis() as u64),
        ),
    ]);
    match execution.result {
        Ok(value) => {
            let content = bounded_tool_result(value);
            stage.finish("completed", stage_fields);
            interaction.event(
                "tool.execute",
                "completed",
                telemetry::fields([("tool_name", json!(tool_name))]),
            );
            Ok(ToolResult {
                tool_name: tool_name.to_owned(),
                content,
            })
        }
        Err(error) => {
            let classification = error.classification();
            stage.finish("failed", {
                let mut fields = stage_fields;
                fields.insert("failure_class".into(), json!(classification));
                fields
            });
            interaction.event(
                "tool.execute",
                "failed",
                telemetry::fields([
                    ("tool_name", json!(tool_name)),
                    ("failure_class", json!(classification)),
                ]),
            );
            Ok(ToolResult {
                tool_name: tool_name.to_owned(),
                content: tool_error_result(classification),
            })
        }
    }
}

async fn send_progress(
    sender: &mpsc::Sender<Event>,
    cancellation: &CancellationToken,
    id: TurnId,
    progress: ProgressEvent,
) -> Result<(), ToolHarnessError> {
    tokio::select! {
        biased;
        _ = cancellation.cancelled() => Err(ToolHarnessError::Cancelled),
        result = sender.send(Event::Progress { id, progress }) => {
            result.map_err(|_| ToolHarnessError::ProgressChannelClosed)
        }
    }
}

fn memory_tool_definitions() -> Vec<ToolDefinition> {
    vec![
        ToolDefinition::function(
            "remember",
            "Save concise, durable information that is likely to be useful beyond this conversation. Do not save trivial remarks, full transcripts, credentials, or raw media. Include short evidence explaining the source and uncertainty.",
            json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["content", "evidence"],
                "properties": {
                    "content": {"type": "string", "description": "Concise memory; retain uncertainty rather than inventing details."},
                    "confidence": {"type": "number", "minimum": 0.0, "maximum": 1.0},
                    "evidence": {
                        "type": "array",
                        "minItems": 1,
                        "items": {
                            "type": "object",
                            "additionalProperties": false,
                            "required": ["content", "source_type"],
                            "properties": {
                                "content": {"type": "string", "description": "Short supporting evidence, not a full transcript."},
                                "source_type": {"type": "string", "description": "For example user_statement, observation, inference, or correction."},
                                "source_timestamp_ms": {"type": "integer"},
                                "source_reference": {"type": "string", "description": "Optional short source reference when available."}
                            }
                        }
                    },
                    "related_memory_ids": {"type": "array", "items": {"type": "integer", "minimum": 1}}
                }
            }),
        ),
        ToolDefinition::function(
            "recall",
            "Search selected long-term memories when prior information is needed. Do not claim a memory exists unless this tool returns it.",
            json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["query"],
                "properties": {
                    "query": {"type": "string", "description": "Natural-language terms for selected local memories."},
                    "limit": {"type": "integer", "minimum": 1, "maximum": 20}
                }
            }),
        ),
        ToolDefinition::function(
            "correct",
            "Create a corrected current memory while preserving the old memory as superseded history. Recall first when necessary to identify the correct memory ID.",
            json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["memory_id", "content", "evidence"],
                "properties": {
                    "memory_id": {"type": "integer", "minimum": 1},
                    "content": {"type": "string", "description": "The corrected concise memory."},
                    "confidence": {"type": "number", "minimum": 0.0, "maximum": 1.0},
                    "evidence": {
                        "type": "array",
                        "minItems": 1,
                        "items": {
                            "type": "object",
                            "additionalProperties": false,
                            "required": ["content", "source_type"],
                            "properties": {
                                "content": {"type": "string"},
                                "source_type": {"type": "string"},
                                "source_timestamp_ms": {"type": "integer"},
                                "source_reference": {"type": "string"}
                            }
                        }
                    },
                    "related_memory_ids": {"type": "array", "items": {"type": "integer", "minimum": 1}}
                }
            }),
        ),
    ]
}

#[derive(Debug)]
enum ValidToolCall {
    Remember(MemoryInput),
    Recall { query: String, limit: usize },
    Correct(CorrectInput),
}

impl ValidToolCall {
    fn name(&self) -> &'static str {
        match self {
            Self::Remember(_) => "remember",
            Self::Recall { .. } => "recall",
            Self::Correct(_) => "correct",
        }
    }

    fn progress(&self) -> ProgressEvent {
        match self {
            Self::Remember(_) => ProgressEvent::SavingMemory,
            Self::Recall { .. } => ProgressEvent::SearchingMemory,
            Self::Correct(_) => ProgressEvent::UpdatingMemory,
        }
    }
}

#[derive(Debug)]
enum ToolArgumentError {
    UnknownTool,
    MalformedArguments,
    InvalidMemoryId,
}

impl ToolArgumentError {
    fn classification(&self) -> &'static str {
        match self {
            Self::UnknownTool => "unknown_tool",
            Self::MalformedArguments => "malformed_arguments",
            Self::InvalidMemoryId => "invalid_memory_id",
        }
    }
}

#[derive(Debug)]
enum ToolExecutionError {
    Store(#[allow(dead_code)] independence_memory::MemoryError),
    StoreUnavailable,
}

impl ToolExecutionError {
    fn classification(&self) -> &'static str {
        match self {
            Self::Store(_) => "memory_database_error",
            Self::StoreUnavailable => "memory_worker_unavailable",
        }
    }
}

fn validate_tool_call(call: &ToolCall) -> Result<ValidToolCall, ToolArgumentError> {
    if call.arguments_json.len() > MAX_TOOL_ARGUMENT_BYTES {
        return Err(ToolArgumentError::MalformedArguments);
    }
    let arguments = call
        .arguments()
        .map_err(|_| ToolArgumentError::MalformedArguments)?;
    let object = arguments
        .as_object()
        .ok_or(ToolArgumentError::MalformedArguments)?;
    match call.name.as_str() {
        "remember" => Ok(ValidToolCall::Remember(memory_input_from_object(object)?)),
        "recall" => {
            reject_unknown_fields(object, &["query", "limit"])?;
            let query = required_string(object, "query")?;
            let limit = optional_usize(object, "limit")?.unwrap_or(5);
            if !(1..=20).contains(&limit) {
                return Err(ToolArgumentError::MalformedArguments);
            }
            Ok(ValidToolCall::Recall { query, limit })
        }
        "correct" => {
            let memory_id = required_memory_id(object)?;
            let replacement = memory_input_from_object_with_extra(object, &["memory_id"])?;
            Ok(ValidToolCall::Correct(CorrectInput {
                memory_id,
                replacement,
            }))
        }
        _ => Err(ToolArgumentError::UnknownTool),
    }
}

fn memory_input_from_object(
    object: &serde_json::Map<String, Value>,
) -> Result<MemoryInput, ToolArgumentError> {
    memory_input_from_object_with_extra(object, &[])
}

fn memory_input_from_object_with_extra(
    object: &serde_json::Map<String, Value>,
    extra: &[&str],
) -> Result<MemoryInput, ToolArgumentError> {
    let mut allowed = vec!["content", "confidence", "evidence", "related_memory_ids"];
    allowed.extend_from_slice(extra);
    reject_unknown_fields(object, &allowed)?;
    let content = required_string(object, "content")?;
    let confidence = object
        .get("confidence")
        .map(|value| value.as_f64().ok_or(ToolArgumentError::MalformedArguments))
        .transpose()?;
    let evidence = object
        .get("evidence")
        .and_then(Value::as_array)
        .ok_or(ToolArgumentError::MalformedArguments)?
        .iter()
        .map(evidence_from_value)
        .collect::<Result<Vec<_>, _>>()?;
    if evidence.is_empty() {
        return Err(ToolArgumentError::MalformedArguments);
    }
    let related_memories = object
        .get("related_memory_ids")
        .map(|value| {
            value
                .as_array()
                .ok_or(ToolArgumentError::MalformedArguments)?
                .iter()
                .map(|value| {
                    let value = value.as_i64().ok_or(ToolArgumentError::InvalidMemoryId)?;
                    MemoryId::try_from(value).map_err(|_| ToolArgumentError::InvalidMemoryId)
                })
                .collect::<Result<Vec<_>, _>>()
        })
        .transpose()?
        .unwrap_or_default();
    Ok(MemoryInput {
        content,
        confidence,
        evidence,
        related_memories,
    })
}

fn evidence_from_value(value: &Value) -> Result<Evidence, ToolArgumentError> {
    let object = value
        .as_object()
        .ok_or(ToolArgumentError::MalformedArguments)?;
    reject_unknown_fields(
        object,
        &[
            "content",
            "source_type",
            "source_timestamp_ms",
            "source_reference",
        ],
    )?;
    Ok(Evidence {
        content: required_string(object, "content")?,
        source_kind: required_string(object, "source_type")?,
        source_timestamp_ms: object
            .get("source_timestamp_ms")
            .map(|value| value.as_i64().ok_or(ToolArgumentError::MalformedArguments))
            .transpose()?,
        source_reference: object
            .get("source_reference")
            .map(required_string_value)
            .transpose()?,
    })
}

fn required_memory_id(
    object: &serde_json::Map<String, Value>,
) -> Result<MemoryId, ToolArgumentError> {
    let value = object
        .get("memory_id")
        .and_then(Value::as_i64)
        .ok_or(ToolArgumentError::InvalidMemoryId)?;
    MemoryId::try_from(value).map_err(|_| ToolArgumentError::InvalidMemoryId)
}

fn required_string(
    object: &serde_json::Map<String, Value>,
    key: &str,
) -> Result<String, ToolArgumentError> {
    object
        .get(key)
        .ok_or(ToolArgumentError::MalformedArguments)
        .and_then(required_string_value)
}

fn required_string_value(value: &Value) -> Result<String, ToolArgumentError> {
    let value = value
        .as_str()
        .filter(|value| !value.trim().is_empty())
        .ok_or(ToolArgumentError::MalformedArguments)?;
    Ok(value.to_owned())
}

fn optional_usize(
    object: &serde_json::Map<String, Value>,
    key: &str,
) -> Result<Option<usize>, ToolArgumentError> {
    object
        .get(key)
        .map(|value| {
            value
                .as_u64()
                .and_then(|value| usize::try_from(value).ok())
                .ok_or(ToolArgumentError::MalformedArguments)
        })
        .transpose()
}

fn reject_unknown_fields(
    object: &serde_json::Map<String, Value>,
    allowed: &[&str],
) -> Result<(), ToolArgumentError> {
    object
        .keys()
        .all(|key| allowed.contains(&key.as_str()))
        .then_some(())
        .ok_or(ToolArgumentError::MalformedArguments)
}

fn memory_to_value(memory: independence_memory::MemoryRecord) -> Value {
    json!({
        "id": memory.id.value(),
        "content": memory.content,
        "created_at_ms": memory.created_at_ms,
        "updated_at_ms": memory.updated_at_ms,
        "confidence": memory.confidence,
        "status": match memory.status {
            independence_memory::MemoryStatus::Current => "current",
            independence_memory::MemoryStatus::Superseded => "superseded",
        },
        "supersedes_memory_id": memory.supersedes.map(MemoryId::value),
        "evidence": memory.evidence.into_iter().map(|evidence| json!({
            "content": evidence.content,
            "source_type": evidence.source_kind,
            "source_timestamp_ms": evidence.source_timestamp_ms,
            "source_reference": evidence.source_reference,
        })).collect::<Vec<_>>(),
        "relationships": memory.related_memories.into_iter().map(|related| json!({
            "memory_id": related.memory_id.value(),
            "relationship": related.relationship,
        })).collect::<Vec<_>>(),
    })
}

fn tool_error_result(classification: &str) -> String {
    json!({
        "ok": false,
        "error_class": classification,
        "message": "The requested local memory operation did not complete. Correct the tool arguments or answer without claiming it succeeded.",
    })
    .to_string()
}

fn bounded_tool_result(value: Value) -> String {
    let result = value.to_string();
    if result.len() <= MAX_TOOL_RESULT_BYTES {
        return result;
    }
    // This branch is intentionally a reported failure rather than silent
    // truncation. Current recall limits and field caps should keep ordinary
    // results below it.
    tool_error_result("tool_result_too_large")
}

fn bounded_tool_name(name: &str) -> &str {
    match name {
        "remember" | "recall" | "correct" => name,
        _ => "unknown",
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::VecDeque,
        sync::{Arc, Mutex, mpsc as std_mpsc},
        time::Duration,
    };

    use serde_json::json;
    use tempfile::tempdir;
    use tokio::sync::mpsc;
    use wiremock::{
        Mock, MockServer, Request, Respond, ResponseTemplate,
        matchers::{method, path},
    };

    use crate::{
        protocol::{Event, ProgressEvent, TurnId},
        telemetry,
        think::{ConversationTurn, OllamaClient, OllamaConfig, ToolCall},
    };

    use super::{
        MemoryService, ToolArgumentError, ToolExecutionError, ToolLoopConfig, ToolTurnRequest,
        ValidToolCall, execute_call, run, validate_tool_call,
    };

    #[derive(Clone)]
    struct SequentialResponses {
        responses: Arc<Mutex<VecDeque<serde_json::Value>>>,
    }

    impl Respond for SequentialResponses {
        fn respond(&self, _request: &Request) -> ResponseTemplate {
            let response = self
                .responses
                .lock()
                .expect("test response queue lock")
                .pop_front()
                .unwrap_or_else(|| {
                    json!({
                        "error": "the test model received more tool-loop requests than expected"
                    })
                });
            ResponseTemplate::new(200).set_body_json(response)
        }
    }

    fn tool_call_response(name: &str, arguments: serde_json::Value) -> serde_json::Value {
        json!({
            "message": {
                "role": "assistant",
                "content": "",
                "tool_calls": [{"function": {"index": 0, "name": name, "arguments": arguments}}]
            },
            "done": true,
            "done_reason": "stop"
        })
    }

    fn final_response(content: &str) -> serde_json::Value {
        json!({
            "message": {"role": "assistant", "content": content},
            "done": true,
            "done_reason": "stop"
        })
    }

    #[test]
    fn validates_only_the_three_memory_tools_without_exposing_arguments() {
        let remember = ToolCall::from_value(
            Some(0),
            "remember".into(),
            json!({
                "content": "Pi result",
                "evidence": [{"content": "user said so", "source_type": "user_statement"}]
            }),
        );
        assert!(matches!(
            validate_tool_call(&remember),
            Ok(ValidToolCall::Remember(_))
        ));
        let malformed = ToolCall::from_value(Some(1), "remember".into(), json!({"content": "x"}));
        assert!(matches!(
            validate_tool_call(&malformed),
            Err(ToolArgumentError::MalformedArguments)
        ));
        let unknown = ToolCall::from_value(Some(2), "open_shell".into(), json!({}));
        assert!(matches!(
            validate_tool_call(&unknown),
            Err(ToolArgumentError::UnknownTool)
        ));
    }

    #[tokio::test]
    async fn progress_precedes_a_real_memory_execution() {
        let directory = tempdir().unwrap();
        let memory = MemoryService::open(directory.path()).unwrap();
        let (sender, mut receiver) = mpsc::channel(2);
        let interaction = telemetry::start_interaction("test", None);
        let call = ToolCall::from_value(
            Some(0),
            "remember".into(),
            json!({
                "content": "Pi result",
                "evidence": [{"content": "The test said 40 seconds.", "source_type": "user_statement"}]
            }),
        );
        let task = tokio::spawn({
            let interaction = interaction.clone();
            async move {
                execute_call(
                    &memory,
                    &call,
                    TurnId::new(7),
                    &sender,
                    &tokio_util::sync::CancellationToken::new(),
                    &interaction,
                    super::DEFAULT_MEMORY_TOOL_TIMEOUT,
                )
                .await
            }
        });
        assert!(matches!(
            receiver.recv().await,
            Some(Event::Progress {
                progress: ProgressEvent::SavingMemory,
                ..
            })
        ));
        let result = task.await.unwrap().unwrap();
        assert_eq!(result.tool_name, "remember");
        assert!(result.content.contains("\"ok\":true"));
    }

    #[tokio::test]
    async fn failed_memory_operation_is_returned_to_model_as_failure_not_success() {
        let directory = tempdir().unwrap();
        let memory = MemoryService::open(directory.path()).unwrap();
        let (sender, mut receiver) = mpsc::channel(2);
        let interaction = telemetry::start_interaction("test", None);
        let call = ToolCall::from_value(
            Some(0),
            "remember".into(),
            json!({
                "content": "This write references a missing memory.",
                "evidence": [{"content": "test fixture", "source_type": "test"}],
                "related_memory_ids": [999_999]
            }),
        );

        let result = execute_call(
            &memory,
            &call,
            TurnId::new(8),
            &sender,
            &tokio_util::sync::CancellationToken::new(),
            &interaction,
            super::DEFAULT_MEMORY_TOOL_TIMEOUT,
        )
        .await
        .unwrap();
        assert!(matches!(
            receiver.recv().await,
            Some(Event::Progress {
                progress: ProgressEvent::SavingMemory,
                ..
            })
        ));
        assert!(result.content.contains("\"ok\":false"));
        assert!(result.content.contains("memory_database_error"));
        assert!(
            independence_memory::MemoryStore::open(directory.path())
                .unwrap()
                .recall("references missing", 5)
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn a_stalled_memory_gate_times_out_without_reporting_success() {
        let directory = tempdir().unwrap();
        let memory = MemoryService::open(directory.path()).unwrap();
        // Hold the only SQLite connection while the spawned blocking task
        // waits at the actual mutex gate. This exercises the operation timeout
        // without a database error fixture or a live model.
        let (locked_sender, locked_receiver) = std_mpsc::sync_channel(1);
        let (release_sender, release_receiver) = std_mpsc::sync_channel(1);
        let store = memory.store.clone();
        let blocker = std::thread::spawn(move || {
            let _gate = store.lock().expect("test SQLite gate");
            locked_sender.send(()).expect("test gate notification");
            let _ = release_receiver.recv();
        });
        locked_receiver.recv().expect("test gate is held");
        let (sender, mut receiver) = mpsc::channel(2);
        let interaction = telemetry::start_interaction("test", None);
        let call = ToolCall::from_value(
            Some(0),
            "remember".into(),
            json!({
                "content": "This timeout fixture must not be reported as saved.",
                "evidence": [{"content": "test fixture", "source_type": "test"}]
            }),
        );
        let task = tokio::spawn({
            let memory = memory.clone();
            let interaction = interaction.clone();
            async move {
                execute_call(
                    &memory,
                    &call,
                    TurnId::new(9),
                    &sender,
                    &tokio_util::sync::CancellationToken::new(),
                    &interaction,
                    Duration::from_millis(20),
                )
                .await
            }
        });
        assert!(matches!(
            receiver.recv().await,
            Some(Event::Progress {
                progress: ProgressEvent::SavingMemory,
                ..
            })
        ));
        let result = task.await.unwrap();
        assert!(matches!(
            result,
            Err(super::ToolHarnessError::ToolExecutionTimedOut { .. })
        ));
        release_sender.send(()).expect("release test gate");
        blocker.join().expect("test gate thread joins");
        // A blocking task that had already started may finish after the caller
        // timed out. It remains unable to send a success result to this turn.
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    #[tokio::test]
    async fn actual_ollama_protocol_continues_after_memory_tool_result() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(SequentialResponses {
                responses: Arc::new(Mutex::new(VecDeque::from([
                    json!({
                        "message": {
                            "role": "assistant",
                            "content": "",
                            "tool_calls": [{
                                "function": {
                                    "index": 0,
                                    "name": "remember",
                                    "arguments": {
                                        "content": "A Raspberry Pi 5 test with a 4B model took 40 seconds.",
                                        "evidence": [{
                                            "content": "The user reported the measurement and later clarified the model size.",
                                            "source_type": "user_statement"
                                        }]
                                    }
                                }
                            }]
                        },
                        "done": true,
                        "done_reason": "stop"
                    }),
                    json!({
                        "message": {
                            "role": "assistant",
                            "content": "I saved that experiment result."
                        },
                        "done": true,
                        "done_reason": "stop"
                    }),
                ]))),
            })
            .mount(&server)
            .await;

        let client =
            OllamaClient::new(OllamaConfig::new(&server.uri(), "tool-test").unwrap()).unwrap();
        client.set_capabilities_for_test(&["tools"]);
        let directory = tempdir().unwrap();
        let memory = MemoryService::open(directory.path()).unwrap();
        let (sender, mut receiver) = mpsc::channel(8);
        let interaction = telemetry::start_interaction("test", None);
        let output = run(
            &client,
            Some(memory),
            ToolTurnRequest {
                history: Vec::new(),
                question: "Remember the Pi measurement.".into(),
                image_bytes: &[],
                turn_id: TurnId::new(1),
            },
            ToolLoopConfig::default(),
            sender,
            tokio_util::sync::CancellationToken::new(),
            &interaction,
        )
        .await
        .unwrap();

        assert_eq!(output.answer, "I saved that experiment result.");
        assert_eq!(output.tool_rounds.len(), 1);
        assert!(matches!(
            receiver.recv().await,
            Some(Event::Progress {
                progress: ProgressEvent::SavingMemory,
                ..
            })
        ));

        let persisted = independence_memory::MemoryStore::open(directory.path())
            .unwrap()
            .recall("Raspberry Pi model", 5)
            .unwrap();
        assert_eq!(persisted.len(), 1);
        assert!(persisted[0].content.contains("40 seconds"));

        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 2);
        let first: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
        assert_eq!(first["tools"].as_array().unwrap().len(), 3);
        let second: serde_json::Value = serde_json::from_slice(&requests[1].body).unwrap();
        let messages = second["messages"].as_array().unwrap();
        assert!(
            messages.iter().any(|message| {
                message["role"] == "assistant" && message["tool_calls"].is_array()
            })
        );
        assert!(messages.iter().any(|message| {
            message["role"] == "tool"
                && message["content"]
                    .as_str()
                    .is_some_and(|content| content.contains("\"operation\":\"remember\""))
        }));
    }

    #[tokio::test]
    async fn multiple_tool_calls_preserve_order_and_emit_progress_for_each() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(SequentialResponses {
                responses: Arc::new(Mutex::new(VecDeque::from([
                    json!({
                        "message": {"role": "assistant", "tool_calls": [
                            {"function": {"index": 0, "name": "recall", "arguments": {"query": "Pi test", "limit": 1}}},
                            {"function": {"index": 1, "name": "recall", "arguments": {"query": "garden", "limit": 1}}}
                        ]},
                        "done": true,
                        "done_reason": "stop"
                    }),
                    json!({
                        "message": {"role": "assistant", "content": "I found both selected memories."},
                        "done": true,
                        "done_reason": "stop"
                    }),
                ]))),
            })
            .mount(&server)
            .await;
        let directory = tempdir().unwrap();
        let mut seed = independence_memory::MemoryStore::open(directory.path()).unwrap();
        seed.remember(independence_memory::MemoryInput::new(
            "Pi test used a 4B model.",
        ))
        .unwrap();
        seed.remember(independence_memory::MemoryInput::new("Garden observation."))
            .unwrap();
        drop(seed);
        let client =
            OllamaClient::new(OllamaConfig::new(&server.uri(), "tool-test").unwrap()).unwrap();
        client.set_capabilities_for_test(&["tools"]);
        let (sender, mut receiver) = mpsc::channel(8);
        let interaction = telemetry::start_interaction("test", None);
        let output = run(
            &client,
            Some(MemoryService::open(directory.path()).unwrap()),
            ToolTurnRequest {
                history: Vec::new(),
                question: "Find the two memories.".into(),
                image_bytes: &[],
                turn_id: TurnId::new(2),
            },
            ToolLoopConfig::default(),
            sender,
            tokio_util::sync::CancellationToken::new(),
            &interaction,
        )
        .await
        .unwrap();

        assert_eq!(output.tool_rounds.len(), 1);
        assert_eq!(output.tool_rounds[0].calls.len(), 2);
        assert_eq!(output.tool_rounds[0].results.len(), 2);
        assert_eq!(
            output.tool_rounds[0]
                .results
                .iter()
                .map(|result| result.tool_name.as_str())
                .collect::<Vec<_>>(),
            vec!["recall", "recall"]
        );
        assert!(matches!(
            receiver.recv().await,
            Some(Event::Progress {
                progress: ProgressEvent::SearchingMemory,
                ..
            })
        ));
        assert!(matches!(
            receiver.recv().await,
            Some(Event::Progress {
                progress: ProgressEvent::SearchingMemory,
                ..
            })
        ));
    }

    #[tokio::test]
    async fn bounded_loop_and_pre_cancelled_turn_fail_without_accepting_a_response() {
        let server = MockServer::start().await;
        let repeated_recall = json!({
            "message": {"role": "assistant", "tool_calls": [{
                "function": {"name": "recall", "arguments": {"query": "Pi"}}
            }]},
            "done": true,
            "done_reason": "stop"
        });
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(SequentialResponses {
                responses: Arc::new(Mutex::new(VecDeque::from([
                    repeated_recall.clone(),
                    repeated_recall,
                ]))),
            })
            .mount(&server)
            .await;
        let client =
            OllamaClient::new(OllamaConfig::new(&server.uri(), "tool-test").unwrap()).unwrap();
        client.set_capabilities_for_test(&["tools"]);
        let directory = tempdir().unwrap();
        let (sender, _receiver) = mpsc::channel(8);
        let interaction = telemetry::start_interaction("test", None);
        let error = run(
            &client,
            Some(MemoryService::open(directory.path()).unwrap()),
            ToolTurnRequest {
                history: Vec::new(),
                question: "Search twice.".into(),
                image_bytes: &[],
                turn_id: TurnId::new(3),
            },
            ToolLoopConfig {
                max_iterations: 1,
                ..ToolLoopConfig::default()
            },
            sender,
            tokio_util::sync::CancellationToken::new(),
            &interaction,
        )
        .await
        .unwrap_err();
        assert!(matches!(
            error,
            super::ToolHarnessError::ToolIterationLimit { maximum: 1 }
        ));

        let cancellation = tokio_util::sync::CancellationToken::new();
        cancellation.cancel();
        let (sender, _receiver) = mpsc::channel(1);
        let cancelled = run(
            &client,
            None,
            ToolTurnRequest {
                history: Vec::new(),
                question: "This must not be sent.".into(),
                image_bytes: &[],
                turn_id: TurnId::new(4),
            },
            ToolLoopConfig::default(),
            sender,
            cancellation,
            &interaction,
        )
        .await;
        assert!(matches!(cancelled, Err(super::ToolHarnessError::Cancelled)));
        assert_eq!(server.received_requests().await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn end_to_end_memory_persists_correction_and_retrieval_across_sessions() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(SequentialResponses {
                responses: Arc::new(Mutex::new(VecDeque::from([
                    tool_call_response(
                        "remember",
                        json!({
                            "content": "A Raspberry Pi 5 test with the 8B model took 40 seconds to respond.",
                            "evidence": [{
                                "content": "The user described the Pi 5 measurement.",
                                "source_type": "user_statement"
                            }]
                        }),
                    ),
                    final_response("I saved the experiment result."),
                    tool_call_response(
                        "correct",
                        json!({
                            "memory_id": 1,
                            "content": "A Raspberry Pi 5 test with the 4B model took 40 seconds to respond.",
                            "evidence": [{
                                "content": "The user corrected the model size from 8B to 4B.",
                                "source_type": "correction"
                            }]
                        }),
                    ),
                    final_response("I updated that experiment result."),
                    tool_call_response("recall", json!({"query": "Raspberry Pi model", "limit": 5})),
                    final_response("The Raspberry Pi 5 test used the 4B model and took 40 seconds."),
                ]))),
            })
            .mount(&server)
            .await;
        let directory = tempdir().unwrap();
        let first_client =
            OllamaClient::new(OllamaConfig::new(&server.uri(), "tool-test").unwrap()).unwrap();
        first_client.set_capabilities_for_test(&["tools"]);
        let (first_sender, _first_progress) = mpsc::channel(8);
        let first_interaction = telemetry::start_interaction("test", None);
        let first_question =
            "I tested Model A on Raspberry Pi 5. It took 40 seconds to produce a response.";
        let first = run(
            &first_client,
            Some(MemoryService::open(directory.path()).unwrap()),
            ToolTurnRequest {
                history: Vec::new(),
                question: first_question.into(),
                image_bytes: &[],
                turn_id: TurnId::new(10),
            },
            ToolLoopConfig::default(),
            first_sender,
            tokio_util::sync::CancellationToken::new(),
            &first_interaction,
        )
        .await
        .unwrap();
        assert_eq!(first.answer, "I saved the experiment result.");

        let (correction_sender, _correction_progress) = mpsc::channel(8);
        let correction_interaction = telemetry::start_interaction("test", None);
        let correction = run(
            &first_client,
            Some(MemoryService::open(directory.path()).unwrap()),
            ToolTurnRequest {
                history: vec![ConversationTurn {
                    user: first_question.into(),
                    tool_rounds: first.tool_rounds.clone(),
                    assistant: first.answer,
                }],
                question: "That was with the 4B model, not the 8B model.".into(),
                image_bytes: &[],
                turn_id: TurnId::new(11),
            },
            ToolLoopConfig::default(),
            correction_sender,
            tokio_util::sync::CancellationToken::new(),
            &correction_interaction,
        )
        .await
        .unwrap();
        assert_eq!(correction.answer, "I updated that experiment result.");

        // A fresh client and an empty history model a complete application
        // restart. Only the SQLite database bridges the two sessions.
        let second_client =
            OllamaClient::new(OllamaConfig::new(&server.uri(), "tool-test").unwrap()).unwrap();
        second_client.set_capabilities_for_test(&["tools"]);
        let (second_sender, _second_progress) = mpsc::channel(8);
        let second_interaction = telemetry::start_interaction("test", None);
        let second = run(
            &second_client,
            Some(MemoryService::open(directory.path()).unwrap()),
            ToolTurnRequest {
                history: Vec::new(),
                question: "What did we learn when we tested the model on the Pi?".into(),
                image_bytes: &[],
                turn_id: TurnId::new(12),
            },
            ToolLoopConfig::default(),
            second_sender,
            tokio_util::sync::CancellationToken::new(),
            &second_interaction,
        )
        .await
        .unwrap();
        assert_eq!(
            second.answer,
            "The Raspberry Pi 5 test used the 4B model and took 40 seconds."
        );

        let store = independence_memory::MemoryStore::open(directory.path()).unwrap();
        let current = store.recall("Raspberry Pi test model", 5).unwrap();
        assert_eq!(current.len(), 1);
        assert!(current[0].content.contains("4B"));
        assert!(current[0].content.contains("40 seconds"));

        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 6);
        let correction_request: serde_json::Value =
            serde_json::from_slice(&requests[2].body).unwrap();
        assert!(
            correction_request["messages"]
                .as_array()
                .unwrap()
                .iter()
                .any(|message| message["role"] == "tool")
        );
        let retrieval_result_request: serde_json::Value =
            serde_json::from_slice(&requests[5].body).unwrap();
        assert!(
            retrieval_result_request["messages"]
                .as_array()
                .unwrap()
                .iter()
                .any(|message| {
                    message["role"] == "tool"
                        && message["content"]
                            .as_str()
                            .is_some_and(|content| content.contains("4B"))
                })
        );
    }

    #[test]
    fn errors_have_stable_classes_without_memory_content() {
        assert_eq!(
            ToolExecutionError::StoreUnavailable.classification(),
            "memory_worker_unavailable"
        );
    }
}
