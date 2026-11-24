# Terminal interface and diagnostics

Independence has two deliberately separate output paths.

## Terminal interface

The application writes its user interface to standard output. This is the
place for answers, accepted final voice transcripts, ready/listening/thinking
status, camera/device information, instructions, and actionable errors.

It does not contain timestamps, Rust module names, log levels, interaction
IDs, worker events, or timing spans. Text displayed here is not input to the
speech engine; voice output is separately authorized and queued after the
controller accepts an answer.

Terminal scrollback and redirected standard output can still retain displayed
conversation text. This is an operating-system/terminal limitation, not a
promise of secure display erasure.

## Diagnostic logging

Diagnostics are for development, not ordinary use. They are disabled by
default. To enable them, set `INDEPENDENCE_LOG` to exactly one of the four
supported levels:

- `error`
- `warn`
- `info`
- `debug`

Diagnostics write to standard error, so they can be captured separately from
the interface. For example:

```bash
mkdir -p local-data/logs
INDEPENDENCE_LOG=debug cargo run -- --voice \
  2>local-data/logs/independence-development.log
```

`trace` is intentionally unsupported. Independence filters dependency trace
noise rather than treating it as a user interface or as a safe debugging
channel.

At every supported level, diagnostics may include component names, operation
names, lifecycle events, durations, stable IDs, error classifications, model
identifiers, and hardware/backend metadata. They must never include user or
assistant text, transcripts, prompts, model responses, tool arguments or
results, memory/evidence contents, image descriptions, or audio/visual data.
When a failure could contain arbitrary external text, the diagnostic records a
stable failure class rather than the raw error wording.

## Structured telemetry

`INDEPENDENCE_TELEMETRY_JSONL` remains a separate, opt-in structured timing
stream. It uses its existing redaction layer and never intentionally records
conversation or media content. For example:

```bash
mkdir -p local-data/telemetry
INDEPENDENCE_TELEMETRY_JSONL=local-data/telemetry/independence.jsonl \
  cargo run
```

Read one compact interaction summary with:

```bash
jq -c 'select(.record_type == "interaction_summary") |
  {interaction_id, outcome, duration_ms, stages: [.state.stages[] | {operation, outcome, duration_ms}]}' \
  local-data/telemetry/independence.jsonl
```

Telemetry and diagnostic files are development artifacts. They are not
encrypted, rotated, or an archive of conversations.

## Pi launcher logs

`scripts/run-raspberry-pi-hailo.sh` writes Independence diagnostics to
`local-data/logs/raspberry-pi-hailo-<time>.log` (default level `debug`) and
the Hailo bridge's own output to `local-data/logs/hailo-bridge-<time>.log`.
The bridge log contains its startup/health messages and bounded failure
reasons, not prompts, images, or answers.

## Hailo bridge request metrics

Every generation adds one line to the bridge log, for example:

```text
Hailo VLM request: outcome=completed lock_wait_ms=0 system_prompt_cache=hit context_tokens_before=268 prompt_tokens=171 output_tokens=38 time_to_first_token_ms=1820 generation_ms=9400 tokens_per_second=4.88 hit_token_cap=no
```

| Field | Meaning |
| --- | --- |
| `outcome` | `completed`, or `aborted` when the client hung up mid-answer |
| `lock_wait_ms` | Time waiting for a previous generation to release the Hailo |
| `system_prompt_cache` | `hit` (snapshot restored), `miss` (built now), `off`, or `unavailable` |
| `context_tokens_before` | Tokens already in context before this request (the cached system prompt) |
| `prompt_tokens` | Tokens read for this request (history, question, 144 per image) |
| `output_tokens` | Tokens generated |
| `time_to_first_token_ms` | Reading the prompt (and image) until the first token |
| `tokens_per_second` | Generation speed after the first token |
| `hit_token_cap` | Whether the answer was cut off by `--max-generated-tokens` |

The same fields appear as `generation_*` in the optional JSONL telemetry.
Counts reveal roughly how long a question or answer was; no text is recorded.

## Voice language setup

At voice startup, one `voice_language_configured` line records the Whisper
language code (`en`, `es`, `auto`, …) and the *file names* of the Whisper
model and Piper voice, without their directories. It is configuration only:
no audio, transcript, or answer text. If a non-English language is paired with
an English-only Whisper model, the first transcription fails with
`failure_class=english_only_model` (normally the launch refuses this first).

## Voice capture outcomes

Each voice recording reports how it ended through the
`microphone.capture.lifecycle` and `microphone.recording` telemetry events.
The outcome is one of `completed`, `cancelled`, `too_short`, or `failed`. A
failure always carries a stable `failure_class`, never audio or transcript
text:

| `failure_class` | Meaning |
| --- | --- |
| `device_unavailable` | The microphone could not be found, configured, opened, or started |
| `stream_error` | The platform audio stream reported an error while recording |
| `dropped_frames` | The capture thread fell more than two seconds behind the audio callback; `dropped_frames` gives the count |
| `buffer_overflow` | The recording exceeded the bounded in-memory limit |
| `invalid_audio` | The audio was empty, silent, or contained invalid samples |
| `lock_poisoned` | A shared error slot was poisoned by a panicking thread |

`too_short` (with `recorded_ms`) means the press held less than 300 ms of
audio. It is guidance for the person, not a device failure, and Whisper is not
run.

"Listening...", "Transcribing...", and "Asking the local model..." are
terminal status lines and are never
spoken. A failed live partial transcription (laptop only) is recorded as the
diagnostic event `partial_transcription_failed` and is not shown in the
terminal, because it does not affect the final transcript.
