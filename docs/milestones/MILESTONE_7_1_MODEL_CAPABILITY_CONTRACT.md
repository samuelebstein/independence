# Milestone 7.1 — Model-agnostic local inference capabilities

> **Status:** Proposed — awaiting owner approval before implementation.
>
> **Date / owner:** 2026-11-13 / Sam
>
> **Related work:** [`docs/raspberry-pi-hailo.md`](../raspberry-pi-hailo.md),
> [`src/think/mod.rs`](../../src/think/mod.rs),
> [`src/profile.rs`](../../src/profile.rs), and
> [`tools/hailo_vlm_bridge.py`](../../tools/hailo_vlm_bridge.py)

## 1. Objective

Make Independence **model-agnostic at its application boundary** while keeping
model-specific code small, explicit, and owned by the local inference backend.

A person should be able to select a reviewed Hailo model for the Raspberry Pi
Hailo profile with the existing `--model <logical model alias>` launch option.
Independence must then ask the selected local backend what it can actually do
rather than infer behavior from `qwen2`, `qwen3`, a HEF filename, or a hardware
profile name.

The first reviewed local model entries are:

| Logical alias | Initial role | Effective still-image limit | Video in this milestone |
| --- | --- | ---: | --- |
| `qwen2-vl:2b` | Existing Hailo baseline | 4 ordered images | Not supported |
| `qwen3-vl:2b` | New explicit Hailo test option | 1 image | Not supported |

The limits are the safe limits of the full local serving path, not a claim
about every theoretical capability of a model family. In particular, the
current upstream Hailo Qwen3-VL release must be treated as single-image until
the exact installed runtime and adapter prove otherwise.

At proposal time, Hailo lists Qwen3-VL 2B for Hailo-10H with a Python/C++ API
in its [Model Explorer](https://hailo.ai/products/hailo-software/model-explorer/generative-ai/qwen3-vl-2b-instruct/).
Its [GenAI model-zoo changelog](https://github.com/hailo-ai/hailo_model_zoo_genai/blob/main/docs/CHANGELOG.rst)
is the more restrictive operational source: it records Qwen3-VL 2B as a
single-frame image model in that release. These are planning inputs only; the
preflight/manual gates below must verify the installed Hailo version and the
actual model artifact before a support claim is made.

At the end of the milestone, the application will receive a versioned,
structured capability record from `/api/show`. It will use that record to:

- enable or keep camera input off;
- prevent a photo capture that cannot fit in the selected model request;
- bound image, message, and request context before sending private data; and
- distinguish a model that supports still images from one that supports video.

This does **not** mean that Independence may load arbitrary model files or let
a model decide which device, camera, file, or private data it can access.

## 2. Why this capability is needed

Today, the Rust application correctly asks a backend whether it advertises
`vision`, but the Hailo bridge itself has Qwen2-specific behavior: its default
alias, HEF discovery, `336×336` image preparation, and four-image request
limit are embedded in one implementation. The Raspberry Pi Hailo profile also
defaults to the Qwen2 logical alias.

That is enough for one known model, but it makes a second model look like a
drop-in file swap when it is not. Qwen3-VL has a different input preparation
contract and a different effective image limit. Swapping only `HAILO_VLM_HEF`
could load a model with the wrong preprocessing, advertise the wrong model
name, or allow a multi-photo request the model cannot serve.

The project goal is not a plugin framework or a model zoo inside Independence.
It is a small, honest contract:

```text
hardware profile chooses local hardware/backend
       +
launch chooses a reviewed logical model
       +
backend declares the effective capabilities and limits it actually serves
       =
Independence makes generic, bounded camera/context decisions
```

This keeps the camera, controller, voice flow, privacy boundaries, and local
HTTP client generic. Only the bridge's reviewed model registry needs to know
how a particular Hailo model is loaded, prompted, and preprocessed.

## 3. Scope

### In scope

- A versioned `independence_capabilities` extension on the existing local
  Ollama-compatible `/api/show` response.
- Typed parsing, validation, caching, and diagnostics for that extension in
  `src/think/mod.rs`.
- A generic capability/context planner that constrains photo admission and
  request shaping without branching on a model name.
- Formalizing `--profile raspberry-pi-hailo` as a hardware/backend selection;
  `--model` chooses the logical served model and defaults to `qwen2-vl:2b`.
- A reviewed Hailo bridge model registry with two entries, Qwen2-VL 2B and
  Qwen3-VL 2B. The registry owns model alias validation, artifact discovery,
  image preparation, prompt adaptation, and effective limits.
- Explicit model-aware preflight and launcher behavior for both reviewed
  models. No model download occurs without an explicit preflight command.
- Capability-aware still-photo limits in typed and button-controlled flows.
- Automated contract tests with fake backends plus a hardware/manual test
  matrix for both models.
- Documentation and operator runbooks that clearly distinguish Qwen2's
  multi-image test from Qwen3's one-image test.

### Existing behavior that must be preserved

- `--model` remains a per-launch choice and takes precedence over the Hailo
  profile default. It does not write a shell, system, or persistent setting.
- The Hailo bridge remains numeric-loopback-only and non-streaming.
- The camera remains explicitly user-triggered. A model's capability record
  never authorizes autonomous photo capture.
- Existing Qwen2 Hailo launches continue to work with the current default
  alias and retain their four-photo capability after the bridge is upgraded.
- Ordinary local Ollama models remain usable. Their standard `vision`
  capability is still honored without requiring them to implement this
  Independence-specific extension.
- Whisper, Piper, security-mode handling, accepted-context ownership,
  cancellation, and telemetry redaction remain separate from model selection.

### Expected implementation surface

| Component | Responsibility after this milestone |
| --- | --- |
| `src/launch.rs` | Continue parsing the backend-neutral `--model` alias; do not parse HEF paths or model-family flags. |
| `src/profile.rs` | Select Hailo hardware defaults and the loopback endpoint; default the logical alias but do not claim its capabilities. |
| `src/think/mod.rs` | Fetch and strictly parse `/api/show`; cache a typed capability record for one configured endpoint/model pair. |
| `src/app.rs` and controller boundary | Ask generic media/context policy whether a capture or request fits before opening the camera or dispatching bytes. |
| `tools/hailo_vlm_bridge.py` | Own the reviewed model registry, Hailo model adapter, effective capability record, and local API response. |
| `scripts/prepare_pi_hailo_vlm.sh` | Explicitly prepare/check one named reviewed model and its compatible runtime; never silently download it. |
| `scripts/run_pi_hailo_vlm_bridge.sh` | Start exactly one named reviewed model and reject an unknown/mismatched selection before opening the HTTP listener. |
| `docs/raspberry-pi-hailo.md` | Provide separate, honest Qwen2 and Qwen3 setup/acceptance paths. |

## 4. Explicitly excluded work

This milestone does **not** authorize:

- arbitrary third-party HEFs, a generic plugin loader, runtime code download,
  model conversion, compilation, or automatic updates;
- accepting a model identity merely because a filename contains `qwen2` or
  `qwen3`;
- loading Qwen2 and Qwen3 concurrently, model hot-swapping during a live
  session, or sharing an in-flight Hailo generation between models;
- camera video capture, video encoding/upload, frame sampling, live camera
  analysis, or claiming video support because a model marketing page mentions
  it;
- native model audio input/output, wake words, translation, audio-to-audio
  changes, or changes to Whisper/Piper;
- automatic summarization, model-generated context compression, persistent
  conversation storage, memory retrieval, or a model-quality benchmark;
- LAN exposure, cloud fallback, account setup, remote telemetry, or model
  artifact downloads triggered by normal Independence startup; and
- changes to Ephemeral Mode's in-memory-only retention policy.

A future video or context-summarization milestone must be separately approved.

## 5. Expected behavior

### 5.1 Selection and startup

The profile and model name have separate meanings:

```text
--profile raspberry-pi-hailo  = Pi buttons, Pi camera adapter, Hailo backend,
                                 loopback bridge endpoint
--model qwen3-vl:2b           = the logical local model this launch expects
```

The model name must be supplied consistently to the bridge and Independence.
The proposed operator pattern is a shell variable, not a hidden persistent
setting:

```bash
# Proposed interface after this milestone; not available yet.
PI_HAILO_MODEL='qwen3-vl:2b'

# terminal A: exactly one bridge/model
HAILO_VLM_MODEL="$PI_HAILO_MODEL" \
HAILO_APPS_DIR="$HOME/code/hailo-apps" \
  scripts/run_pi_hailo_vlm_bridge.sh

# terminal B: the client expects the same logical model
./target/release/independence --voice \
  --profile raspberry-pi-hailo \
  --model "$PI_HAILO_MODEL" \
  --input buttons \
  --input-device "$PI_BUTTON_DEVICE" \
  --audio-input "$PI_TEST_MIC" \
  --audio-output "$PI_TEST_SPEAKER"
```

The bridge must reject an unknown alias before opening its HTTP port. A client
request for an alias different from the bridge's served alias must fail at
`/api/show`; Independence must leave media capability unknown/disabled and
display a concise local mismatch diagnostic. It must not guess that a nearby
model is compatible.

The selected model is immutable for a bridge process and an Independence
process. Changing models means stopping terminal A, starting a new bridge, and
starting a fresh Independence session. This prevents accepted text/photos from
being quietly reinterpreted under a different model contract.

### 5.2 Capability response contract

The existing top-level Ollama-compatible `capabilities` array remains for
compatibility. The local bridge adds a namespaced object to `/api/show`:

```json
{
  "capabilities": ["vision"],
  "independence_capabilities": {
    "schema_version": 1,
    "image": {
      "supported": true,
      "max_items_per_request": 4,
      "max_bytes_per_item": 10485760,
      "max_pixels_per_item": 10000000,
      "accepted_mime_types": ["image/jpeg", "image/png"],
      "final_user_message_only": true
    },
    "video": {
      "supported": false
    },
    "request": {
      "max_messages": 16,
      "max_text_bytes": 65536,
      "max_total_bytes": 14680064,
      "max_generated_tokens": 200,
      "max_context_tokens": 2048
    }
  }
}
```

The values above illustrate the schema and must be produced from the selected
model adapter's **effective** limits. An effective limit is the lower of the
reviewed model's support, the bridge's validated operational limits, and an
Independence app-level safety ceiling. It is not an invitation for the client
to exceed its own memory or privacy limits.

Rules for schema version 1:

- `schema_version` is required and must equal `1`.
- If `image.supported` is true, every image limit shown above is required,
  positive, and bounded by an Independence hard safety ceiling.
- `video.supported` is an explicit boolean. A video limit is not meaningful
  unless video is true; version 1 serves no video-capable bridge.
- The bridge owns internal resizing/cropping and prompt formatting. It must
  not expose a model's raw tensor dimensions as a command for the generic
  camera adapter to follow.
- The record contains a logical model alias and safe limits only. It must not
  expose HEF paths, environment values, device serials, prompts, media, or
  Hailo exception strings.
- A future incompatible schema version is `Unknown`, not a reason to make up
  limits from the model name.

The Hailo bridge's `healthz` endpoint may repeat a small, safe startup summary,
but `/api/show` is the only capability authority consumed by Independence.

### 5.3 Capability interpretation and safe fallback

`src/think/mod.rs` must retain the existing three-state distinction:

```text
Supported     backend explicitly says it supports the feature
Unsupported   backend explicitly says it does not
Unknown       metadata is missing, malformed, unavailable, or unrecognized
```

For still images, the resulting behavior is:

| Backend response | Application behavior |
| --- | --- |
| `vision` plus a valid v1 image record | Enable the camera subject to the advertised effective limits. |
| `vision` only, with no extension | Preserve basic legacy still-image support, but admit at most one photo per request and do not enable video or detailed context optimization. |
| Valid extension says image unsupported | Do not list/open/capture a camera for the request. |
| `/api/show` fails, has no `vision`, or has an unknown/incompatible record without legacy `vision` | Keep camera/media input off and leave text-only requests available. |
| Malformed detailed record plus legacy `vision` | Warn without exposing response payloads, use the conservative one-photo legacy policy, and never enable video. |

The application must never convert an unknown capability into supported because
the alias looks visual. A backend's `vision` declaration permits still-image
requests only; it does not enable microphone access, speaker output, arbitrary
files, camera video, or autonomous observation.

### 5.4 Media and context planning

Before opening the camera, the application creates a generic `ContextPlan`
from the cached capability record and the already accepted temporary context.
The plan contains only the exact accepted history/photos that will be sent,
their counts/byte budgets, and a non-sensitive explanation of any limit.

The policy is deliberately conservative:

- A physical `P` press beyond `max_items_per_request` is rejected **before** a
  camera capture begins. The person sees a clear message such as “this model
  accepts one photo per question.”
- Photos retain their capture order. The app must never make a collage,
  reorder images, or silently drop an image to fit a limit.
- If previously accepted photos plus new photos cannot fit the selected
  model's cap, the request is not dispatched. The interface explains that a
  new scene/reset is needed; it does not choose a different photo on the
  person's behalf.
- Text history is added newest whole turn first within the advertised message
  and text-byte limits. If an older whole turn does not fit, it is omitted from
  that request with a visible non-content context-limit notice and a
  telemetry count only. It remains in the current in-memory session unless
  ordinary reset/shutdown clears it.
- `max_context_tokens` is information for diagnostics and a future backend
  tokenizer-aware planner. Rust must not claim an exact token count without
  the model's tokenizer. The bridge remains the final request-size guard.
- Current hard caps for retained draft bytes, HTTP request bytes, and image
  decoding remain in force even if the backend advertises larger values.

For Qwen3 in this milestone, one-image behavior is a product constraint, not
a degraded attempt at the Qwen2 comparison flow. Qwen2 remains the accepted
multi-image comparison path.

### 5.5 Model adapter behavior

The Python bridge gets a small source-controlled registry, conceptually:

```text
logical alias
  -> approved artifact locator and runtime compatibility rule
  -> model-specific prompt/frame adapter
  -> effective capability descriptor
```

The registry is not a general extension system. A model adapter may know about
Qwen2's image preparation or Qwen3's image preparation; the Rust application
must not. It produces the generic normalized prompt/frame contract accepted by
the Hailo GenAI API and clears Hailo context after each completed/failed
request as the current bridge does.

`HAILO_VLM_HEF` remains an explicit operator override only if it is paired with
a reviewed registry alias. The bridge must reject an unknown alias, a missing
artifact, an incompatible runtime, or a model/alias mismatch. Filename matching
may locate a reviewed artifact but is never proof of model identity. A real
first inference remains the acceptance proof.

## 6. Implementation boundaries

### State and I/O ownership

```text
CLI --profile/--model
        |
        v
profile resolver: hardware defaults + expected local alias only
        |
        v
Ollama-compatible client -- POST /api/show --> loopback Hailo bridge
        ^                                      |
        |                                      v
typed capability cache <--- validated descriptor/model registry/adapter
        |
        v
ContextPlan -> Controller/VoiceRuntime -> explicit camera capture -> /api/chat
```

| Owner | Owns | Must not own |
| --- | --- | --- |
| Launch/profile resolver | Per-launch model alias, endpoint, hardware adapter selection | HEF paths, model capability claims, media bytes |
| Hailo bridge registry | Reviewed model mapping, artifact/runtime validation, frame/prompt adaptation, effective capability limits | Conversation retention, camera consent, controller state |
| `OllamaClient` / capability cache | One `/api/show` fetch and strict metadata parsing for one endpoint/model pair | Model-name heuristics or hardware access |
| Context planner/controller | Media admission, request context selection, visible limit behavior, cancellation identity | Hailo preprocessing or model-specific templates |
| Camera service | One explicitly authorized fresh still capture | Selecting a model or deciding whether a model supports vision |
| Security manager | Ephemeral lifecycle and ordinary cleanup authority | Model selection or serving process control |
| Telemetry | Counts, safe model alias, schema version, timing/outcome | Prompt text, transcript, answer, HEF path, media bytes/base64 |

The client remains loopback-only, disables ambient HTTP proxy use, and refuses
redirects. The bridge is a separate local process, not a trusted persistence
boundary; it must retain its existing no-payload logging policy.

### Concrete implementation plan

1. Define typed capability structures in Rust, including strict parsing of the
   optional namespaced `/api/show` object and an explicit conservative legacy
   still-image policy.
2. Move generic media/context admission into a small testable policy object.
   Wire it before both button photo capture and typed camera request dispatch.
3. Replace Hailo bridge constants with a reviewed `ModelAdapter` registry.
   Each adapter owns its Hailo frame preparation, prompt mapping, effective
   limits, artifact discovery rule, and model alias.
4. Make the prepare/launcher scripts select a registry alias explicitly and
   reject unsupported combinations before launching Python.
5. Extend `/api/show`, bridge startup output, and metadata-only telemetry with
   the selected alias and capability schema version/limits.
6. Preserve the existing `--model` path through `VoiceLaunchOptions` and
   `ResolvedVoiceProfile`; remove Qwen2-specific assumptions from Rust outside
   the Hailo profile's default alias and tests/docs.
7. Add Qwen2/Qwen3 fake-contract tests first, then execute the isolated
   hardware tests in the manual procedure below.

No new global model manager, database, background polling worker, or dynamic
plugin interface is justified by this milestone.

## 7. Security and privacy threat review

This review follows the [project security review checklist](../security/REVIEW_CHECKLIST.md).
It is a proposed review, not evidence that the implementation is already safe.

### Assets and trust boundaries

| Asset | Boundary / risk | Required protection |
| --- | --- | --- |
| Fresh camera images and accepted session photos | Camera process, Rust process, bridge process, Hailo runtime, OS buffers | Explicit capture only; bounded in-memory request; no bridge payload logs; ordinary Ephemeral cleanup remains truthful about OS/runtime residuals. |
| Spoken transcript and conversation context | Whisper/Rust/bridge request boundary | Send only the controller-authorized current request; no full text in telemetry or errors. |
| Model artifact and model identity | Filesystem, Hailo runtime, local operator | Source-controlled alias registry, explicit preparation, runtime compatibility check, no automatic download, no filename trust. |
| Availability / RAM / accelerator time | Malformed local server response, huge media, concurrent Hailo workload | App and bridge hard ceilings; numeric-loopback policy; serialized model use; clear failure rather than retry storms. |
| Capability metadata | A local service can lie or be stale | Treat it as a bounded operational contract, parse strictly, clamp limits, fail closed for new media features, and do not turn it into authority over user consent. |

### Required protections

- Keep `127.0.0.1`/`::1` enforcement, no proxy inheritance, and redirect
  refusal for the local backend client.
- Require an explicit `--model`/bridge alias match. Do not infer a capability
  from an HEF filename, Hailo model family, or hardware profile.
- Clamp every advertised size/count against application-controlled maxima
  before allocation, camera capture, image encoding, or request construction.
- Reject a photo above the effective count before invoking `rpicam-still` or
  another camera backend; do not capture and then discard it.
- Avoid all prompt/image/transcript/answer/HEF-path content in normal bridge
  output and optional JSONL. Error text exposed over HTTP remains generic.
- Do not let a model advertisement create an action capability. `vision` only
  gates a user-requested still image; it never grants file, video, mic, or
  network access.
- Keep model selection launch-scoped. Do not persist selected aliases or use
  telemetry as a model history.

### Residual risks

- Any local process with the user's OS privileges can inspect a loopback
  request or model artifact; loopback is not a cryptographic boundary.
- An image can contain prompt-injection text. The model response remains
  untrusted dialogue and cannot bypass controller/device authorization.
- The OS, camera driver, Hailo runtime, terminal, swap, or crash handling may
  retain data outside Independence's in-memory cleanup claims.
- File-name and runtime checks reduce configuration mistakes but cannot prove
  a model artifact is benign. The owner must approve the source/version/check
  procedure for each registry entry.

### Owner approvals required

1. Approve Qwen2-VL 2B and Qwen3-VL 2B as the first two reviewed registry
   entries, including their official artifact source, runtime version, disk
   budget, and update/revocation procedure.
2. Approve the conservative legacy policy: a vision-only backend without the
   extension receives at most one still image per request. This may narrow an
   undocumented multi-image behavior of third-party Ollama servers.
3. Approve explicit rejection/reset rather than automatic photo selection when
   accepted visual context exceeds a model's cap. This avoids silently changing
   the meaning of a comparison question.
4. Approve Qwen3 as a single-still-image test path for this milestone. Video
   and Qwen3 multi-image behavior remain out of scope until separately proven.

## 8. Automated tests

All ordinary tests must use fake HTTP/Hailo/camera backends. They must not
download models, open hardware, inspect real HEFs, capture personal media, or
call a network service.

| Layer | Test | Invariant proven |
| --- | --- | --- |
| Rust capability parser | Valid v1 Qwen2/Qwen3 descriptors parse into the same generic type. | Rust has no model-name branch to determine media support. |
| Rust capability parser | Missing, malformed, future-version, contradictory, negative/zero, or over-hard-cap fields are unknown/rejected safely. | Untrusted metadata cannot expand media access or allocation limits. |
| Rust legacy compatibility | `capabilities:["vision"]` without the extension permits one still image; no `vision` or failed show keeps media off. | Existing simple local Ollama operation remains useful without guessing multi-image/video support. |
| Profile/CLI | `--profile raspberry-pi-hailo --model qwen2-vl:2b` and `qwen3-vl:2b` resolve aliases/loopback endpoint without declaring capabilities. | Hardware profile and model capability ownership stay separate. |
| Context planner | Qwen2 admits ordered photos through its effective cap; Qwen3 blocks a second `P` before invoking fake camera capture. | No silent image loss or unnecessary capture. |
| Context planner | Exceeding accepted-photo/message/text budgets produces a visible generic limit result and sends no request. | Context cannot exceed the declared/effective contract. |
| Controller/runtime | Cancellation, stale completion, and Ephemeral cleanup keep their existing semantics with capability-limited requests. | New planning does not weaken turn identity or cleanup. |
| Python model registry | Unknown alias, missing artifact, incompatible runtime, or alias/HEF mismatch fails before listening/inference. | Arbitrary files are not treated as reviewed models. |
| Python adapters | Qwen2 and Qwen3 receive their respective prepared-frame shape/prompt mapping through fakes. | Model-specific behavior stays inside the bridge. |
| Python HTTP contract | `/api/show` returns legacy `vision` plus valid v1 descriptor; `/api/chat` rejects over-limit media before fake backend invocation. | The server is the single source of effective constraints. |
| Python privacy tests | Startup/API telemetry contains alias/counts/limits/outcomes only. | No text, image data, HEF path, or response leaks into bridge telemetry. |

Run at minimum:

```bash
cargo fmt --check
cargo test --locked
cargo clippy --all-targets -- -D warnings
python3 -m unittest tools.tests.test_hailo_vlm_bridge
git diff --check
```

Add targeted tests for every new capability state and every failure path above;
the command list alone is not an acceptance claim.

## 9. Manual verification procedure

These steps are planned verification after implementation. They deliberately
separate model installation, bridge behavior, and Independence behavior so a
failure identifies one layer.

### A. Establish the local Hailo baseline

On the Pi, with a monitor/keyboard available for recovery if needed:

```bash
hailortcli fw-control identify
dpkg-query -W hailo-h10-all
```

This reads the local accelerator/runtime state. It must identify Hailo-10H.
Record the installed package version because Qwen3 availability depends on the
compatible Hailo runtime/model-zoo release. Stop Hailo-Ollama and any other
Hailo GenAI process before starting either VLM bridge.

### B. Explicitly prepare each reviewed model

The implemented script will expose an explicit model selection. Its final
spelling is part of this milestone's acceptance test; the intended form is:

```bash
# These are proposed commands, not commands available before implementation.
HAILO_APPS_DIR="$HOME/code/hailo-apps" \
  scripts/prepare_pi_hailo_vlm.sh \
  --model qwen2-vl:2b --install-python-dependencies --download-model

HAILO_APPS_DIR="$HOME/code/hailo-apps" \
  scripts/prepare_pi_hailo_vlm.sh \
  --model qwen3-vl:2b --install-python-dependencies --download-model
```

These commands require the network and consume local disk space. Each must
print the selected alias, compatible runtime check, and one verified model
artifact. It must never silently substitute one model for the other.

### C. Test each bridge without Independence

For each alias, start one bridge in terminal A and leave it running. In
terminal B, request `/healthz`, `/api/show`, and a bundled non-personal sample
image. Record only whether the commands passed and their non-sensitive model
alias/limit metadata.

Qwen2 acceptance:

- `/api/show` reports `vision`, schema version 1, and an effective still-image
  cap of four.
- A four-image ordered request succeeds.
- A fifth image returns a concise local over-limit error before Hailo inference.

Qwen3 acceptance:

- `/api/show` reports `vision`, schema version 1, and an effective still-image
  cap of one; video is explicitly false.
- A one-image request succeeds.
- A two-image request returns a concise local over-limit error before Hailo
  inference. It must not silently choose one image.

Stop terminal A with Ctrl+C before starting the other model. Do not treat one
model's successful response as proof that the other artifact/runtime pair is
valid.

### D. Test Independence with base hardware already verified

Recreate `PI_BUTTON_DEVICE`, `PI_TEST_MIC`, `PI_TEST_SPEAKER`, and
`PI_PIPER_BIN` in the current SSH shell. Test Qwen3 first with exactly one
fresh physical `P` photo during a held Talk turn and ask a visual question.
Expected result:

```text
physical button -> local mic/Whisper -> verified one-photo capability
-> one fresh in-memory camera still -> loopback Qwen3 bridge -> local answer/Piper
```

Then start the Qwen2 bridge, use the matching `--model qwen2-vl:2b`, capture
two fresh ordered `P` photos in one held Talk turn, and ask a comparison
question. The existing four-image limit may be separately tested only after
the two-image comparison succeeds.

For both models, pressing Escape must preserve existing cancellation behavior;
the app must not dispatch a late answer or retain newly rejected/cancelled
photos as accepted context.

### E. Verify local telemetry boundaries if opted in

If bridge JSONL telemetry is explicitly enabled, inspect only field names,
model alias, schema version, counts, timings, and outcome. Confirm it contains
no spoken question, transcript, model answer, HEF path, JPEG/PNG bytes, or
base64. Terminal scrollback remains a separate exposure because it displays
dialogue intentionally.

## 10. Acceptance criteria

This milestone is complete only when all of the following have evidence:

- [ ] `--profile raspberry-pi-hailo` chooses hardware/backend defaults but
  does not hardcode a capability claim; `--model` selects a reviewed logical
  alias for the launch.
- [ ] The Hailo bridge refuses an unknown alias, invalid artifact selection,
  or incompatible runtime before serving HTTP.
- [ ] `/api/show` exposes valid, versioned, redacted effective constraints for
  each supported bridge model, while retaining the standard `vision` list.
- [ ] The Rust client parses capabilities by schema, not by model-family name,
  and caches them per endpoint/model pair.
- [ ] Qwen2 preserves ordered multi-image requests through its declared cap;
  Qwen3 accepts one image and rejects a second before capture/inference.
- [ ] A missing/malformed capability extension cannot enable video, increase
  a hard media limit, or cause camera capture when `vision` is unknown.
- [ ] Generic legacy `vision` servers retain a documented conservative
  one-image path or the owner-approved alternative is implemented and tested.
- [ ] Context-budget behavior is visible and deterministic; no photo is
  silently reordered, collaged, discarded, or sent beyond the selected cap.
- [ ] Qwen2 and Qwen3 complete their separate bundled-image bridge tests and
  their separate live Pi acceptance tests.
- [ ] Existing cancellation, Ephemeral cleanup, loopback-only networking, and
  payload-redaction tests pass unchanged or with reviewed equivalent coverage.
- [ ] Documentation makes no claim that video, arbitrary HEFs, concurrent
  model execution, or untested Hailo artifacts are supported.

## 11. Documentation updates

Implementation must update:

- `README.md` — explain that `--profile` chooses hardware and `--model`
  chooses a served logical model; link to capability semantics.
- `LEARNING.md` — explain capability discovery, effective versus theoretical
  limits, conservative legacy behavior, and why preprocessing is backend-owned.
- `docs/ARCHITECTURE.md` — document the capability cache, context planner,
  model registry boundary, and no-hot-swap rule.
- `docs/raspberry-pi-hailo.md` — replace the single-Qwen2 procedure with
  explicit Qwen2/Qwen3 preflight, bridge, and hardware acceptance gates.
- `docs/ROADMAP.md` — update this milestone from proposed to implemented only
  after evidence is recorded.
- Inline CLI help and bridge `--help` — describe only supported aliases and
  the fact that a model name is not a free-form HEF loader.

## 12. Completion report

The completion report must record:

- the exact supported model aliases, artifact source/version/check procedure,
  Hailo runtime versions, and effective capability records tested;
- files changed and why each model-specific branch remains confined to the
  bridge registry;
- automated commands actually run and their results;
- Qwen2 and Qwen3 bridge/manual hardware results separately, including any
  failed gate and its diagnostic category;
- whether the legacy vision-only fallback and over-cap visual-context policy
  received the owner approvals listed above;
- privacy review evidence, including telemetry inspection; and
- known limitations, especially Qwen3 one-image behavior and the fact that
  video remains unimplemented.

Do not begin video support, arbitrary-model loading, automatic model download,
or persistent model preferences as part of this milestone. Each requires a
separate approved specification.
