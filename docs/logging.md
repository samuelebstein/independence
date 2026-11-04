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
