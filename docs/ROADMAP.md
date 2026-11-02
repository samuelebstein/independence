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

Milestone 6.2 now records the proposed authentication and key-management
architecture in
[security/AUTHENTICATION_AND_KEY_MANAGEMENT.md](security/AUTHENTICATION_AND_KEY_MANAGEMENT.md).
It is an architecture document awaiting owner review, not an implementation of
PIN authentication, cryptography, TPM support, or backups.

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
| Complete | Raspberry Pi setup guide, evdev button adapter, `rpicam-still` adapter | `48dba28`, `cff0ff6`, `docs/rasberry-pi.md` |
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

Milestone 6.2 records the established product decisions for independent local
PIN/hardware and recovery-phrase paths, and leaves the remaining implementation
details explicit: a compatible TPM/secure-element, measured-boot policy,
Argon2id calibration, BIP-39 passphrase policy, storage format, and backup
rollback/retention policy. None was needed to safely add an unauthenticated,
memory-only mode, and none authorizes cryptographic implementation yet.

## Security architecture and next-step status

Each implementation milestone must be independently understandable,
automatically testable, manually verifiable, privacy-reviewed, and documented
before the next begins. No follow-up implementation scope is approved yet.

| Milestone | Objective | Depends on |
| --- | --- | --- |
| 6.1 — Security state and initialization | **Implemented:** state boundary and Ephemeral Mode, without private-storage migration | M5.5 design; manual Pi-button verification remains |
| 6.2 — Authentication & key-management architecture | **Documented; awaiting owner review.** Defines the independent local hardware/PIN and portable recovery-phrase paths, and makes cloud backups recovery-only. No production code is added. | 6.1; [6.2 research](milestones/MILESTONE_6_2_RESEARCH.md) |
| Next implementation milestone | **Not defined.** It may be proposed only after the owner reviews and approves the M6.2 architecture. | Approved M6.2 architecture and an explicitly approved scope |

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
