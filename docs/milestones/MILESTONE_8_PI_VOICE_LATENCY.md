# Milestone 8: Raspberry Pi voice latency

> Status: approved 2026-11-22 (owner decisions recorded in §13)
>
> Date: 2026-11-22. Owner: Sam Ebstein. Branch: `pi-voice-latency`.
> Evidence: `local-data/logs/raspberry-pi-hailo-20261122-234633.log` (Pi,
> Milestone 7 build), Hailo's published Qwen3-VL-2B-Instruct figures, and a
> code review of the bridge and Independence. Builds on
> [Milestone 7](MILESTONE_7_PI_VOICE_RELIABILITY.md) (Appendix A).

## 1. Objective

On the `raspberry-pi-hailo` profile, cut the time from releasing Talk/ENTER to
hearing the first words of the answer from today's 17–22 s (45 s after a
cancelled question) to about 8 s or less, and make every stage's cost visible
in the logs.

## 2. Why the capability is needed

Measured on the Pi (release → first audio):

| Turn | Whisper | Model | Piper | Release → first audio | Spoken length |
| --- | --- | --- | --- | --- | --- |
| 1, no photo | 2.0 s | 12.0 s | 3.4 s | 17.4 s | 15.8 s |
| 3, photo | 2.0 s | 16.9 s | 3.0 s | 21.9 s | 17.0 s |
| 5, photo, after a cancelled turn | 1.9 s | 39.2 s | 3.8 s | 45.0 s | 25.9 s |

Independence's own glue between stages totals about 50 ms (release →
transcription 20–40 ms, resampling ≤ 11 ms, hand-offs < 1 ms). The model is
70–87 % of the wait.

Hailo publishes, for this model on Hailo-10H: time to first token 1.47 s,
**4.89 tokens/s**, 144 vision tokens per image, 2048-token context. The Pi is
running at about that speed. Model time ≈ prompt reading + output tokens ÷ 4.9,
so a 50-token answer costs about 10 s of generation alone. Causes, by size:

1. **A cancelled question keeps the Hailo busy.** `generate_all` cannot be
   interrupted and holds the bridge's generation lock, so a new question waits
   for the abandoned answer to finish (turn 5: 39 s instead of ~22 s). The
   bridge notices the disconnected client only after generation ends.
2. **Nothing is shown or spoken until the last token** (`generate_all`,
   `stream: false`), and Piper then synthesizes the whole answer (3–4 s)
   before any audio plays.
3. **Answers are long for voice.** `src/think/teacher.txt` also asks the model
   to "invite one useful next observation or question". Every 10 words is
   about 2.5 s. Low temperature (0.1) loops run to the token cap.
4. **Too much is re-read every turn.** The bridge clears the Hailo context
   after each request (by design), so the ~210-token system prompt and up to
   six previous exchanges are re-read every time; text-only follow-ups also
   resend the previous photo (`VisualRequest::UseAccepted`; turn 4 logged
   `ollama.image.encode` with `photos=0`), adding 144 vision tokens.
5. **Whisper encodes a full 30-second window** for a 3-second question
   (`audio_ctx` is not set in `src/dictation/transcribe.rs`), using the
   unquantized `ggml-base.en.bin`.
6. **No model-side measurements.** Prompt tokens, time to first token, output
   tokens, tokens/s, and lock wait are not recorded, so the prompt/output split
   above is an estimate.

## 3. Scope

Phases are ordered so each can be measured before the next; each ends with a
working build. Items marked *(Pi)* apply only to `raspberry-pi-hailo`.

- **P1. Measure the model.** The bridge records per request, in its log and
  optional JSONL telemetry: lock wait, prompt tokens (via the Hailo context
  usage API when available), time to first token, output tokens, tokens/s,
  whether the token cap was reached, and outcome (completed / aborted /
  failed). Counts and timings only. Internally the bridge switches from
  `generate_all` to Hailo's streaming `generate()` to observe tokens; the HTTP
  contract is unchanged in this phase.
- **P2. Cancel frees the Hailo.** While generating, the bridge checks between
  tokens whether the client connection has closed and stops generation if so
  (leaving `generate()`'s context, then `clear_context()`), releasing the lock
  within about one token time. A new question no longer waits for an
  abandoned answer.
- **P3. Shorter voice turns.**
  - A voice-only addition to the system prompt asking for one to three short
    spoken sentences with no lists or markdown, and no follow-up question
    unless needed. Typed mode is unchanged.
  - *(Pi)* Launcher defaults for bridge temperature and token cap (values in
    §13).
  - Voice follow-ups send a photo only with the turn that captured it (owner
    decision §13.2).
  - Voice history sent to the model is unchanged (all accepted exchanges, up
    to the controller's limit of 6; owner decision §13.3).
- **P4. Stream end to end.**
  - Bridge: support `stream: true`, returning Ollama's newline-delimited JSON
    chunks (`{"message":{"content":...},"done":false}` … final
    `done: true`). `stream: false` keeps working. A failed client write
    aborts generation (as P2).
  - Independence: `OllamaClient` reads the stream; the turn runtime forwards
    text chunks tagged with the current `TurnId`; a sentence splitter emits
    complete sentences; the controller authorizes each sentence for display
    and speech only while that `TurnId` is current (design change, §13.5);
    the complete answer enters history only when the stream finishes.
  - Speech: each sentence is synthesized and queued in order under the turn's
    owner; synthesis of sentence N+1 overlaps playback of sentence N. Escape
    and a new press cancel queued and playing sentences as today.
  - Terminal: the answer appears sentence by sentence as it is spoken
    (§13.4). Typed mode keeps whole-answer display unless trivially shared.
- **P5. Faster Whisper.** Set whisper.cpp's `audio_ctx` from the clip length
  (with a safety margin and a floor), and support an optional quantized model
  path via the existing `INDEPENDENCE_WHISPER_MODEL`. Accuracy is checked on
  scripted phrases. No new Whisper model is introduced (§13.6).
- **P6. System prompt loaded once at start** *(owner request; uses
  `save_context` / `load_context` when the Pi's HailoRT exposes them)*. At
  bridge start, read the fixed system prompt once, save that context, and
  restore it per request instead of re-reading it. The snapshot is keyed by
  the exact system-prompt text: a request with a different system prompt
  (for example, typed mode without the voice addition) is read in full as
  today. If the API is unavailable, the bridge logs that the cache is off and
  behaves exactly as before. The snapshot contains only the system prompt, never user
  content; `clear_context()` behavior after each request is otherwise kept.
  Requires the bridge to receive the system prompt identically each time (it
  does today: `TEACHER_INSTRUCTIONS` plus the P3 voice addition).

Must be preserved: controller ownership of history and acceptance (with the
§13.5 extension), stale-result rejection by `TurnId`, one photo per request,
camera consent, loopback-only bridge, no persistence of conversation,
answers shown exactly as the model produced them (no post-processing; Milestone
7 §13.7), no transcript/answer/prompt text in logs or telemetry.

## 4. Explicitly excluded work

- A different VLM, a different TTS engine, or changes to the HAL voice model.
- Keeping conversation context resident on the accelerator between requests
  (beyond the non-private system-prompt snapshot in P6).
- A long-running Piper process or Piper raw-audio streaming (revisit after P4
  measurements).
- Changing `/boot/firmware/config.txt` (PCIe Gen 3), the CPU governor, or other
  OS settings; these are manual owner experiments in §9, not code.
- Answer post-processing (trimming, de-duplication).
- Typed-mode streaming display, unless P4 makes it free.

## 5. Expected behavior

- **Normal voice turn:** release → "Transcribing..." (shorter) → "You: …" →
  "Asking the local model..." → first sentence printed and spoken a few
  seconds later → remaining sentences follow without long gaps → "Ready".
- **Escape or a new press during generation:** audio stops, the bridge stops
  generating within about one token time, and the next question starts
  immediately. No partial answer enters history.
- **Stream breaks mid-answer** (bridge failure): the sentences already spoken
  stay on screen; a warning says the answer was incomplete; nothing enters
  history.
- **Follow-up without a new photo:** answered from text history only (if
  §13.2 approved); the model is not shown the old photo.
- **Typed mode and laptop voice:** unchanged except where P1/P2/P4 bridge or
  client changes apply transparently.
- **Model guidance vs enforcement:** the token cap and history limit are
  enforced; "short spoken sentences" is a request the model may ignore.

## 6. Implementation boundaries

- `tools/hailo_vlm_bridge.py` owns P1, P2, the bridge side of P4, and P6.
  Metrics go to stdout (captured by the launcher log) and to the existing
  optional `hailo_vlm.*` JSONL.
- `src/think/mod.rs` owns the streaming client (async `reqwest`
  `Response::chunk()`; no new HTTP dependency expected) and request shaping
  (voice prompt addition, history limit, photo policy inputs).
- `src/controller.rs` remains the authority: it gains a narrowly scoped
  "provisional sentence for the current `TurnId`" event/effect pair; accepted
  history is still written once, from the complete answer.
- `src/app.rs` owns the sentence splitter boundary and turn-runtime plumbing;
  `src/speak/worker.rs` keeps one ordered queue and the existing generation
  cancellation.
- `src/dictation/transcribe.rs` owns `audio_ctx`.
- `scripts/run-raspberry-pi-hailo.sh` owns Pi defaults (temperature, cap).

## 7. Security and privacy threat review

- **Data touched:** prompts, photos, and answers flow as today over loopback;
  streaming changes timing, not destinations.
- **New exposure:** sentences are displayed and spoken before the full answer
  is accepted. They are still authorized per `TurnId`, never written to
  history unless the answer completes, and dropped if the turn is cancelled.
- **Less exposure:** abort stops the accelerator processing a cancelled
  question sooner; P3 sends the previous photo and old history less often.
- **P6 snapshot:** holds the fixed, non-private system prompt only; it must be
  created before any user request and never re-saved after one. Bridge restart
  discards it.
- **Logs/telemetry:** token counts, durations, booleans, outcomes only. No
  prompt, transcript, answer, or image data. Prompt token counts reveal
  approximate question length; acceptable for local diagnostics, documented.
- **Dependencies:** none new expected. Hailo APIs used (`generate`,
  `get_context_usage_size`, `save_context`, `load_context`) are verified in the
  laptop Hailo Apps checkout only; P1 begins by confirming them on the Pi.
- Review against [REVIEW_CHECKLIST.md](../security/REVIEW_CHECKLIST.md): pending.

## 8. Automated tests

No test needs the Pi, Hailo, a microphone, Piper, or a network.

- Bridge (fake streaming VLM): metrics computed correctly; no text in metrics;
  client disconnect stops generation and releases the lock; `stream: true`
  emits valid NDJSON ending in `done: true`; `stream: false` unchanged;
  context snapshot restored per request when available and skipped when not.
- Rust client: NDJSON parsing across arbitrary chunk boundaries, error chunks,
  truncated stream, cancellation mid-stream.
- Controller: provisional sentences accepted only for the current `TurnId`;
  stale ones ignored; history written once from the complete answer; cancelled
  partial never enters history.
- Sentence splitter: punctuation, decimals, abbreviations, missing final
  punctuation (flushed at end of stream).
- Speech: sentences play in order; Escape cancels queued and playing ones.
- Request shaping: voice prompt addition present only in voice; history limit
  applied; photo sent only on the capturing turn (if approved).
- Whisper `audio_ctx` computation for short, typical, and 30-second clips.
- Launcher: new defaults passed to the bridge.

## 9. Manual verification procedure (real Pi)

1. Confirm the Hailo APIs on the Pi:
   `$HOME/.venvs/hailo-qwen3-smoke/bin/python -c "from hailo_platform.genai import VLM; print([m for m in dir(VLM) if not m.startswith('_')])"`.
2. Optional system checks (owner experiments, record results):
   `scripts/check_pi_hailo_power.sh` during a turn;
   `cat /sys/devices/system/cpu/cpu0/cpufreq/scaling_governor`.
3. After each phase, run `scripts/run-raspberry-pi-hailo.sh` and the same
   scripted, non-private turns: two text questions, two photo questions, one
   follow-up without a photo, one question cancelled by a new press after
   ~3 s followed immediately by another question.
4. From the Independence and bridge logs record: Whisper ms, bridge lock wait,
   prompt tokens, time to first token, output tokens, tokens/s, release →
   first audio.
5. P5 only: read ten scripted phrases and compare transcripts with and without
   `audio_ctx` (and with any alternative model).

## 10. Acceptance criteria

- [ ] Bridge log shows prompt tokens, time to first token, output tokens,
      tokens/s, lock wait, and outcome for every request; no text content.
- [ ] After a cancel, the next request's lock wait is < 1 s.
- [ ] Release → first audio: median ≤ 8 s across the scripted turns
      (baseline 17–22 s).
- [ ] Whisper final recognition ≤ 1.0 s for the scripted short questions with
      no transcript regression on the ten phrases.
- [ ] Escape stops audio and generation; no partial answer in history.
- [ ] Typed mode and laptop voice still pass their tests.
- [ ] `cargo test`, clippy, rustfmt, and Python tests pass.

## 11. Documentation updates

README "Teach Sam", `LEARNING.md` (where the time goes and why streaming
helps), `docs/ARCHITECTURE.md` (streaming flow and provisional sentences),
`docs/logging.md` (bridge metrics), `docs/raspberry-pi-hailo-qwen3.md`
(new defaults, metrics, Whisper model option), `docs/security/THREAT_MODEL.md`
(provisional output).

## 12. Completion report

Status: **implemented on branch `pi-voice-latency`; awaiting owner review and
the §9 Pi measurement run.** Nothing is committed.

**Confirmed on the Pi before coding (owner ran it):** `VLM` exposes
`generate` (streaming context manager), `generate_all`, `save_context`,
`load_context`, `get_context_usage_size`, `clear_context`, `tokenize`,
`max_context_capacity`, and `get_generation_recovery_sequence`.

**What was done, by phase:**

- P1/P2 (`tools/hailo_vlm_bridge.py`): generation always uses streaming
  `generate()`; `GenerationStats` (lock wait, cache state, context before,
  prompt tokens, time to first token, output tokens, tokens/s, cap hit,
  outcome) is logged per request and added to JSONL and to the Ollama-style
  response (`prompt_eval_count`, `eval_count`, durations). Generation stops
  when the client hangs up: a failed chunk write when streaming, or a
  non-blocking `MSG_PEEK` end-of-file check between tokens otherwise.
- P3: `src/think/voice.txt` appended to the system prompt in voice
  (`voice_instructions()`); launcher defaults temperature 0.3 and cap 100;
  `Controller::with_photo_only_on_capturing_turn()` in voice.
- P4: bridge `stream: true` → NDJSON; `OllamaClient::answer_streaming` with an
  NDJSON parser that tolerates arbitrary chunking and a server that ignores
  `stream: true`; `SentenceSplitter`; `Event::TurnPartial` /
  `Output::AnswerSentence` / `Output::AnswerStreamed`;
  `TurnResult::Answer { streamed }`; terminal prints sentences as spoken; the
  speech worker synthesizes the next queued utterance on a scoped helper
  thread while the current one plays.
- P5: `audio_ctx_for` sizes whisper.cpp's encoder window (clip + 128 frames,
  multiple of 64, floor 256, max 1500); no model change.
- P6: system-only "prime" request (`done_reason: load`) builds a context
  snapshot of the exact system prompt (one-token generation, as Hailo Apps
  does), restored with `load_context()` per request; different system text
  rebuilds it; any failure falls back to reading it in full;
  `--no-system-prompt-cache` / `BRIDGE_SYSTEM_PROMPT_CACHE=0` turns it off.
  Independence primes it at voice startup for the `hailo-vlm` backend.

**Prompt change (owner request, 2026-11-22):** `teacher.txt` no longer tells
the model to invite a next observation or question; it now says "If there is
a question worth asking, ask it; otherwise just answer." `voice.txt` no longer
discourages follow-up questions. Asking is left to the model's judgment in
both modes.

**Fixes from the first Pi run of this branch (2026-11-22):**

- Voice reached the 6-exchange limit and told the person to type `/reset`,
  which button mode cannot read. Voice now keeps the last 6 exchanges and
  forgets the oldest (`Controller::with_rolling_history`); typed mode keeps
  the limit and `/reset`.
- Whisper's non-speech markers (`[BLANK_AUDIO]`, `[ Silence ]`, …) were sent to
  the model as questions. They are now removed; a recording with no words
  shows "No speech heard. Hold ENTER, speak, then release." and sends nothing.
- "Asking the local model..." is printed only when a request actually starts.
- **P5 failed its accuracy check and is off by default.** With the sized
  `audio_ctx`, Whisper repeated phrases ("What is gravity? What is gravity?",
  one question five times) — a known effect of shrinking whisper.cpp's
  encoder window. The full 30-second window is the default again;
  `INDEPENDENCE_WHISPER_SIZED_WINDOW=1` enables the sized window for
  experiments only. Whisper stays at ~2 s per question.

**Implementation decision beyond the plan (for owner review):** the client
previously rejected answers with `done_reason: length`. The bridge now reports
that honestly whenever the cap is hit, and in streaming the sentences have
already been spoken, so a capped answer is now accepted exactly as produced
(for typed mode too) and `done_reason` is recorded in telemetry. This follows
§13 decision and Milestone 7's "answers shown exactly as produced".

**Commands actually run (laptop, 2026-11-22):**

- `cargo test` — 212 passed, 0 failed, 3 ignored (hardware checks).
- `cargo clippy --all-targets` — no warnings; `cargo fmt --check` — clean.
- `python3 -m unittest tools/tests/test_hailo_vlm_bridge.py
  tools/tests/test_pi_hailo_shell_scripts.py` — 44 passed, including real
  loopback-socket tests for NDJSON streaming and hang-up abort.

**Not yet tested (needs the Pi):** real Hailo streaming/abort behavior
(whether leaving `generate()` early stops the device promptly), the
system-prompt snapshot's effect on answer quality and prompt tokens, actual
latencies against §10 targets, Whisper accuracy with `audio_ctx` on your
voice, and audible gaps between sentences with the HAL voice.

## 13. Owner decisions (recorded 2026-11-22)

1. **Pi bridge defaults:** temperature 0.3, token cap 100.
2. **Photos on follow-ups (voice):** a photo is sent only with the turn that
   took it.
3. **Voice history:** keep sending all accepted exchanges (controller limit 6).
4. **Display:** the answer is printed sentence by sentence as it is spoken.
5. **Controller design change:** approved — provisional sentences are
   authorized per `TurnId`; history is written once from the complete answer.
6. **Whisper:** enable the clip-sized `audio_ctx`; do not introduce a new
   model.
7. **HAL speech speed:** unchanged.
8. **Order:** all phases (P1–P6) in one pass.
9. **System prompt:** keep it loaded from bridge start rather than re-reading
   it every turn, where the Hailo API allows (P6, promoted from optional).
