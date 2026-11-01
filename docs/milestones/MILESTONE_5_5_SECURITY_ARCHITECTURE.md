# Milestone 5.5 — Security architecture and threat model

> **Status:** analysis complete; awaiting project-owner approval before
> implementation. This milestone is documentation, research, and architecture
> only.

## Objective

Create a reviewable security architecture for future personal Independence data
without modifying the application, implementing cryptography, adding
dependencies, changing runtime behavior, downloading models, or installing
system packages.

## Why this milestone exists

The current local prototype is deliberately ephemeral: conversation/photo state
lives in process memory and disappears at exit. That minimizes persistence, but
it is not the same as protecting a future personal memory system. Before
Independence stores anything private, it needs a specific answer to basic
questions: what key encrypts it, how a person unlocks it, how recovery works,
what a thief can do with a copied disk, and where the architecture cannot help.

## Scope and exclusions

### In scope

- Current-code audit of controller, voice/dictation, camera, model, speech,
  telemetry, Raspberry Pi profile, data lifetime, tests, and Git history.
- Threat model with assets, trust boundaries, attackers, scenarios, goals, and
  residual risks.
- Recommended master-DEK, PIN, BIP-39 recovery, storage, lifecycle, and backup
  architecture.
- Evaluation of standards, candidate Rust libraries, Linux/Pi capabilities, and
  alternatives.
- Permanent project instructions, project/architecture/roadmap documentation,
  reusable milestone template, and security review checklist.

### Explicitly excluded

- Application code, cryptographic code, dependencies, Cargo files, native
  package changes, model changes, system services, user accounts, storage
  migration, recovery UI, PIN UI, network services, telemetry changes, and
  automated commits.
- Beginning Milestone 6.1 or any later implementation milestone.

## Code-grounded findings

The audit found a useful existing event/controller design:

- **src/controller.rs** owns accepted in-memory history/photos and authorizes
  results through TurnId checks.
- **src/dictation/** keeps CPAL capture and Whisper recognition local and
  bounded; RecordingId rejects stale results.
- **src/see/** stages validated file/camera photos in memory, releases capture
  before inference, and preserves accepted scenes on failures.
- **src/think/** talks to configured loopback-only Ollama HTTP, with no cloud
  fallback.
- **src/speak/** logs authorized transcript/answer text to the terminal and
  optionally invokes local Piper/Rodio playback.
- **src/telemetry.rs** emits redacted JSONL/OTLP only when configured, but it is
  not encrypted storage and it does not make terminal scrollback private.
- **src/profile.rs**, **src/listen/evdev.rs**, and **src/see/camera.rs** provide
  Linux/Raspberry Pi adapters without a second application.

What is absent is equally important: no authentication, DEK, encrypted store,
locked state, recovery phrase, encrypted backup, secure enclave integration,
or persistent personal memory exists today.

## Deliverables

| Deliverable | Purpose |
| --- | --- |
| [AGENTS.md](../../AGENTS.md) | Permanent local-first, milestone, privacy, and crypto-development instructions |
| [PROJECT.md](../PROJECT.md) | Project purpose, values, and non-goals |
| [ARCHITECTURE.md](../ARCHITECTURE.md) | Actual current architecture and data paths |
| [ROADMAP.md](../ROADMAP.md) | Evidence-based completed capability map and proposed 6.1–6.7 sequence |
| [MILESTONE_TEMPLATE.md](MILESTONE_TEMPLATE.md) | Reusable small-milestone structure |
| [THREAT_MODEL.md](../security/THREAT_MODEL.md) | Assets, attackers, trust boundaries, scenarios, and residual risk |
| [security/ARCHITECTURE.md](../security/ARCHITECTURE.md) | Key hierarchy, ceremony, storage, lifecycle, backup, platform, and library proposal |
| [REVIEW_CHECKLIST.md](../security/REVIEW_CHECKLIST.md) | Future-milestone security/privacy gate |

## Recommended architecture in one paragraph

Generate one random 32-byte master DEK. Encrypt it independently in a
PIN-derived envelope and a BIP-39-recovery-derived envelope. Derive the PIN KEK
with calibrated Argon2id; derive a recovery KEK from BIP-39's standard seed
operation followed by context-bound HKDF. Use XChaCha20-Poly1305 to authenticate
and encrypt envelopes and private records, with fresh random nonces and
authenticated format metadata. Keep the DEK only in a SecurityManager while
Unlocked; give the controller a narrow storage capability rather than the raw
key. Combine application-level encrypted storage with LUKS2 where appropriate,
but do not confuse either with protection from a compromised running OS.

## Standards and candidate-library assessment

| Area | Proposed standard/approach | Candidate Rust/library direction | Important qualification |
| --- | --- | --- | --- |
| Recovery mnemonic | BIP-39, 24 words, standard mnemonic-to-seed operation | rust-bitcoin bip39 | A phrase is a bearer secret and needs an envelope; no custom mnemonic scheme |
| PIN KDF | Argon2id with per-envelope random salt and stored parameters | RustCrypto argon2 | Parameter calibration differs across laptop/Pi; short PINs remain weak offline |
| Key separation | HKDF-SHA-256 with versioned context | RustCrypto hkdf plus sha2 | Use explicit purpose labels; do not reuse raw roots |
| Encryption/integrity | XChaCha20-Poly1305 AEAD, fresh nonces, AAD | RustCrypto chacha20poly1305 | Audit evidence is version-scoped; require current advisory/version review |
| Secret hygiene | Explicit secret wrappers and zeroization | secrecy plus zeroize | Best effort only; not protected memory or enclave |
| Device layer | LUKS2 as optional defense in depth | cryptsetup/operator configuration | Does not create app-level lock state |
| Database/store | Record format or SQLCipher, selected later | Evaluate in 6.4 | Storage engine is a material owner decision |

Primary/authoritative references are linked throughout the security documents:
[BIP-39](https://github.com/bitcoin/bips/blob/master/bip-0039.mediawiki),
[RFC 9106](https://www.rfc-editor.org/rfc/rfc9106.html),
[RFC 5869](https://www.rfc-editor.org/rfc/rfc5869.html),
[Libsodium AEAD documentation](https://doc.libsodium.org/secret-key_cryptography/aead),
[Linux cryptsetup documentation](https://man7.org/linux/man-pages/man8/cryptsetup.8.html),
and [Trove's threat-model method](https://trove.distrust.co/threat-model.html).
The proposed RustCrypto AEAD direction also has a
[public NCC Group review](https://pentestreports.com/files/reports/nccgroup/NCC_Group_MobileCoin_RustCrypto_AESGCM_ChaCha20Poly1305_Implementation_Review_2020-02-12_v1.0.pdf)
whose scope is an earlier commit, not an unlimited certification.

## Owner approvals required before Milestone 6.1+

1. Minimum PIN policy, allowed characters, retry/cooldown behavior, and the
   honest wording used for weak numeric PINs.
2. Empty versus optional BIP-39 passphrase policy. The recommendation is empty
   in v1 to avoid silently creating an unrecoverable second secret.
3. Envelope encoding, migration, atomic-write, and corruption/fallback policy.
4. Encrypted storage engine: small record store versus SQLCipher or another
   separately reviewed option.
5. Exact lock/sleep/wake and inactivity behavior.
6. Backup destination, retention, rollback handling, and nondestructive restore
   UX.
7. Whether/when to support TPM2, FIDO2, external security hardware, or
   platform-specific authenticators.
8. Whether a control-only spoken “next” recovery interaction is acceptable
   after its isolated privacy design is tested, with a non-voice fallback.
9. The release-review bar for key-management code and dependencies.

## Acceptance-criteria assessment

| Criterion | Evidence | Status |
| --- | --- | --- |
| Comprehensive threat model | [THREAT_MODEL.md](../security/THREAT_MODEL.md) | Satisfied as documentation |
| Assets and trust boundaries | Asset/boundary tables and diagram | Satisfied as documentation |
| Major attack scenarios | Scenario matrix including theft, phrase, OS, logs, supply chain, backups | Satisfied as documentation |
| Master-key and recovery architecture | Key hierarchy, envelope contract, lifecycle | Satisfied as recommendation |
| Technical rationale | Standards/alternatives and limitations | Satisfied as recommendation |
| Relevant libraries evaluated | Candidate library/platform table | Satisfied; no dependency selected or added |
| Known limitations explicit | Goals/non-goals, secure-memory, OS, PIN, and backup sections | Satisfied |
| Linux/Pi differences documented | Security architecture platform section | Satisfied as design |
| Backup/restoration architecture | Backup and nondestructive restore contract | Satisfied as design |
| Reusable review checklist | [REVIEW_CHECKLIST.md](../security/REVIEW_CHECKLIST.md) | Satisfied |
| Owner can approve trade-offs | Explicit approval list | Awaiting owner decision |
| No application code/behavior changed | Documentation-only file set | To be verified in completion checks |

## Completion report

The architecture/research deliverables are complete. The milestone remains
**awaiting owner approval** because the decisions above materially change the
threat model and user experience. No Milestone 6.1 work is authorized by this
document.

Documentation/repository verification run for this milestone:

- `cargo fmt --check` — passed.
- `cargo check` — passed.
- `cargo test` — passed: 139 tests passed, 3 hardware/model-dependent tests
  remained intentionally ignored, and none failed.
- `cargo clippy --all-targets -- -D warnings` — passed.
- `git diff --check` — passed after the final documentation changes.
- Required-documentation paths and trailing whitespace in the changed/new
  documents were checked locally.

No live microphone, camera, Ollama, Piper, recovery-phrase, encrypted-storage,
or hardware-authenticator test was run or implied. There is no such security
implementation in this milestone to test.
