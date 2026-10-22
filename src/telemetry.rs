//! Privacy-preserving observability for Independence.
//!
//! Terminal dialogue remains a readable `tracing` stream. This module is a
//! separate path for redacted machine-readable records, OpenTelemetry spans,
//! and metrics. It deliberately accepts identifiers, sizes, durations, and
//! outcomes rather than transcripts, answers, audio, image bytes, or prompts.

use std::{
    collections::BTreeMap,
    env,
    fs::OpenOptions,
    io::{self, BufWriter, Write},
    path::PathBuf,
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::{self, Receiver, SyncSender, TrySendError},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use opentelemetry::{
    KeyValue, global,
    metrics::{Counter, Histogram, Meter, MeterProvider, UpDownCounter},
    trace::TraceContextExt,
};
use opentelemetry_otlp::{MetricExporter, SpanExporter};
use opentelemetry_sdk::{Resource, metrics::SdkMeterProvider, trace::SdkTracerProvider};
use serde::Serialize;
use serde_json::{Value, json};
use tracing::{Span, info, warn};
use tracing_opentelemetry::OpenTelemetrySpanExt;

use crate::protocol::{RecordingId, TurnId};

/// Version of the independently documented local JSONL schema.
pub const JSONL_SCHEMA_VERSION: u16 = 1;

const JSONL_QUEUE_CAPACITY: usize = 256;
const MAX_STAGE_RECORDS: usize = 64;
const TELEMETRY_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);

static GLOBAL: OnceLock<Arc<TelemetryCore>> = OnceLock::new();

/// Optional measurements reported by one completed non-streaming Ollama
/// response. They remain a single record so callers cannot accidentally mix
/// statistics from different responses.
pub struct OllamaMetricData<'a> {
    pub model: &'a str,
    pub total_duration_ns: Option<u64>,
    pub load_duration_ns: Option<u64>,
    pub prompt_eval_duration_ns: Option<u64>,
    pub eval_duration_ns: Option<u64>,
    pub prompt_eval_count: Option<u64>,
    pub prompt_eval_cached_count: Option<u64>,
    pub eval_count: Option<u64>,
}

/// Owns the optional JSONL worker and OpenTelemetry providers for a process.
///
/// The shared [`TelemetryCore`] is intentionally global because workers are
/// created in several capability modules. The runtime itself remains owned by
/// `main`, which gives shutdown one bounded place to flush app-owned work.
pub struct TelemetryRuntime {
    core: Arc<TelemetryCore>,
    jsonl: Option<JsonlWorker>,
    tracer_provider: SdkTracerProvider,
    meter_provider: SdkMeterProvider,
    startup_warnings: Vec<String>,
}

/// Initializes local telemetry. It never contacts an exporter unless an OTLP
/// endpoint is explicitly configured through standard `OTEL_EXPORTER_OTLP_*`
/// variables. A JSONL path is opt-in through `INDEPENDENCE_TELEMETRY_JSONL`.
pub fn initialize() -> TelemetryRuntime {
    let config = TelemetryConfig::from_environment();
    let (tracer_provider, meter_provider, mut startup_warnings) = build_providers(&config);
    global::set_tracer_provider(tracer_provider.clone());
    global::set_meter_provider(meter_provider.clone());

    let jsonl_dropped = Arc::new(AtomicU64::new(0));
    let (jsonl_sender, jsonl) = match config.jsonl_path {
        Some(path) => match JsonlWorker::start(path, jsonl_dropped.clone()) {
            Ok(worker) => (Some(worker.sender.clone()), Some(worker)),
            Err(error) => {
                startup_warnings.push(format!(
                    "telemetry JSONL was disabled because its configured file could not be opened: {error}"
                ));
                (None, None)
            }
        },
        None => (None, None),
    };

    let meter = meter_provider.meter("independence");
    let core = Arc::new(TelemetryCore {
        run_id: run_id(),
        next_interaction_id: AtomicU64::new(1),
        jsonl_sender,
        jsonl_dropped,
        metrics: Metrics::new(meter),
    });

    let _ = GLOBAL.set(core.clone());
    TelemetryRuntime {
        core,
        jsonl,
        tracer_provider,
        meter_provider,
        startup_warnings,
    }
}

impl TelemetryRuntime {
    /// Emits startup diagnostics only after the normal terminal logger exists.
    pub fn report_startup(&mut self) {
        for warning_message in self.startup_warnings.drain(..) {
            warn!(target: "telemetry", event = "startup_warning", detail = %warning_message, "telemetry");
        }
        let exporter = if otlp_endpoint_configured() {
            "configured"
        } else {
            "disabled"
        };
        info!(
            target: "telemetry",
            event = "initialized",
            run_id = %self.core.run_id,
            jsonl = self.jsonl.is_some(),
            otlp = exporter,
            schema_version = JSONL_SCHEMA_VERSION,
            "telemetry"
        );
        self.core.emit_run_event(
            "telemetry.initialization",
            "completed",
            fields([
                ("jsonl_enabled", json!(self.jsonl.is_some())),
                ("otlp_configured", json!(exporter == "configured")),
                ("schema_version", json!(JSONL_SCHEMA_VERSION)),
            ]),
        );
    }

    /// Flushes local telemetry without allowing exporter trouble to hold up the
    /// application indefinitely. The OTLP SDK owns its bounded batch queues.
    pub fn shutdown(mut self) {
        if let Some(worker) = self.jsonl.take() {
            worker.shutdown();
        }
        let dropped = self.core.jsonl_dropped.load(Ordering::Acquire);
        if dropped != 0 {
            warn!(
                target: "telemetry",
                event = "jsonl_records_dropped",
                count = dropped,
                "telemetry"
            );
        }

        if let Err(error) = self
            .tracer_provider
            .shutdown_with_timeout(TELEMETRY_SHUTDOWN_TIMEOUT)
        {
            warn!(target: "telemetry", event = "trace_flush_failed", error = %error, "telemetry");
        }

        // The SDK's metric provider does not expose a timeout-aware shutdown
        // API. It owns only optional periodic export; force_flush is attempted
        // in a detached helper so process shutdown stays bounded.
        let meter_provider = self.meter_provider.clone();
        let (done_sender, done_receiver) = mpsc::sync_channel(1);
        let _ = thread::Builder::new()
            .name("independence-telemetry-metric-flush".into())
            .spawn(move || {
                let result = meter_provider.force_flush();
                let _ = meter_provider.shutdown();
                let _ = done_sender.send(result);
            });
        match done_receiver.recv_timeout(TELEMETRY_SHUTDOWN_TIMEOUT) {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                warn!(target: "telemetry", event = "metric_flush_failed", error = %error, "telemetry")
            }
            Err(_) => warn!(target: "telemetry", event = "metric_flush_timed_out", "telemetry"),
        }
    }
}

/// Starts a privacy-preserving interaction timeline. Voice mode calls this at
/// Enter press, before a `TurnId` exists; typed and camera requests call it at
/// their dispatch boundary.
pub fn start_interaction(mode: &'static str, draft_id: Option<u64>) -> Interaction {
    global_core().start_interaction(mode, draft_id)
}

/// Emits a run-level event that has no user interaction yet, such as startup
/// metadata probing. The attributes must already be redacted.
pub fn run_event(operation: &'static str, outcome: &'static str, attributes: Fields) {
    global_core().emit_run_event(operation, outcome, attributes);
}

fn global_core() -> Arc<TelemetryCore> {
    GLOBAL
        .get_or_init(|| {
            // Library tests can exercise a capability without entering the
            // binary's main function. Keep that path fully local and exporter
            // free instead of requiring a global initialization order.
            let tracer_provider = SdkTracerProvider::builder().build();
            let meter_provider = SdkMeterProvider::builder().build();
            global::set_tracer_provider(tracer_provider);
            global::set_meter_provider(meter_provider.clone());
            Arc::new(TelemetryCore {
                run_id: run_id(),
                next_interaction_id: AtomicU64::new(1),
                jsonl_sender: None,
                jsonl_dropped: Arc::new(AtomicU64::new(0)),
                metrics: Metrics::new(meter_provider.meter("independence")),
            })
        })
        .clone()
}

#[derive(Clone)]
pub struct Interaction {
    inner: Arc<InteractionInner>,
}

struct InteractionInner {
    core: Arc<TelemetryCore>,
    id: u64,
    mode: &'static str,
    root_span: Span,
    started: Instant,
    state: Mutex<InteractionState>,
    completed: AtomicBool,
}

#[derive(Default)]
struct InteractionState {
    draft_id: Option<u64>,
    turn_id: Option<u64>,
    recording_id: Option<u64>,
    photo_count: u64,
    input: Option<Fields>,
    ollama: Option<Fields>,
    milestones_ms: BTreeMap<String, u64>,
    stages: Vec<StageSummary>,
    stages_truncated: bool,
    unavailable: BTreeMap<String, String>,
}

#[derive(Clone, Serialize)]
struct StageSummary {
    operation: String,
    outcome: String,
    started_after_interaction_ms: u64,
    duration_ms: u64,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    attributes: Fields,
}

/// A running, explicitly named application operation. Constructed around
/// worker scheduling and execution boundaries, never in an audio callback.
pub struct Stage {
    interaction: Interaction,
    span: Span,
    operation: &'static str,
    started: Instant,
    started_after_interaction: Duration,
    attributes: Fields,
    completed: bool,
}

/// Balances an application-owned in-flight gauge even when a worker returns
/// early or unwinds. It deliberately represents only Independence work, not
/// an unobservable queue inside Ollama or an audio driver.
pub struct InFlightGuard {
    interaction: Interaction,
    component: &'static str,
}

impl Interaction {
    fn new(core: Arc<TelemetryCore>, mode: &'static str, draft_id: Option<u64>) -> Self {
        let id = core.next_interaction_id.fetch_add(1, Ordering::Relaxed);
        let root_span = tracing::info_span!(
            target: "telemetry",
            "independence.interaction",
            otel.kind = "internal",
            "independence.run_id" = %core.run_id,
            "independence.interaction_id" = id,
            "independence.mode" = mode,
            "independence.draft_id" = ?draft_id,
            "independence.outcome" = tracing::field::Empty,
            "independence.duration_ms" = tracing::field::Empty,
        );
        let interaction = Self {
            inner: Arc::new(InteractionInner {
                core,
                id,
                mode,
                root_span,
                started: Instant::now(),
                state: Mutex::new(InteractionState {
                    draft_id,
                    ..InteractionState::default()
                }),
                completed: AtomicBool::new(false),
            }),
        };
        interaction.event("independence.interaction.started", "started", Fields::new());
        interaction
    }

    pub fn id(&self) -> u64 {
        self.inner.id
    }

    pub fn mode(&self) -> &'static str {
        self.inner.mode
    }

    pub fn bind_turn(&self, turn_id: TurnId) {
        self.with_state(|state| state.turn_id = Some(turn_id.value()));
        self.event(
            "independence.turn.bound",
            "bound",
            fields([("turn_id", json!(turn_id.value()))]),
        );
    }

    pub fn bind_recording(&self, recording_id: RecordingId) {
        self.with_state(|state| state.recording_id = Some(recording_id.value()));
        self.event(
            "independence.recording.bound",
            "bound",
            fields([("recording_id", json!(recording_id.value()))]),
        );
    }

    pub fn set_photo_count(&self, photo_count: usize) {
        self.with_state(|state| state.photo_count = photo_count as u64);
    }

    pub fn set_input(&self, input: Fields) {
        let input = redact_fields(input);
        set_span_attributes(&self.inner.root_span, &input);
        self.with_state(|state| state.input = Some(input.clone()));
        self.event("independence.request.prepared", "prepared", input);
    }

    pub fn set_ollama(&self, ollama: Fields) {
        let ollama = redact_fields(ollama);
        set_span_attributes(&self.inner.root_span, &ollama);
        self.with_state(|state| state.ollama = Some(ollama));
    }

    /// Records optional server-provided Ollama measurements as OpenTelemetry
    /// metrics. `None` remains absent: no zero is emitted for unknown values.
    pub fn record_ollama_statistics(&self, statistics: OllamaMetricData<'_>) {
        self.inner
            .core
            .metrics
            .record_ollama_statistics(&statistics);
    }

    pub fn mark(&self, milestone: &'static str) {
        let elapsed = self.inner.started.elapsed();
        self.with_state(|state| {
            state
                .milestones_ms
                .insert(milestone.to_owned(), duration_ms(elapsed));
        });
        self.event(
            milestone,
            "observed",
            fields([("after_interaction_ms", json!(duration_ms(elapsed)))]),
        );
    }

    pub fn unavailable(&self, field: &'static str, reason: impl Into<String>) {
        let reason = reason.into();
        self.with_state(|state| {
            state.unavailable.insert(field.to_owned(), reason.clone());
        });
    }

    pub fn event(&self, operation: &'static str, outcome: &'static str, attributes: Fields) {
        let attributes = redact_fields(attributes);
        let correlation = self.correlation(&self.inner.root_span);
        self.inner.core.emit_record(JsonRecord::Event {
            schema_version: JSONL_SCHEMA_VERSION,
            timestamp_utc_unix_ms: utc_millis(),
            run_id: self.inner.core.run_id.clone(),
            interaction_id: self.inner.id,
            trace_id: correlation.trace_id,
            span_id: correlation.span_id,
            mode: self.inner.mode,
            operation,
            outcome,
            attributes: redact_fields(attributes),
        });
        let _entered = self.inner.root_span.enter();
        info!(
            target: "telemetry",
            event = operation,
            outcome,
            interaction_id = self.inner.id,
            "telemetry event"
        );
    }

    pub fn stage(&self, operation: &'static str, attributes: Fields) -> Stage {
        let span = tracing::info_span!(
            target: "telemetry",
            parent: &self.inner.root_span,
            "independence.operation",
            otel.kind = "internal",
            "independence.operation" = operation,
            "independence.interaction_id" = self.inner.id,
            "independence.mode" = self.inner.mode,
            "independence.outcome" = tracing::field::Empty,
            "independence.duration_ms" = tracing::field::Empty,
        );
        let attributes = redact_fields(attributes);
        set_span_attributes(&span, &attributes);
        Stage {
            interaction: self.clone(),
            span,
            operation,
            started: Instant::now(),
            started_after_interaction: self.inner.started.elapsed(),
            attributes,
            completed: false,
        }
    }

    pub fn queue_wait(
        &self,
        component: &'static str,
        wait: Duration,
        pending_depth: usize,
        active: usize,
    ) {
        let attributes = fields([
            ("component", json!(component)),
            ("wait_reason", json!(queue_wait_reason(component))),
            ("queue_wait_ms", json!(duration_ms(wait))),
            ("pending_depth", json!(pending_depth)),
            ("active_work", json!(active)),
        ]);
        self.event("independence.queue.wait", "completed", attributes.clone());
        self.inner
            .core
            .metrics
            .record_queue_wait(component, self.inner.mode, wait);
        self.inner.core.metrics.record_queue_state(
            component,
            self.inner.mode,
            pending_depth,
            active,
        );
    }

    /// Records a sampled local queue state without creating a queue or
    /// implying anything about a server-side scheduler. Callers already own
    /// their real queue/gate; this only exports its observed state.
    pub fn queue_state(&self, component: &'static str, pending_depth: usize, active: usize) {
        self.inner.core.metrics.record_queue_state(
            component,
            self.inner.mode,
            pending_depth,
            active,
        );
    }

    pub fn in_flight(&self, component: &'static str, delta: i64) {
        self.inner
            .core
            .metrics
            .record_in_flight(component, self.inner.mode, delta);
    }

    pub fn in_flight_guard(&self, component: &'static str) -> InFlightGuard {
        self.in_flight(component, 1);
        InFlightGuard {
            interaction: self.clone(),
            component,
        }
    }

    /// Writes the one completion summary. An accepted answer can remain open
    /// until answer playback reports its own outcome; cancellation/failure is
    /// otherwise recorded immediately.
    pub fn complete(&self, outcome: &'static str, attributes: Fields) {
        if self.inner.completed.swap(true, Ordering::AcqRel) {
            return;
        }

        let elapsed = self.inner.started.elapsed();
        self.inner.root_span.record("independence.outcome", outcome);
        self.inner
            .root_span
            .record("independence.duration_ms", duration_ms(elapsed));
        let state = self
            .inner
            .state
            .lock()
            .expect("telemetry interaction state lock was poisoned")
            .summary(elapsed);
        let correlation = self.correlation(&self.inner.root_span);
        self.inner.core.metrics.record_operation(
            "independence.interaction",
            self.inner.mode,
            outcome,
            elapsed,
        );
        self.inner.core.emit_record(JsonRecord::InteractionSummary {
            schema_version: JSONL_SCHEMA_VERSION,
            timestamp_utc_unix_ms: utc_millis(),
            run_id: self.inner.core.run_id.clone(),
            interaction_id: self.inner.id,
            trace_id: correlation.trace_id,
            span_id: correlation.span_id,
            mode: self.inner.mode,
            outcome,
            duration_ms: duration_ms(elapsed),
            attributes: redact_fields(attributes),
            state: Box::new(state),
            telemetry_jsonl_dropped_records: self.inner.core.jsonl_dropped.load(Ordering::Acquire),
        });
    }

    fn correlation(&self, span: &Span) -> Correlation {
        let context = span.context();
        let span = context.span();
        let span_context = span.span_context();
        let trace_id = if span_context.is_valid() {
            span_context.trace_id().to_string()
        } else {
            "unavailable".into()
        };
        let span_id = if span_context.is_valid() {
            span_context.span_id().to_string()
        } else {
            "unavailable".into()
        };
        Correlation { trace_id, span_id }
    }

    fn with_state(&self, update: impl FnOnce(&mut InteractionState)) {
        let mut state = self
            .inner
            .state
            .lock()
            .expect("telemetry interaction state lock was poisoned");
        update(&mut state);
    }
}

impl Stage {
    pub fn finish(mut self, outcome: &'static str, extra: Fields) {
        if self.completed {
            return;
        }
        self.completed = true;
        self.attributes.extend(redact_fields(extra));
        let elapsed = self.started.elapsed();
        let elapsed_ms = duration_ms(elapsed);
        self.span.record("independence.outcome", outcome);
        self.span.record("independence.duration_ms", elapsed_ms);
        self.attributes
            .insert("duration_ms".into(), json!(elapsed_ms));
        set_span_attributes(&self.span, &self.attributes);
        let correlation = self.interaction.correlation(&self.span);
        self.interaction.inner.core.emit_record(JsonRecord::Event {
            schema_version: JSONL_SCHEMA_VERSION,
            timestamp_utc_unix_ms: utc_millis(),
            run_id: self.interaction.inner.core.run_id.clone(),
            interaction_id: self.interaction.inner.id,
            trace_id: correlation.trace_id,
            span_id: correlation.span_id,
            mode: self.interaction.inner.mode,
            operation: self.operation,
            outcome,
            attributes: redact_fields(self.attributes.clone()),
        });
        self.interaction.with_state(|state| {
            if state.stages.len() < MAX_STAGE_RECORDS {
                state.stages.push(StageSummary {
                    operation: self.operation.to_owned(),
                    outcome: outcome.to_owned(),
                    started_after_interaction_ms: duration_ms(self.started_after_interaction),
                    duration_ms: elapsed_ms,
                    attributes: self.attributes.clone(),
                });
            } else {
                state.stages_truncated = true;
            }
        });
        self.interaction.inner.core.metrics.record_operation(
            self.operation,
            self.interaction.inner.mode,
            outcome,
            elapsed,
        );
        let _entered = self.span.enter();
        info!(
            target: "telemetry",
            event = "operation_finished",
            operation = self.operation,
            outcome,
            duration_ms = elapsed_ms,
            interaction_id = self.interaction.id(),
            "telemetry"
        );
    }
}

impl Drop for Stage {
    fn drop(&mut self) {
        if !self.completed {
            // This is intentionally visible rather than silently classifying
            // an early-return as success. It also enters the same summary
            // timeline as a finished stage, so cancellation/failure retains
            // timings from work that started before a later boundary failed.
            self.completed = true;
            let elapsed = self.started.elapsed();
            let elapsed_ms = duration_ms(elapsed);
            self.span.record("independence.outcome", "abandoned");
            self.span.record("independence.duration_ms", elapsed_ms);
            self.attributes
                .insert("duration_ms".into(), json!(elapsed_ms));
            set_span_attributes(&self.span, &self.attributes);
            let correlation = self.interaction.correlation(&self.span);
            self.interaction.inner.core.emit_record(JsonRecord::Event {
                schema_version: JSONL_SCHEMA_VERSION,
                timestamp_utc_unix_ms: utc_millis(),
                run_id: self.interaction.inner.core.run_id.clone(),
                interaction_id: self.interaction.inner.id,
                trace_id: correlation.trace_id,
                span_id: correlation.span_id,
                mode: self.interaction.inner.mode,
                operation: self.operation,
                outcome: "abandoned",
                attributes: redact_fields(self.attributes.clone()),
            });
            self.interaction.with_state(|state| {
                if state.stages.len() < MAX_STAGE_RECORDS {
                    state.stages.push(StageSummary {
                        operation: self.operation.to_owned(),
                        outcome: "abandoned".into(),
                        started_after_interaction_ms: duration_ms(self.started_after_interaction),
                        duration_ms: elapsed_ms,
                        attributes: self.attributes.clone(),
                    });
                } else {
                    state.stages_truncated = true;
                }
            });
            self.interaction.inner.core.metrics.record_operation(
                self.operation,
                self.interaction.inner.mode,
                "abandoned",
                elapsed,
            );
            let _entered = self.span.enter();
            info!(
                target: "telemetry",
                event = "operation_abandoned",
                operation = self.operation,
                duration_ms = elapsed_ms,
                interaction_id = self.interaction.id(),
                "telemetry"
            );
        }
    }
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        self.interaction.in_flight(self.component, -1);
    }
}

impl InteractionState {
    fn summary(&self, interaction_duration: Duration) -> SummaryState {
        let interval = |start: &'static str, end: &'static str| {
            self.milestones_ms
                .get(start)
                .zip(self.milestones_ms.get(end))
                .map(|(start, end)| end.saturating_sub(*start))
        };
        let mut explicit_intervals_ms = BTreeMap::new();
        insert_interval(
            &mut explicit_intervals_ms,
            "enter_press_to_microphone_recording",
            interval("voice.enter_pressed", "microphone.recording_started"),
        );
        insert_interval(
            &mut explicit_intervals_ms,
            "enter_release_to_request_dispatched",
            interval("voice.enter_released", "turn.request_dispatched"),
        );
        insert_interval(
            &mut explicit_intervals_ms,
            "enter_release_to_accepted_text_answer",
            interval("voice.enter_released", "controller.answer_accepted"),
        );
        insert_interval(
            &mut explicit_intervals_ms,
            "enter_release_to_final_answer_playback_started",
            interval("voice.enter_released", "final_answer.playback_started"),
        );
        insert_interval(
            &mut explicit_intervals_ms,
            "cancellation_requested_to_local_cleanup_completed",
            interval(
                "cancellation.requested",
                "cancellation.local_cleanup_completed",
            ),
        );

        // Stages can overlap or nest. Merge their windows rather than adding
        // their durations, then label the remainder honestly instead of
        // guessing why it occurred.
        let mut intervals = self
            .stages
            .iter()
            .map(|stage| {
                let start = stage.started_after_interaction_ms;
                (start, start.saturating_add(stage.duration_ms))
            })
            .collect::<Vec<_>>();
        intervals.sort_unstable();
        let mut covered_ms = 0_u64;
        let mut current: Option<(u64, u64)> = None;
        for (start, end) in intervals {
            match current {
                Some((current_start, current_end)) if start <= current_end => {
                    current = Some((current_start, current_end.max(end)));
                }
                Some((current_start, current_end)) => {
                    covered_ms = covered_ms.saturating_add(current_end - current_start);
                    current = Some((start, end));
                }
                None => current = Some((start, end)),
            }
        }
        if let Some((start, end)) = current {
            covered_ms = covered_ms.saturating_add(end - start);
        }
        let total_ms = duration_ms(interaction_duration);

        SummaryState {
            draft_id: self.draft_id,
            turn_id: self.turn_id,
            recording_id: self.recording_id,
            photo_count: self.photo_count,
            input: self.input.clone(),
            ollama: self.ollama.clone(),
            milestones_ms: self.milestones_ms.clone(),
            explicit_intervals_ms,
            stages: self.stages.clone(),
            stages_truncated: self.stages_truncated,
            stage_union_covered_ms: covered_ms,
            unattributed_elapsed_ms: total_ms.saturating_sub(covered_ms),
            unavailable: self.unavailable.clone(),
        }
    }
}

fn insert_interval(intervals: &mut BTreeMap<String, u64>, name: &'static str, value: Option<u64>) {
    if let Some(value) = value {
        intervals.insert(name.to_owned(), value);
    }
}

#[derive(Serialize)]
struct SummaryState {
    #[serde(skip_serializing_if = "Option::is_none")]
    draft_id: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    turn_id: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    recording_id: Option<u64>,
    photo_count: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    input: Option<Fields>,
    #[serde(skip_serializing_if = "Option::is_none")]
    ollama: Option<Fields>,
    milestones_ms: BTreeMap<String, u64>,
    /// Named user-visible intervals only when both endpoints occurred.
    explicit_intervals_ms: BTreeMap<String, u64>,
    stages: Vec<StageSummary>,
    stages_truncated: bool,
    /// Union, not sum, of completed stage windows. It remains meaningful
    /// even when nested stages overlap on the actual timeline.
    stage_union_covered_ms: u64,
    /// Time outside completed stages. It may include user recording, waiting,
    /// or scheduling; telemetry intentionally does not invent a cause.
    unattributed_elapsed_ms: u64,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    unavailable: BTreeMap<String, String>,
}

struct TelemetryCore {
    run_id: String,
    next_interaction_id: AtomicU64,
    jsonl_sender: Option<SyncSender<JsonCommand>>,
    jsonl_dropped: Arc<AtomicU64>,
    metrics: Metrics,
}

impl TelemetryCore {
    fn start_interaction(
        self: &Arc<Self>,
        mode: &'static str,
        draft_id: Option<u64>,
    ) -> Interaction {
        Interaction::new(self.clone(), mode, draft_id)
    }

    fn emit_run_event(&self, operation: &'static str, outcome: &'static str, attributes: Fields) {
        self.emit_record(JsonRecord::RunEvent {
            schema_version: JSONL_SCHEMA_VERSION,
            timestamp_utc_unix_ms: utc_millis(),
            run_id: self.run_id.clone(),
            trace_id: "unavailable".into(),
            span_id: "unavailable".into(),
            operation,
            outcome,
            attributes: redact_fields(attributes),
        });
    }

    fn emit_record(&self, record: JsonRecord) {
        let Some(sender) = &self.jsonl_sender else {
            return;
        };
        match sender.try_send(JsonCommand::Record(Box::new(record))) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => {
                self.jsonl_dropped.fetch_add(1, Ordering::Relaxed);
            }
            Err(TrySendError::Disconnected(_)) => {
                self.jsonl_dropped.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}

struct Metrics {
    operation_duration: Histogram<f64>,
    operation_total: Counter<u64>,
    queue_wait: Histogram<f64>,
    queue_pending_depth: Histogram<u64>,
    active_work: Histogram<u64>,
    in_flight: UpDownCounter<i64>,
    ollama_reported_duration: Histogram<f64>,
    ollama_tokens: Counter<u64>,
    ollama_generation_rate: Histogram<f64>,
}

impl Metrics {
    fn new(meter: Meter) -> Self {
        Self {
            operation_duration: meter
                .f64_histogram("independence.operation.duration")
                .with_unit("s")
                .with_description("Independence operation execution duration in seconds")
                .build(),
            operation_total: meter
                .u64_counter("independence.operation.total")
                .with_description("Completed Independence operations")
                .build(),
            queue_wait: meter
                .f64_histogram("independence.queue.wait")
                .with_unit("s")
                .with_description("Independence local queue wait duration in seconds")
                .build(),
            queue_pending_depth: meter
                .u64_histogram("independence.queue.pending_depth")
                .with_unit("{work}")
                .with_description("Observed pending work in an Independence-owned local queue")
                .build(),
            active_work: meter
                .u64_histogram("independence.active_work")
                .with_unit("{work}")
                .with_description("Observed active work in an Independence-owned component")
                .build(),
            in_flight: meter
                .i64_up_down_counter("independence.in_flight")
                .with_description("Independence application-owned in-flight work")
                .build(),
            ollama_reported_duration: meter
                .f64_histogram("independence.ollama.reported_duration")
                .with_unit("s")
                .with_description("Optional Ollama-reported durations in seconds")
                .build(),
            ollama_tokens: meter
                .u64_counter("independence.ollama.tokens")
                .with_description("Optional Ollama-reported token counts")
                .build(),
            ollama_generation_rate: meter
                .f64_histogram("independence.ollama.generation_tokens_per_second")
                .with_unit("{token}/s")
                .with_description(
                    "Ollama generation throughput when count and positive duration are supplied",
                )
                .build(),
        }
    }

    fn record_operation(
        &self,
        operation: &'static str,
        mode: &'static str,
        outcome: &'static str,
        elapsed: Duration,
    ) {
        let attributes = [
            KeyValue::new("component", component_for(operation)),
            KeyValue::new("operation", operation),
            KeyValue::new("mode", mode),
            KeyValue::new("outcome", outcome),
        ];
        self.operation_duration
            .record(elapsed.as_secs_f64(), &attributes);
        self.operation_total.add(1, &attributes);
    }

    fn record_queue_wait(&self, component: &'static str, mode: &'static str, wait: Duration) {
        self.queue_wait.record(
            wait.as_secs_f64(),
            &[
                KeyValue::new("component", component),
                KeyValue::new("mode", mode),
            ],
        );
    }

    fn record_queue_state(
        &self,
        component: &'static str,
        mode: &'static str,
        pending_depth: usize,
        active_work: usize,
    ) {
        let attributes = [
            KeyValue::new("component", component),
            KeyValue::new("mode", mode),
        ];
        self.queue_pending_depth.record(
            u64::try_from(pending_depth).unwrap_or(u64::MAX),
            &attributes,
        );
        self.active_work
            .record(u64::try_from(active_work).unwrap_or(u64::MAX), &attributes);
    }

    fn record_in_flight(&self, component: &'static str, mode: &'static str, delta: i64) {
        self.in_flight.add(
            delta,
            &[
                KeyValue::new("component", component),
                KeyValue::new("mode", mode),
            ],
        );
    }

    fn record_ollama_statistics(&self, statistics: &OllamaMetricData<'_>) {
        let duration = |phase: &'static str, nanoseconds: Option<u64>| {
            if let Some(nanoseconds) = nanoseconds {
                self.ollama_reported_duration.record(
                    Duration::from_nanos(nanoseconds).as_secs_f64(),
                    &[
                        KeyValue::new("component", "ollama"),
                        KeyValue::new("model", statistics.model.to_owned()),
                        KeyValue::new("phase", phase),
                    ],
                );
            }
        };
        duration("total", statistics.total_duration_ns);
        // Ollama calls this `load_duration`; Independence deliberately does
        // not reinterpret it as disk loading alone.
        duration("ollama_reported_load", statistics.load_duration_ns);
        duration("prompt_eval", statistics.prompt_eval_duration_ns);
        duration("eval", statistics.eval_duration_ns);

        let tokens = |direction: &'static str, count: Option<u64>| {
            if let Some(count) = count {
                self.ollama_tokens.add(
                    count,
                    &[
                        KeyValue::new("component", "ollama"),
                        KeyValue::new("model", statistics.model.to_owned()),
                        KeyValue::new("direction", direction),
                    ],
                );
            }
        };
        tokens("prompt", statistics.prompt_eval_count);
        tokens("prompt_cached", statistics.prompt_eval_cached_count);
        tokens("generated", statistics.eval_count);

        if let (Some(count), Some(duration_ns)) =
            (statistics.eval_count, statistics.eval_duration_ns)
            && duration_ns > 0
        {
            self.ollama_generation_rate.record(
                count as f64 / Duration::from_nanos(duration_ns).as_secs_f64(),
                &[
                    KeyValue::new("component", "ollama"),
                    KeyValue::new("model", statistics.model.to_owned()),
                ],
            );
        }
    }
}

fn component_for(operation: &str) -> &'static str {
    if operation.contains("ollama") {
        "ollama"
    } else if operation.contains("whisper") {
        "whisper"
    } else if operation.contains("camera") || operation.contains("photo") {
        "camera"
    } else if operation.contains("speech")
        || operation.contains("piper")
        || operation.contains("playback")
    {
        "speech"
    } else if operation.contains("recording") || operation.contains("audio") {
        "audio"
    } else {
        "controller"
    }
}

fn queue_wait_reason(component: &str) -> &'static str {
    match component {
        "camera" => "camera_capture_gate",
        "speech" => "ordered_speech_worker",
        "whisper" => "blocking_recognition_worker",
        "microphone" => "microphone_capture_start",
        _ => "application_owned_work",
    }
}

#[derive(Serialize)]
#[serde(tag = "record_type", rename_all = "snake_case")]
enum JsonRecord {
    RunEvent {
        schema_version: u16,
        timestamp_utc_unix_ms: u128,
        run_id: String,
        trace_id: String,
        span_id: String,
        operation: &'static str,
        outcome: &'static str,
        attributes: Fields,
    },
    Event {
        schema_version: u16,
        timestamp_utc_unix_ms: u128,
        run_id: String,
        interaction_id: u64,
        trace_id: String,
        span_id: String,
        mode: &'static str,
        operation: &'static str,
        outcome: &'static str,
        attributes: Fields,
    },
    InteractionSummary {
        schema_version: u16,
        timestamp_utc_unix_ms: u128,
        run_id: String,
        interaction_id: u64,
        trace_id: String,
        span_id: String,
        mode: &'static str,
        outcome: &'static str,
        duration_ms: u64,
        attributes: Fields,
        state: Box<SummaryState>,
        telemetry_jsonl_dropped_records: u64,
    },
}

enum JsonCommand {
    Record(Box<JsonRecord>),
    Shutdown(SyncSender<()>),
}

struct JsonlWorker {
    sender: SyncSender<JsonCommand>,
    thread: Option<JoinHandle<()>>,
    dropped: Arc<AtomicU64>,
}

impl JsonlWorker {
    fn start(path: PathBuf, dropped: Arc<AtomicU64>) -> io::Result<Self> {
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        let (sender, receiver) = mpsc::sync_channel(JSONL_QUEUE_CAPACITY);
        let writer_dropped = dropped.clone();
        let thread = thread::Builder::new()
            .name("independence-telemetry-jsonl".into())
            .spawn(move || write_jsonl(receiver, BufWriter::new(file), writer_dropped))?;
        Ok(Self {
            sender,
            thread: Some(thread),
            dropped,
        })
    }

    fn shutdown(mut self) {
        let (done_sender, done_receiver) = mpsc::sync_channel(1);
        if self
            .sender
            .try_send(JsonCommand::Shutdown(done_sender))
            .is_err()
        {
            self.dropped.fetch_add(1, Ordering::Relaxed);
            warn!(target: "telemetry", event = "jsonl_shutdown_dropped", "telemetry");
            return;
        }
        if done_receiver
            .recv_timeout(TELEMETRY_SHUTDOWN_TIMEOUT)
            .is_err()
        {
            warn!(target: "telemetry", event = "jsonl_flush_timed_out", "telemetry");
            return;
        }
        if let Some(thread) = self.thread.take()
            && thread.join().is_err()
        {
            warn!(target: "telemetry", event = "jsonl_worker_panicked", "telemetry");
        }
    }
}

fn write_jsonl(
    receiver: Receiver<JsonCommand>,
    mut writer: BufWriter<std::fs::File>,
    dropped: Arc<AtomicU64>,
) {
    while let Ok(command) = receiver.recv() {
        match command {
            JsonCommand::Record(record) => {
                let serialized = serde_json::to_writer(&mut writer, &record).is_ok();
                if !serialized || writer.write_all(b"\n").is_err() {
                    dropped.fetch_add(1, Ordering::Relaxed);
                    // Do not include an arbitrary I/O error string here: a
                    // configured path can itself be sensitive. The terminal
                    // still makes the operational failure visible.
                    warn!(target: "telemetry", event = "jsonl_write_failed", "telemetry");
                }
            }
            JsonCommand::Shutdown(done_sender) => {
                if writer.flush().is_err() {
                    dropped.fetch_add(1, Ordering::Relaxed);
                    warn!(target: "telemetry", event = "jsonl_flush_failed", "telemetry");
                }
                let _ = done_sender.send(());
                break;
            }
        }
    }
}

#[derive(Clone)]
struct Correlation {
    trace_id: String,
    span_id: String,
}

#[derive(Default)]
struct TelemetryConfig {
    jsonl_path: Option<PathBuf>,
}

impl TelemetryConfig {
    fn from_environment() -> Self {
        Self {
            jsonl_path: env::var_os("INDEPENDENCE_TELEMETRY_JSONL")
                .filter(|path| !path.is_empty())
                .map(PathBuf::from),
        }
    }
}

fn build_providers(config: &TelemetryConfig) -> (SdkTracerProvider, SdkMeterProvider, Vec<String>) {
    let resource = Resource::builder()
        .with_service_name("independence")
        .with_attributes([KeyValue::new("service.version", env!("CARGO_PKG_VERSION"))])
        .build();
    let mut warnings = Vec::new();

    let mut tracer_builder = SdkTracerProvider::builder().with_resource(resource.clone());
    let mut meter_builder = SdkMeterProvider::builder().with_resource(resource);
    if otlp_endpoint_configured() {
        match SpanExporter::builder().build() {
            Ok(exporter) => tracer_builder = tracer_builder.with_batch_exporter(exporter),
            Err(error) => warnings.push(format!(
                "OTLP trace export is disabled because its configuration is invalid: {error}"
            )),
        }
        match MetricExporter::builder().build() {
            Ok(exporter) => meter_builder = meter_builder.with_periodic_exporter(exporter),
            Err(error) => warnings.push(format!(
                "OTLP metric export is disabled because its configuration is invalid: {error}"
            )),
        }
    }

    let _ = config;
    (tracer_builder.build(), meter_builder.build(), warnings)
}

fn otlp_endpoint_configured() -> bool {
    [
        "OTEL_EXPORTER_OTLP_ENDPOINT",
        "OTEL_EXPORTER_OTLP_TRACES_ENDPOINT",
        "OTEL_EXPORTER_OTLP_METRICS_ENDPOINT",
    ]
    .into_iter()
    .any(|name| env::var_os(name).is_some_and(|value| !value.is_empty()))
}

fn run_id() -> String {
    format!("run-{}-{}", utc_millis(), std::process::id())
}

fn utc_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

fn duration_ms(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

/// A redacted, stable attribute map for telemetry records. Do not put dialogue
/// text, prompt contents, image/audio bytes, paths, or credentials here.
pub type Fields = BTreeMap<String, Value>;

pub fn fields<const N: usize>(entries: [(&str, Value); N]) -> Fields {
    entries
        .into_iter()
        .map(|(key, value)| (key.to_owned(), value))
        .collect()
}

/// Last-resort privacy guard for machine-readable telemetry. Capability code
/// should supply only identifiers, counts, durations, and bounded labels, but
/// common dialogue/payload-shaped keys are still removed here defensively.
fn redact_fields(mut fields: Fields) -> Fields {
    fields.retain(|key, value| {
        let key = key.to_ascii_lowercase();
        let sensitive = key == "text"
            || key.contains("transcript")
            || key.contains("question")
            || key.contains("answer")
            || key.contains("prompt")
            || key.contains("content")
            || key.contains("image_payload")
            || key.contains("audio_payload")
            || key.contains("recording_bytes")
            || key.contains("credential")
            || key.contains("secret")
            || key.ends_with("_path");
        if sensitive {
            return false;
        }
        // Long opaque strings are more likely to be dialogue or server
        // diagnostics than a bounded operational label.
        value.as_str().is_none_or(|string| string.len() <= 256)
    });
    fields
}

/// Mirrors scalar, already-redacted JSON attributes onto an OpenTelemetry span.
/// Arrays and objects remain in JSONL only: forcing them into strings would
/// make the trace schema less queryable and risks accidental content capture.
fn set_span_attributes(span: &Span, fields: &Fields) {
    for (key, value) in fields {
        match value {
            Value::String(value) => span.set_attribute(key.clone(), value.clone()),
            Value::Bool(value) => span.set_attribute(key.clone(), *value),
            Value::Number(number) => {
                if let Some(value) = number.as_i64() {
                    span.set_attribute(key.clone(), value);
                } else if let Some(value) = number.as_u64() {
                    match i64::try_from(value) {
                        Ok(value) => span.set_attribute(key.clone(), value),
                        // OpenTelemetry scalar attributes have no u64 form.
                        // Preserve a rare oversized value as text rather than
                        // lossy-converting a correlation/count field to f64.
                        Err(_) => span.set_attribute(key.clone(), value.to_string()),
                    }
                } else if let Some(value) = number.as_f64() {
                    span.set_attribute(key.clone(), value);
                }
            }
            Value::Null | Value::Array(_) | Value::Object(_) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        sync::{Arc, atomic::AtomicU64},
        time::Duration,
    };

    use opentelemetry::{
        Context,
        trace::{Span as _, TraceContextExt, Tracer as _, TracerProvider as _},
    };
    use serde_json::json;
    use tempfile::tempdir;

    use super::{
        Fields, Interaction, JsonlWorker, Metrics, TelemetryCore, duration_ms, fields,
        redact_fields,
    };
    use opentelemetry::metrics::MeterProvider;
    use opentelemetry_sdk::metrics::SdkMeterProvider;
    use opentelemetry_sdk::trace::{InMemorySpanExporter, SdkTracerProvider};

    fn test_core(worker: &JsonlWorker) -> Arc<TelemetryCore> {
        let provider = SdkMeterProvider::builder().build();
        Arc::new(TelemetryCore {
            run_id: "test-run".into(),
            next_interaction_id: AtomicU64::new(1),
            jsonl_sender: Some(worker.sender.clone()),
            jsonl_dropped: Arc::new(AtomicU64::new(0)),
            metrics: Metrics::new(provider.meter("independence-test")),
        })
    }

    #[test]
    fn fields_retain_explicit_values_without_implicit_debug_capture() {
        let fields = fields([("image_count", json!(2)), ("outcome", json!("accepted"))]);
        assert_eq!(fields["image_count"], 2);
        assert_eq!(fields["outcome"], "accepted");
        assert_eq!(Fields::new().len(), 0);
    }

    #[test]
    fn duration_units_are_explicit_milliseconds() {
        assert_eq!(duration_ms(Duration::from_micros(1_999)), 1);
    }

    #[test]
    fn content_shaped_fields_are_removed_before_export() {
        let fields = redact_fields(fields([
            ("model", json!("qwen3-vl:4b-instruct")),
            ("transcript", json!("private words")),
            ("image_payload", json!("not exported")),
            ("encoded_image_bytes", json!(1_024)),
        ]));
        assert_eq!(fields["model"], "qwen3-vl:4b-instruct");
        assert_eq!(fields["encoded_image_bytes"], 1_024);
        assert!(!fields.contains_key("transcript"));
        assert!(!fields.contains_key("image_payload"));
    }

    #[test]
    fn summary_merges_overlapping_stages_and_keeps_missing_intervals_absent() {
        let mut state = super::InteractionState::default();
        state.milestones_ms.insert("voice.enter_pressed".into(), 0);
        state
            .milestones_ms
            .insert("microphone.recording_started".into(), 240);
        state.stages = vec![
            super::StageSummary {
                operation: "camera.capture_and_validate".into(),
                outcome: "completed".into(),
                started_after_interaction_ms: 100,
                duration_ms: 200,
                attributes: Fields::new(),
            },
            super::StageSummary {
                operation: "ollama.http.exchange".into(),
                outcome: "completed".into(),
                started_after_interaction_ms: 250,
                duration_ms: 200,
                attributes: Fields::new(),
            },
        ];

        let summary = state.summary(Duration::from_millis(600));
        // [100, 300) and [250, 450) overlap by 50 ms, so their union is
        // 350 ms rather than a misleading 400 ms sum.
        assert_eq!(summary.stage_union_covered_ms, 350);
        assert_eq!(summary.unattributed_elapsed_ms, 250);
        assert_eq!(
            summary.explicit_intervals_ms["enter_press_to_microphone_recording"],
            240
        );
        assert!(
            !summary
                .explicit_intervals_ms
                .contains_key("enter_release_to_request_dispatched")
        );
    }

    #[test]
    fn jsonl_preserves_one_redacted_correlated_timeline_without_summing_stages() {
        let directory = tempdir().expect("temporary telemetry directory");
        let path = directory.path().join("telemetry.jsonl");
        let worker = JsonlWorker::start(path.clone(), Arc::new(AtomicU64::new(0)))
            .expect("JSONL worker starts");
        let core = test_core(&worker);
        let interaction = Interaction::new(core.clone(), "voice", Some(7));

        // Start both operations before either completes. The summary records
        // independent stages with their own offsets/durations instead of a
        // fictitious sum of overlapping work.
        let capture = interaction.stage("camera.capture_and_validate", Fields::new());
        let recognition = interaction.stage("whisper.final.recognition", Fields::new());
        let abandoned = interaction.stage("ollama.response.decode", Fields::new());
        interaction.queue_wait("speech", Duration::from_millis(4), 2, 1);
        recognition.finish("completed", fields([("text", json!("must not escape"))]));
        capture.finish("completed", fields([("encoded_image_bytes", json!(512))]));
        drop(abandoned);
        interaction.unavailable("ollama.server_queue_depth", "not exposed by the local API");
        interaction.complete(
            "accepted_text",
            fields([("answer", json!("must not escape"))]),
        );

        drop(interaction);
        drop(core);
        worker.shutdown();

        let records = fs::read_to_string(path).expect("read JSONL output");
        let values = records
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("valid JSON line"))
            .collect::<Vec<_>>();
        assert!(values.len() >= 4, "events and one summary are emitted");
        assert!(values.iter().all(|value| value["run_id"] == "test-run"));
        assert!(
            values
                .iter()
                .filter_map(|value| value.get("interaction_id"))
                .all(|id| id == 1)
        );
        assert!(values.iter().any(|value| {
            value["operation"] == "independence.queue.wait"
                && value["attributes"]["queue_wait_ms"] == 4
                && value["attributes"]["wait_reason"] == "ordered_speech_worker"
        }));
        assert!(!records.contains("must not escape"));
        let summary = values
            .iter()
            .find(|value| value["record_type"] == "interaction_summary")
            .expect("completion summary");
        let stages = summary["state"]["stages"].as_array().expect("stage array");
        assert_eq!(stages.len(), 3);
        assert!(stages.iter().any(|stage| {
            stage["operation"] == "ollama.response.decode" && stage["outcome"] == "abandoned"
        }));
        assert!(
            summary["state"]["unavailable"]
                .get("ollama.server_queue_depth")
                .is_some()
        );
    }

    #[test]
    fn in_memory_otel_exporter_receives_a_correlated_parent_and_child() {
        // This uses the SDK's own in-memory exporter rather than a network
        // collector. Direct OpenTelemetry spans keep this test independent of
        // the process-wide `tracing` subscriber and its static callsite cache.
        let exporter = InMemorySpanExporter::default();
        let provider = SdkTracerProvider::builder()
            .with_simple_exporter(exporter.clone())
            .build();
        let tracer = provider.tracer("independence-test");
        let parent = tracer.start("independence.interaction");
        let parent_context = Context::current_with_span(parent);
        let mut child = tracer.start_with_context("independence.operation", &parent_context);
        child.end();
        parent_context.span().end();

        provider.force_flush().expect("in-memory spans flush");
        let spans = exporter.get_finished_spans().expect("read exported spans");
        let root = spans
            .iter()
            .find(|span| span.name.as_ref() == "independence.interaction")
            .expect("root interaction span");
        let stage = spans
            .iter()
            .find(|span| span.name.as_ref() == "independence.operation")
            .expect("operation span");
        assert_eq!(stage.span_context.trace_id(), root.span_context.trace_id());
        assert_eq!(stage.parent_span_id, root.span_context.span_id());
    }
}
