# Milestone 7: Raspberry Pi voice reliability and immediate start

> Status: approved 2026-11-22 (owner decisions recorded in §13)
>
> Date: 2026-11-22. Owner: Sam Ebstein. Evidence: Pi debug logs
> `local-data/logs/latency-20261122-2129.log`, `-2134.log`, and the
> `--max-generated-tokens 60` run (not committed; timing and outcome fields
> only, no transcript content).

## 1. Objective

On the Raspberry Pi Hailo profile, a normal hold-to-talk turn, with or
without a photo, completes every time, and the terminal tells the person what
is happening in the right order and only once. Independence starts in
Ephemeral Mode immediately, and the microphone opens as soon as ENTER is
pressed. General speed work is not a goal of this milestone (see Appendix A);
R7 and R8 remove waits the owner experiences on every start and every turn.

## 2. Why the capability is needed

In two Pi sessions, five of the eleven attempted turns failed or were
discarded, and the terminal output was duplicated and out of order. A device
meant to be trusted without a screen cannot lose what the person said.

Observed problems:

1. **Recording lost when ENTER is released during photo capture.** In every
   photo turn so far (5/5), releasing ENTER while `rpicam-still` was still
   running (~1.4 s after P) ended in "Voice recognition could not be
   completed"; releasing after the capture finished worked.

   | Run / turn | P | Capture done | Release | Result |
   | --- | --- | --- | --- | --- |
   | 2134 / 1 | 54.9 | 56.56 | 56.62 | ok |
   | tokens-60 / 8 | 30.6 | 32.15 | 32.77 | ok |
   | 2129 / 9 | 15.1 | — | 16.46 | `device_failure` |
   | 2129 / 10 | 29.1 | — | 29.48 | `device_failure` |
   | tokens-60 / 9 | 16.8 | — | 17.74 | `device_failure` |

   Likely cause (from code, not yet proven): the microphone callback stores
   audio with `try_lock` and counts any contention as dropped frames
   (`src/dictation/audio.rs:569`); any dropped frame fails the whole recording
   (`:594`). Live partial transcription clones the entire buffer under that
   lock every 750 ms (`PARTIAL_TRANSCRIPTION_INTERVAL`, `src/app.rs:56`) while
   Whisper (4 threads), `rpicam-still`, and Piper compete for the CPU. Two of
   the failures report `microphone.recording=device_failure` (stream error
   path) and one reports `completed` followed by a lifecycle failure (buffer
   `take` path), so the exact cause is not yet distinguishable — the
   diagnostics only say `device_failure`.

2. **Very short recordings reported as recognition failures.** Turns 6 and 7
   recorded 30–60 ms (release 30–50 ms after the microphone opened, ~0.9 s
   after press). Whisper failed in under 10 ms and the person saw "Voice
   recognition could not be completed". The owner reports a sticky ENTER
   button; the software should handle either cause clearly.
3. **Duplicate terminal lines:** "Thinking..." (printed from both
   `src/app.rs:1036` and `src/speak/mod.rs:145`), the Ephemeral Mode entry
   instructions, and "Ephemeral Mode is active" (`src/interface.rs:213, 235`).
4. **Waiting before the device is usable.** Every start requires a
   five-second ENTER hold before Ephemeral Mode begins
   (`EPHEMERAL_ENTRY_HOLD`, `src/security.rs:93`). This is documented as a
   usability/intent gesture, not authentication
   (`docs/security/ARCHITECTURE.md`, "Physical entry policy"). Private Mode is
   still accepted by `--mode private` only to be rejected at startup.
5. **Spoken status cues delay speaking.** "i'm listening" and "i'm thinking"
   are synthesized by Piper and played; the microphone does not open until the
   listening cue finishes (~0.7–2.5 s after press, `src/app.rs:1469`), so the
   start of what the person says can be lost. The thinking cue competes with
   the final Whisper pass for CPU.
6. **Out-of-order terminal lines:** "Ready. Hold ENTER to speak." printed
   before the answer; a recognition warning printed before "Photo captured".

## 3. Scope

All changes apply to `--voice`. Items marked *(Pi)* apply only when the
resolved profile is `raspberry-pi-hailo`.

- **R1. Diagnose before fixing.** Replace the single `device_failure` label
  with a bounded failure class: `stream_error`, `dropped_frames`,
  `buffer_overflow`, `too_short`, `lock_poisoned`. Include counts (frames
  dropped, recording ms) and never audio or text.
- **R2. No live partial transcription** *(Pi)*. Partial ticks are ignored for
  this profile. The final transcript is still displayed and logged as today.
  Laptop keeps live partials.
- **R3. Microphone capture cannot lose audio to lock contention.** The audio
  callback must never block and must never silently drop frames because
  another thread is reading. Proposed: move samples from the callback to the
  capture thread through a lock-free single-producer/single-consumer ring
  buffer (`rtrb`, a maintained crate used for this purpose in real-time
  audio), so the capture thread alone owns the growing recording.
- **R4. Release during photo capture.** Releasing ENTER while a capture is in
  progress finishes the recording normally; the turn waits for the capture
  (bounded by the existing 5 s `RPICAM_PROCESS_TIMEOUT`) and then submits
  transcript + photo. If the capture fails, the existing rule holds: no
  text-only request; ask the person to try the photo again.
- **R5. Short recordings.** A recording shorter than a minimum (proposed
  300 ms) is not sent to Whisper. The person sees "Recording too short. Hold
  ENTER while you speak." The release-before-microphone-opened case
  ("Recording was not started.") uses the same wording.
- **R6. Terminal output once and in order.** Each status line is printed by
  exactly one owner. Order for a turn: `Listening...` → (`Photo captured...`)
  → `Thinking...` → `You: ...` → `Independence: ...` → `Ready. Hold ENTER to
  speak.` Warnings for a turn appear in that turn's position, never after the
  next prompt.

- **R7. Immediate Ephemeral Mode; no Private Mode option.**
  - Remove the five-second physical entry gesture. Starting the program is the
    explicit action; Ephemeral Mode begins at launch in every input mode, and
    the first ENTER press starts a normal recording.
  - Remove `--mode` / Private Mode from the command line (an unknown option
    error, not a parsed-then-rejected mode). The `SecurityManager` lifecycle
    states and the future encryption design stay in place, unused by the CLI,
    so the approved security architecture is not discarded.
- **R8. No spoken status cues; microphone opens on press.**
  - Do not synthesize or play "i'm listening" / "i'm thinking". Remove the
    cue-before-capture gate; pressing ENTER starts capture immediately.
  - `Listening...` and `Thinking...` remain as single terminal status lines
    (R6) and as diagnostic events with timestamps, for verification.
  - Piper is used only for answers and existing fixed error notices.

**Added 2026-11-22 after the first Pi run (owner-approved scope):**

- **R9. One-command Pi launcher: `scripts/run-raspberry-pi-hailo.sh`.**
  - Defaults for every path/device used so far (HEF, Hailo Apps, bridge
    Python, Piper command/model/config, button device, microphone, speaker,
    bridge generation settings), each overridable by an environment variable.
  - Pre-flight: required files exist, button device exists, release binary
    exists, and `rpicam-still --list-cameras` lists a camera. A missing camera
    is fatal here, before the bridge spends time loading the model.
  - Starts the Hailo bridge in the background, waits for `/healthz` to report
    `ready` (bounded), then runs Independence in the foreground with
    `--profile raspberry-pi-hailo`. Diagnostics and bridge output go to
    timestamped files under `local-data/logs/`.
  - **The bridge stops when Independence exits** (Ctrl+C, error, or normal
    exit) — owner decision. If a bridge is already listening on the port, the
    script refuses rather than reusing or killing a process it did not start.
- **R11. Startup checks say what is actually unavailable.**
  - For the `hailo-vlm` backend, a failed capability check says the Hailo
    bridge is not reachable/ready (today's warning wrongly says text chat is
    available).
  - The `raspberry-pi-hailo` profile requires a working camera. At voice
    startup Independence lists cameras once; if none is found (or listing
    fails), it exits with a fatal error naming the fix (power off, reseat the
    ribbon, check `rpicam-still --list-cameras`) before opening the microphone
    or speaker. Other profiles are unchanged: the laptop and the text-only
    `raspberry-pi` profile do not require a camera.

Must be preserved: controller ownership of turns and photos, camera consent,
one photo per request with replacement, cancellation semantics (Escape,
new recording, shutdown), Ephemeral Mode, no transcript text in diagnostics,
laptop behavior other than R1/R3/R5/R6.

## 4. Explicitly excluded work

- Prompt/brevity and token-cap changes, temperature defaults, and
  sentence-by-sentence speech (latency milestone).
- Post-processing model answers (trimming cut-off sentences or repeated
  text). Owner decision: it would hide model behavior that should be seen and
  fixed at its source (prompt, token cap, temperature).
- Keeping the bridge running between Independence runs (owner chose stop).

- Other latency work (Appendix A): token cap defaults,
  voice-mode brevity prompt, sentence-by-sentence speech, bridge streaming,
  conversation-history limits.
- Model output quality, including the repetition loop seen with Qwen3-VL-2B
  ("magnetic refrigerator…").
- Allowing more than one photo per request.
- Software debouncing of the physical button beyond R5 (investigate the
  button first; see §9 step 1).
- Camera speed, Whisper model changes, removing other modes, a launcher
  script.

## 5. Expected behavior

- **Start:** `Ephemeral Mode is active. Hold ENTER to speak.` appears once
  at launch; no hold is needed.
- **`--mode private`:** rejected as an unknown option before any device or
  model setup.
- **Normal turn:** press → `Listening...` with no audio cue and the
  microphone already recording → speak → release → `Thinking...` →
  transcript → answer, every time.
- **Photo, early release:** P then immediate release → "Finishing
  recording..." → turn proceeds once the photo is ready, with the photo.
- **Photo capture fails:** recording is kept but no request is sent; the
  person is told the photo failed and to try again (current rule).
- **Tap or sticky button (< 300 ms audio):** "Recording too short. Hold ENTER
  while you speak." No Whisper run, no warning-level failure.
- **Real device failure** (USB mic unplugged, ALSA stream error): existing
  warning, with an R1 failure class in diagnostics.
- **Escape at any point:** unchanged — recording, pending photo, model
  request, and speech are cancelled.

## 6. Implementation boundaries

- `src/dictation/audio.rs` owns capture (R1, R3, R5 measurement). The
  callback only pushes samples; the capture thread owns the recording.
- `src/app.rs` owns partial-tick gating by profile (R2) and the
  release-while-capturing sequencing (R4). The controller remains the only
  owner of accepted photos and turns; R4 must not let a stale capture attach
  to a newer turn (existing `TurnId` / draft identity checks apply).
- `src/app.rs` / `src/security.rs`: remove `await_button_ephemeral_entry`
  and the entry gesture; acquire the Ephemeral lease directly at startup
  (R7). `src/launch.rs`: remove `--mode` parsing (R7).
- `src/app.rs` / `src/speak/worker.rs`: remove the pending listening-cue
  state, cue speech requests, and the fixed-phrase cue cache (R8). Escape and
  new-recording cancellation of answer speech are unchanged.
- `src/interface.rs` becomes the single place each status line is emitted
  (R6); callers request a status, they do not print.
- New dependency `rtrb` for R3 (owner decision §13.2).

## 7. Security and privacy threat review

- **Data touched:** microphone samples (in memory only, as today), transcript,
  photograph. No new storage, network, or process.
- **Changes in exposure:** R2 removes repeated full-buffer copies and
  intermediate transcripts on the Pi. R3 changes in-memory buffering only;
  samples are still discarded at the end of the turn. R4 keeps a captured
  photo for at most the existing capture timeout before the turn proceeds or
  is cancelled.
- **R7 (removing the entry hold):** the hold was an intent gesture, not
  authentication, and gated no private data. Launching the program now starts
  Ephemeral Mode; the microphone still opens only while ENTER is held and the
  camera only on P during a hold. Residual risk: a stray ENTER press on the
  Pi records immediately rather than after an entry ceremony. Removing the
  `--mode private` option removes no protection, since Private Mode had no
  implementation.
- **R8 (no spoken cues):** on a Pi with no screen there is no audible
  microphone indicator; the held button is the only indication that the
  microphone is open. Capture remains strictly push-to-talk. A future
  hardware LED or short tone could restore an indicator without Piper.
- **Logging:** R1 adds failure classes and counts only — no samples, text,
  device serial numbers, or raw backend error strings beyond the existing
  bounded messages.
- **Dependency:** `rtrb` (if adopted) is a small, pure-Rust, no-network crate;
  review its version and license before adding.
- **Residual risks:** unchanged from the current threat model.
- Review against [REVIEW_CHECKLIST.md](../security/REVIEW_CHECKLIST.md):
  pending, completed with implementation.

## 8. Automated tests

No test needs a Pi, microphone, camera, Hailo, Piper, or network.

- R1: each capture failure path reports its class; no class contains sample or
  text data.
- R2: `raspberry-pi-hailo` ignores partial ticks; the laptop profile does not.
- R3: a fake callback pushing frames while a reader repeatedly snapshots loses
  no frames; overflow still reports `buffer_overflow`.
- R4: release while a fake capture is pending → one request with transcript +
  photo after capture completes; capture failure → no request; Escape during
  the wait → nothing sent; a late capture for an old draft is ignored.
- R5: 0 ms, 100 ms, 299 ms recordings → too-short notice and no recognizer
  call; 300 ms → recognizer called.
- R7: voice mode with `--input buttons` and terminal input reaches "ready"
  without any entry gesture; the first press starts a recording; `--mode` and
  `--mode private` are rejected by the parser; the Ephemeral lease is acquired
  exactly once.
- R8: a press starts capture with no speech request; the fake speech service
  receives only answer text; release followed by answer still cancels
  correctly on Escape.
- R6: a recorded terminal sink shows each status line once and in the §3 order
  for: text turn, photo turn, failed photo, too-short recording, Escape.

## 9. Manual verification procedure (real Pi)

1. **Button check (before code changes).** Run
   `sudo evtest /dev/input/event2`, press and hold ENTER for ~3 s ten times,
   and note any release event that arrives before you lift your finger. This
   tells us whether the "sticky" button sends early releases.
2. Start the bridge and Independence as usual (without any `--mode` flag) with `INDEPENDENCE_LOG=debug`
   redirected to `local-data/logs/reliability-m7-<time>.log`.
3. Use a non-private scene. Perform and record the outcome of:
   - 5 × text-only turns ("What time of day is it usually darkest?")
   - 5 × photo turns, releasing ENTER immediately after pressing P
   - 5 × photo turns, releasing ENTER ~3 s after P
   - 3 × quick taps of ENTER
   - 1 × Escape while "Finishing recording..." waits for a photo
4. Confirm: Ephemeral Mode is active immediately at launch; no spoken
   "i'm listening"/"i'm thinking"; speaking the first word immediately after
   pressing ENTER appears in the transcript.
5. Pass: every non-tap turn produces an answer; taps show the too-short
   notice; no duplicate or out-of-order lines; Escape sends nothing.
6. In the log, confirm no `device_failure`/`dropped_frames`, and that any
   failure shows a specific class.

## 10. Acceptance criteria

- [ ] 15/15 scripted turns in §9 step 3 produce an answer.
- [ ] Releasing during photo capture never loses the recording.
- [ ] Recordings under the minimum show the too-short notice and do not run
      Whisper.
- [ ] Each status line appears once per event, in the §3 order.
- [ ] Capture failures in diagnostics carry a specific class.
- [ ] Ephemeral Mode is active at launch with no hold; `--mode private` is
      rejected.
- [ ] No Piper synthesis or playback occurs for listening/thinking status.
- [ ] Press → `microphone.recording_started` ≤ 300 ms on the Pi.
- [ ] Laptop `cargo run -- --voice` still shows live partials.
- [ ] No transcript, answer, or audio content in diagnostic logs.
- [ ] `cargo test` and `python3 -m pytest tools/tests` pass.

## 11. Documentation updates

- README "Teach Sam About the Latest Milestone".
- `docs/ARCHITECTURE.md`: capture buffering, Pi partials off,
  release-during-capture behavior, interface output ownership.
- `docs/logging.md`: capture failure classes.
- `docs/raspberry-pi-hailo-qwen3.md`: photo timing behavior.
- `docs/security/ARCHITECTURE.md` ("Physical entry policy") and
  `docs/security/THREAT_MODEL.md`: no entry gesture; no audible microphone
  indicator; Private Mode not offered.
- Pi setup docs and README run commands: no five-second hold, no cues.

## 12. Completion report

Status: **implemented on branch `pi-voice-reliability`; awaiting owner review
and the §9 Pi run.** Nothing is committed.

**Files changed:** `Cargo.toml`/`Cargo.lock` (`rtrb` 0.3.6, MIT OR
Apache-2.0), `src/dictation/audio.rs`, `src/dictation/controller.rs`,
`src/dictation/mod.rs`, `src/protocol.rs`, `src/app.rs`, `src/audio_to_audio.rs`,
`src/interface.rs`, `src/speak/mod.rs`, `src/speak/worker.rs`,
`src/security.rs`, `src/launch.rs`, `src/main.rs`, `src/profile.rs`; docs listed
in §11.

**What was done, by item:**

- R1: `CaptureFailureClass` (`device_unavailable`, `stream_error`,
  `dropped_frames`, `buffer_overflow`, `invalid_audio`, `lock_poisoned`) on
  `microphone.recording` / `microphone.capture.lifecycle`, plus
  `dropped_frames` and `recorded_ms` counts.
- R2: `live_partial_transcription` profile default, false for
  `raspberry-pi-hailo`; partial ticks are not scheduled and snapshot requests
  are ignored.
- R3: CPAL callback → `rtrb` SPSC ring (2 s) → capture-thread-owned `Vec`,
  drained every 100 ms; snapshots copy on the capture thread.
- R4: existing `try_submit` wait confirmed and covered by tests (success,
  failed photo, Escape while waiting).
- R5: `DictationEvent::RecordingTooShort` / `DictationOutput::TooShort` for
  < 300 ms; Whisper not run.
- R6: "Thinking..." on release only; transcript no longer resets status;
  "Ready" printed only when idle (after answer/failure); Escape prints
  "Cancelled." once; partial failures are diagnostics-only; startup prompt
  printed last.
- R6 follow-up from the first Pi run (2026-11-22): releasing during a photo
  printed "Thinking..." a second time after the transcript, because the model
  dispatch printed it again once "Photo captured" had become the last status.
  Voice mode now prints "Thinking..." only on release; covered by
  `terminal_shows_thinking_once_when_release_comes_before_the_photo`.
- R9: `scripts/run-raspberry-pi-hailo.sh` with env-overridable defaults,
  pre-flight checks (binary, model, Hailo Apps, bridge Python, Piper/voice,
  button device, curl, camera), refusal when the bridge port is already in
  use, background bridge start with bounded `/healthz` wait, foreground
  Independence with timestamped logs, and an EXIT trap that stops the bridge
  (SIGTERM, then SIGKILL after 10 s). `tools/hailo_vlm_bridge.py` now treats
  SIGTERM like Ctrl+C so the Hailo device is released in its `finally` block.
- R11: `hailo_bridge_unavailable` warning replaces the misleading "text chat
  is available" message for the `hailo-vlm` backend; `require_camera` profile
  default (true only for `raspberry-pi-hailo`) makes voice startup fail with
  `AppError::CameraRequired` before the microphone/speaker open.
- R7: entry gesture, `EphemeralEntryGesture`, `--mode`, `run_with_mode`,
  `AppError::PrivateModeUnavailable` removed; `--mode` rejected with an
  explanation. `SecurityManager` lifecycle and `OperatingMode::Private` kept.
- R8: listening/thinking cues, cue gate, cue cache, and `SpeechOwner::Draft`
  removed; microphone opens on press.

**Commands actually run (laptop, 2026-11-22):**

- `cargo test` — 187 passed, 0 failed, 3 ignored (hardware checks).
- `cargo clippy --all-targets` — no warnings.
- `cargo fmt --check` — clean.
- `python3 -m unittest tools/tests/test_hailo_vlm_bridge.py
  tools/tests/test_pi_hailo_shell_scripts.py` — 25 passed, including 10
  launcher tests with a fake bridge, camera tool, and app (`pytest` and
  `shellcheck` are not installed on this laptop).

**First Pi run (2026-11-22, 2 photo turns):** both completed; one released
during capture and correctly waited for the photo; press → microphone open
125 ms and 149 ms; no `failure_class` in the log; no transcript or answer text
in diagnostics.

**Not yet tested:** the launcher against the real bridge/HEF (actual model
load time vs the 180 s default, SIGTERM release of the real Hailo device), the
real `rpicam-still` output with and without a camera, and the rest of §9 on
the Raspberry Pi — the `evtest` button
check, the 15 scripted turns, real microphone/camera timing, and the
press → `microphone.recording_started` ≤ 300 ms target. No release build or
Pi build was run. The R3 fix is verified against a synthetic concurrent
producer, not a real USB microphone under load.

**Security/privacy review:** §7 and the threat-model updates cover the data
touched (in-memory audio only), the removed entry gesture, and the loss of an
audible microphone indicator. No new storage, network, process, or secret
handling. Logs gain fixed classes and counts only. The checklist items for
secrets/cryptography/storage are not applicable to this milestone.

**Known limits / follow-ups:** latency work (Appendix A); a non-Piper
microphone indicator (LED or short tone); the plain `raspberry-pi` profile
still runs live partials (only `raspberry-pi-hailo` was in scope).

## 13. Owner decisions (recorded 2026-11-22)

7. **Scope addition after the first Pi run:** add R9 (launcher) and R11
   (startup camera/bridge checks). R10 (clean answer endings) was proposed and
   rejected: answers are shown exactly as the model produced them, so model
   problems stay visible.
8. **Bridge lifetime:** the launcher stops the bridge when Independence exits.
9. **Camera required on the Hailo Pi:** a missing camera is a fatal startup
   error for `raspberry-pi-hailo` (launcher and application), not a warning.

1. **Live partials on the Pi:** off for `raspberry-pi-hailo`; the final
   transcript satisfies the earlier transcription-logging request on the Pi.
2. **R3 ring buffer:** do it in this milestone regardless of R1+R2 results.
3. **R4:** after an early release, wait for the photo (bounded by the capture
   timeout); never send the transcript alone.
4. **Minimum recording length:** 300 ms.
5. **Immediate Ephemeral Mode** (added 2026-11-22): remove the five-second
   entry hold; Private Mode is not offered until Ephemeral Mode is mature.
6. **No spoken status cues** (added 2026-11-22): listening/thinking are
   terminal status and diagnostics only; the microphone opens on press.

---

## Appendix A: Deferred latency findings

Kept for the follow-up latency milestone; not authorized here.

| Measurement (photo turns unless noted) | Value |
| --- | --- |
| Hailo model, 200-token cap, long answer | 45.0 s |
| Hailo model, 200-token cap, first turn, short answer | 6.5 s |
| Hailo model, 60-token cap, later turns (text / photo) | 12.7 s / 15.2 s |
| Whisper final pass | 2.3–3.5 s |
| Piper synthesis of whole answer | 2.3–8.9 s |
| Press → microphone open (cue synthesized on first use) | up to 2.5 s |
| Release → first answer audio | 12.4–21.3 s (≈57 s at 200 tokens) |

Candidate work: token cap and voice brevity prompt;
sentence-by-sentence speech; measure how much the resent conversation history
and the resent accepted photo (follow-ups carry the last photo,
`src/controller.rs:784`) add to model time; bridge streaming.
