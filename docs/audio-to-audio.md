# Audio-to-audio experiment

This is an intentionally narrow experiment in Independence. It sends one
completed, in-memory microphone recording to a direct-audio backend and plays
one complete in-memory WAV response. It does **not** transcribe speech, make a
text prompt, use Piper or the HAL voice, attach a photograph, retain
conversation history, or decide when to listen.

The existing text pipeline remains the default. Select this experiment only
with `--pipeline audio-to-audio`.

```text
TalkPressed -> CPAL captures mono f32 samples at the actual device rate
TalkReleased -> recording is complete and the device is released
                              |
                    selected voice pipeline
                     /                  \
                    /                    \
transcribe-and-speak                     audio-to-audio
 Whisper requires 16 kHz                 direct-audio backend receives
 -> text Ollama -> Piper                 recording + sample-rate metadata
 -> Rodio playback                       -> complete WAV -> Rodio playback
```

The common endpoint is the existing selected Rodio/CPAL output device. The two
branches intentionally do not share Whisper or Piper work.

## Three independent choices

The launch configuration deliberately separates three questions:

```text
--pipeline  How does this interaction flow through Independence?
--backend   Which runtime serves the selected model or test fixture?
--model     Which model should that runtime select?
```

`ollama` is the default backend for both pipelines. `fixture` is an explicit,
model-less test backend for `audio-to-audio`; it returns a known WAV and does
not load, contact, or interpret the configured model. A hardware profile only
chooses device defaults—it does not choose any of these three values.

## What starts where

For the default `transcribe-and-speak` pipeline, a held Talk button causes
`CpalRecorder` in `src/dictation/audio.rs` to capture bounded samples. Its
callback converts the device sample representation to mono `f32`, but retains
the negotiated sample rate in `protocol::CapturedAudio`. When a final
recording reaches `DictationRuntime::start_transcription` in
`src/dictation/mod.rs`, `prepare_for_whisper` converts it to Whisper's required
16 kHz mono format. `WhisperTranscriber` recognizes text, the existing
controller sends that text to Ollama, and `SpeechService` sends authorized text
to Piper before `RodioPlayback` plays the returned WAV.

For `audio-to-audio`, `AudioToAudioRuntime` in `src/audio_to_audio.rs` receives
the same `TalkPressed`/`TalkReleased` events and the same final
`CapturedAudio`, but sends it directly to an `AudioAnswerBackend`. It never
constructs `DictationSession`, `WhisperTranscriber`, `SpeechService`, Piper, a
camera service, or a text-model turn. The existing `Controller` still assigns a
`TurnId` and decides whether a returning WAV is current. Only an accepted
`Effect::PlayAudio` reaches `DirectPlaybackService`, which is a WAV-only worker
around the existing Rodio adapter.

That separation is why cancelling a recording or beginning another one can
make late audio harmless: backend workers return bytes; they cannot play them
or update text conversation state themselves.

## Launch the fixture first

Run this from the repository root. It opens the selected/default microphone
only while Talk is held and opens the selected/default speaker only for output.

```bash
cargo run --release -- --voice \
  --pipeline audio-to-audio \
  --backend fixture
```

The terminal adapter uses the existing hold-to-talk controls:

1. Hold Enter to record.
2. Release Enter to submit that recording.
3. The fixture validates the in-memory capture and returns a short, generated
   440 Hz WAV tone. Hearing that tone proves the application route from capture
   through the direct backend to playback; it does **not** prove speech
   recognition or model intelligence.
4. Press Escape to cancel current capture, backend work, and direct playback.
   Hold Enter again to cancel an unfinished response and start a fresh one.
5. `P` is deliberately ignored in this mode. No camera service is constructed
   and no photograph can be captured.
6. Ctrl+C releases app-owned capture/playback work and exits.

The fixture does not contact Ollama, load a Whisper model, start Piper, or
load the HAL voice. It returns no text answer. A normal hardware test needs a
working microphone and selected output device, but no local language or speech
model download.

To choose hardware explicitly, first list it without opening a stream:

```bash
cargo run --release -- --audio-input-devices
cargo run --release -- --audio-devices
```

Then pass exact IDs reported by those listings:

```bash
cargo run --release -- --voice \
  --pipeline audio-to-audio \
  --backend fixture \
  --audio-input 'Exact CPAL input id' \
  --audio-output 'Exact CPAL output id'
```

For the Raspberry Pi button adapter, the same pipeline has no separate
application or controller:

```bash
cargo run --release -- --voice \
  --profile raspberry-pi \
  --input buttons \
  --input-device /dev/input/by-id/usb-Example-event-kbd \
  --audio-input 'Exact CPAL input id' \
  --audio-output 'Exact CPAL output id' \
  --pipeline audio-to-audio \
  --backend fixture \
  --model gemma3:1b
```

The explicit model in that fixture command demonstrates configuration
precedence; the fixture itself does not contact or load that model.

## Configuration and capability discovery

Hardware profiles choose hardware defaults only. Pipeline, backend, and model
are independent choices:

```text
--model               > INDEPENDENCE_MODEL > OLLAMA_MODEL > profile model default
--backend             > INDEPENDENCE_BACKEND > ollama default
--pipeline             > transcribe-and-speak default
--audio-input/output   > corresponding environment variable > profile/default
```

`--model` and `--backend` are `--voice` options. For example, the existing
text pipeline can be made explicit without changing global configuration:

```bash
cargo run --release -- --voice \
  --pipeline transcribe-and-speak \
  --backend ollama \
  --model gemma3:1b
```

When `--backend ollama` is selected, Independence first calls the chosen local
server's `POST /api/show` once and caches its response for the session. It
never guesses vision or audio support from a model name. A successful `vision`
capability enables the existing camera path; an absent capability or
unavailable metadata keeps the camera off. There is no application-side
image-support setting: capabilities come from the selected backend/model at
runtime.

Capability metadata is necessary but not sufficient for a pipeline: the
backend adapter must also implement a verified request/response format. For
example, a model may advertise audio understanding without producing a WAV
answer, while a speech model may produce a WAV from text without accepting a
recording. Independence enables a direct audio-to-audio exchange only when
both requirements are established.

Use this command only to see the direct-audio capability diagnosis; it does
not open the microphone or transmit a recording to Ollama when the required
direct audio exchange is unavailable:

```bash
cargo run --release -- --voice \
  --pipeline audio-to-audio \
  --backend ollama \
  --model qwen3-vl:4b-instruct
```

The current Ollama adapter has no verified direct recording-to-WAV exchange, so
it reports that gap and does **not** silently substitute Whisper plus Piper.
The local Ollama version and model remain untouched. `--pipeline
audio-to-audio` with no backend flag therefore selects Ollama and reports its
compatibility result; use `--backend fixture` for the plumbing proof.

## Current bounds and privacy

- CPAL capture is bounded by the existing 30-second recording limit and raw
  sample limit. It is in memory only.
- The audio-to-audio backend receives mono `f32` samples plus their actual
  sample rate. It may request a future backend-specific conversion, but this
  experiment does not relabel or resample them as Whisper audio.
- The fixture output is a generated WAV, not a copy of the microphone input.
  It is held in memory and not saved.
- There is at most one active controller-authorized direct-audio request and a
  bounded one-item direct playback queue. Escape and a new talk press
  invalidate old output before it can be accepted for playback.
- Logs and telemetry record IDs, format/count/byte sizes, and outcomes—not raw
  audio samples, transcript text, image payloads, or model prompts.

## Real audio-to-audio backend status

No real bidirectional audio model or alternate runtime was downloaded,
installed, or run for this milestone. The fixture verifies the application
plumbing only. The currently selected Ollama backend fails honestly rather
than sending audio to an undocumented endpoint.

Qwen's Qwen3-Omni-Instruct family is a possible later local experiment because
its upstream project describes audio understanding and speech generation. It
uses a separate model/runtime stack and needs a deliberate hardware, memory,
license, and dependency review before installation; it is not an Ollama
fallback and is not integrated here. See the upstream
[Qwen3-Omni project](https://github.com/QwenLM/Qwen3-Omni) and the official
[Ollama model-details API](https://docs.ollama.com/api-reference/show-model-details)
and [speech capability documentation](https://docs.ollama.com/capabilities/speech)
for the capability terminology used above.

## What the automated tests prove

The ordinary Rust suite uses a synthetic recorder, direct backend, and fake
playback device. It verifies that a 48 kHz capture reaches the direct backend
unchanged, the backend's exact returned WAV reaches playback, malformed audio
does not play, and Escape/new Talk presses make both already-returned and
late-returning audio stale. These tests do not activate a microphone, speaker,
camera, Whisper model, Piper executable, Ollama server, or Qwen3-Omni model.
