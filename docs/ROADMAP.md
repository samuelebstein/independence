# Independence roadmap

This roadmap distinguishes evidence from intention. A checked capability is
grounded in the current code and Git history; a proposed milestone is not an
implementation commitment until the project owner approves it.

## Current status

The repository is a working local terminal prototype with typed questions,
explicit fresh-camera input, local dictation and hold-to-talk voice input,
local Ollama inference, optional local Piper playback, telemetry, and Linux/
Raspberry Pi hardware adapters. Milestone 6.1 adds a process-local security
lifecycle boundary and makes **Ephemeral Mode** the default: temporary context
is isolated in memory and cleared on ordinary shutdown. It does **not** yet
have authentication, encrypted private storage, persistent lock state,
recovery keys, or encrypted backups.

## Delivered capabilities evidenced by history

The older work was not consistently numbered as formal milestones, so this
table avoids retroactively inventing milestone numbers. It records the
delivered capability and the relevant commits/docs instead.

| Status | Delivered capability | Evidence |
| --- | --- | --- |
| Complete | Project vision and local-model learning material | `VISION.md`; early commits beginning `4163d10` |
| Complete | Typed Ollama question path, explicit `/look`, staged camera stills, accepted scene/history, cancellation guards | `cacbb02`, `7767e4d`, `src/controller.rs`, `src/see/`, `src/think/` |
| Complete | Local Whisper dictation and explicit voice bridge | `563e616`, `59a75fa`, `src/dictation/`, `src/app.rs` |
| Complete | Simple hold-to-talk voice/photo interaction restored after automatic-turn experiments | `cbbca77`, `4802af6`, `src/listen/`, `src/app.rs` |
| Complete | Local Piper/HAL speech queue and cancellation | `5bb0ebd`, `src/speak/` |
| Complete | Correlated redacted telemetry/JSONL/optional OTLP | `40fabae`, `src/telemetry.rs` |
| Complete | Raspberry Pi setup guide, evdev button adapter, `rpicam-still` adapter | `48dba28`, `cff0ff6`, `docs/raspberry-pi.md` |
| Complete | Runtime model capability discovery and isolated audio-to-audio fixture path | `e878e09`, `e12f267`, `d570372`, `docs/audio-to-audio.md` |
| Complete | Current learning/vision essays | `a955012` and `docs/` |
| Implemented; manual hardware check pending | 6.1 security lifecycle boundary and default Ephemeral Mode | `src/security.rs`, `src/controller.rs`, `docs/milestones/MILESTONE_6_1_SECURITY_FOUNDATION_EPHEMERAL_MODE.md` |

## Milestone 5.5 — Security architecture and threat model

**Status: architecture completed; its non-cryptographic foundation is now used
by Milestone 6.1.**

The analysis and documentation are complete in
[milestones/MILESTONE_5_5_SECURITY_ARCHITECTURE.md](milestones/MILESTONE_5_5_SECURITY_ARCHITECTURE.md),
[security/THREAT_MODEL.md](security/THREAT_MODEL.md), and
[security/ARCHITECTURE.md](security/ARCHITECTURE.md). They deliberately do not
implement encryption, authentication, storage, or recovery.

The remaining approval questions are material for the cryptographic and
persistent-data work: PIN policy, recovery-passphrase policy, storage-engine
choice, backup/export model, hardware-rooted protection, and the
release/review standard for key-management code. They must be resolved before
Milestone 6.2 or later, but they were not needed to safely add an
unauthenticated, memory-only mode.

## Proposed security milestones

Each milestone must be independently understandable, automatically testable,
manually verifiable, privacy-reviewed, and documented before the next begins.

| Milestone | Objective | Depends on |
| --- | --- | --- |
| 6.1 — Security state and initialization | **Implemented:** state boundary and Ephemeral Mode, without private-storage migration | M5.5 design; manual Pi-button verification remains |
| 6.2 — Master key and recovery phrase | Generate a DEK, create versioned PIN/recovery envelopes, and implement a safe recovery ceremony boundary | 6.1; approved key/envelope format |
| 6.3 — PIN authentication and recovery | Implement PIN-derived unwrapping, controlled recovery, rewrapping, and rate-limit policy | 6.2; approved PIN policy |
| 6.4 — Encrypted local storage | Put private application data behind the security manager and chosen encrypted-storage format | 6.1–6.3; approved storage decision |
| 6.5 — Device lifecycle and physical controls | Lock/sleep/wake/shutdown behavior and safe physical controls on Linux and Pi | 6.4; platform review |
| 6.6 — Authentication capabilities | Add runtime discovery and adapters for supported authenticators without treating biometrics as encryption keys | 6.3; per-platform capability review |
| 6.7 — Encrypted backup and restoration | Create authenticated export, nondestructive restore, and recovery-envelope migration | 6.2 and 6.4; approved backup policy |

## Proposed inference/runtime milestone

| Milestone | Objective | Depends on |
| --- | --- | --- |
| 7.1 — Model-agnostic local inference capabilities | Let a local backend declare versioned, effective media/context constraints so Independence can select a reviewed Hailo model with `--model` and make generic, bounded camera/context decisions | Owner approval of [`MILESTONE_7_1_MODEL_CAPABILITY_CONTRACT.md`](milestones/MILESTONE_7_1_MODEL_CAPABILITY_CONTRACT.md); verified Qwen2/Qwen3 artifacts and Hailo runtime compatibility |

## Rules for every future milestone

Use [MILESTONE_TEMPLATE.md](milestones/MILESTONE_TEMPLATE.md). In particular,
every milestone must state:

1. what user-visible capability it adds and what it deliberately excludes;
2. who owns state, I/O, cancellation, and private material;
3. its security/privacy threat review and residual risks;
4. automated tests that exercise the production boundary with fakes; and
5. manual steps that distinguish a real hardware/model test from a mock test.

This keeps a “small milestone” meaningful: a person should be able to read it,
test it, see it work, and understand what remains before the next change.
