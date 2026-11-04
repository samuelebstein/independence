# Security threat model

> **Status:** Milestone 5.5 design artifact plus Milestone 6.1 foundation.
> The current code implements an Ephemeral Mode/lifecycle boundary, but not
> encryption, authentication, locked storage, recovery, or secure memory.

## Purpose and method

This threat model makes two things explicit before private persistence is
added: what Independence must protect, and where a protection stops. It adapts
the useful part of [Trove's threat-model methodology](https://trove.distrust.co/threat-model.html):
write attacker capabilities down, make assumptions conspicuous, and add
defenses in understandable layers rather than assuming a friendly host.

The analysis is deliberately more conservative than the current terminal
prototype. It assumes that future Independence may hold a person's lasting
conversations, personal memories, photographs, settings, encrypted backups,
and recovery material. It does **not** import Trove's organizational custody,
quorum, or air-gapped-OS requirements; those are not current Independence
requirements.

## Current baseline: what exists today

The current code has useful privacy boundaries, but no cryptographic private
storage boundary:

- `SecurityManager` (`src/security.rs`) owns a process-local lifecycle policy
  and an opaque Ephemeral Mode lease. Production startup is `Uninitialized`;
  there is no production route to `Unlocked` or Private Mode yet.
- The default Ephemeral `Controller` keeps accepted conversation turns and
  freshly captured `Photo` bytes in process memory only (`src/controller.rs`).
  It starts empty; `/reset` and orderly runtime shutdown clear application-owned
  context; process exit loses it.
- Ephemeral Mode rejects `/look` before file I/O, so it does not load an
  arbitrary saved photo or personal file. Explicit fresh camera capture remains
  available under the existing consent/control path and stays in memory.
- Captured microphone samples, Whisper work, camera JPEGs, and Piper WAVs are
  bounded in memory for an interaction. The application does not intentionally
  save camera stills or recordings (`src/dictation/`, `src/see/`, `src/speak/`).
- Ollama requests go only to an allowed loopback HTTP endpoint, with proxies and
  redirects disabled (`src/think/mod.rs`). A loopback SSH tunnel can nevertheless
  forward that port to a separate owner-controlled machine.
- Typed and spoken input are not passed through a shell. A spoken slash command
  remains question text.
- Optional JSONL telemetry is redacted and OTLP is only enabled by explicit
  configuration (`src/telemetry.rs`). Conversation text now enters neither the
  diagnostic nor telemetry paths. The terminal interface intentionally shows
  recognized final text and answers on stdout, so terminal scrollback and
  redirection remain exposure boundaries.

There is currently no PIN, recovery phrase, master key, encrypted database,
persistent lock/unlock state, secure enclave integration, encrypted backup, or
private storage. Treat every current local user/process that can inspect the
app's memory, terminal, files, child processes, or device nodes as potentially
able to observe interaction material.

### What Milestone 6.1 changes—and does not change

The lifecycle boundary prevents a controller/UI/CLI selection from asserting
that private data is available. It makes an unauthenticated temporary session
the only production session and prevents local-file photo loading in that
session. It does **not** protect plaintext while it is in application memory,
force another local process to forget input, erase terminal scrollback, or
securely erase OS/allocator/swap/crash-dump copies. The local Ollama server is
also outside the application's memory-clearing guarantee.

## Assets to protect

| Asset | Why it matters | Normal exposure window | Desired protection |
| --- | --- | --- | --- |
| Conversation history and personal memories | Can reveal beliefs, relationships, plans, and context | Unlocked session; future persistent store | Confidentiality and integrity at rest; no controller access while locked |
| Photographs and visual context | Can reveal people, home, documents, location, or screen contents | Capture/load through model request | Explicit capture, minimal retention, encrypted persistence, integrity |
| Audio recordings and transcripts | Can reveal voice, environment, identity, and sensitive speech | Recording/Whisper/model path | Explicit capture, bounded in-memory lifetime, encrypted persistence if retained |
| Personal settings/configuration | Can reveal selected devices, models, endpoints, behavior, and privacy choices | Startup and runtime | Integrity; confidentiality when personally revealing |
| Master data-encryption key (DEK) | Decrypts private application data | Setup/unlock/recovery/unlocked session | Confidentiality, controlled lifetime, no logging/serialization |
| PIN and PIN-derived key material | Gate to one DEK envelope | PIN entry/unwrapping | Never log/persist plaintext; slow offline guessing; rate limits |
| Recovery phrase and derived recovery material | Alternate path to DEK envelope | Setup/recovery ceremony | Never transcribe, log, model, or persist phrase; user-controlled physical copy |
| Encrypted key envelopes | Necessary companion to PIN/recovery path | Persistent device/backup metadata | Integrity, versioning, availability, tamper detection |
| Encrypted backups | Can contain a complete personal archive | Export/transport/restore | Confidentiality, integrity, portable recovery, no plaintext key/phrase |
| Device identity/security state | Determines which storage and authenticator policy applies | Startup/lifecycle | Integrity and explicit capability reporting |

## Security goals and non-goals

### Goals for the approved future design

1. A powered-off or locked device/storage copy should not yield readable private
   Independence data without a valid authorization path.
2. A PIN and a recovery phrase should independently unwrap the same random DEK
   through separate encrypted envelopes; changing a PIN should not re-encrypt
   all data.
3. A recovery phrase plus an available encrypted envelope should recover access
   without deleting existing data. A phrase alone cannot recreate lost data.
4. Application code should not receive private history until a security manager
   has granted an unlocked storage capability.
5. Capture and recovery interactions should avoid leaking private material to
   logs, telemetry, prompts, conversation history, remote endpoints, or
   unrelated child processes.
6. Tampering, corruption, wrong credentials, and stale formats should fail
   closed and visibly, rather than yielding plausible garbage.

### Deliberate non-goals and residual boundaries

- Application encryption cannot protect private material from a fully
  compromised operating system while Independence is unlocked.
- It cannot erase text already shown in terminal scrollback, heard in a room,
  copied from a screen, or sent to a configured external system.
- It cannot make a short numeric PIN high entropy; a copied encrypted envelope
  still permits offline guessing attempts.
- It cannot recover data if the encrypted device data and every backup are
  gone, even if the recovery phrase remains.
- It cannot make a generic Linux PC or Raspberry Pi have a TPM, secure enclave,
  locked RAM, trusted boot, or resistance to physical observation.
- It cannot prove a model, dependency, camera driver, or operating system has
  no vulnerability. Supply-chain and runtime hardening remain ongoing work.

## Trust boundaries

```text
person + physical room
  | PIN / recovery paper / screen / microphone / camera
  v
Independence security manager  <----> encrypted application storage + envelope metadata
  | grants narrow unlocked storage capability
  v
controller / voice runtime / camera runtime
  |                      |                         |
  v                      v                         v
Whisper/whisper.cpp   Ollama process/API      Piper/rpicam-still/CPAL/Rodio
  |                      |                         |
  +---------------------- operating system / kernel / drivers ----------------+
                                  |
               disks, swap, logs, removable media, backup destination, network
```

The future security manager is the only intended boundary that owns a DEK.
The current manager already owns lifecycle/mode policy but has no DEK. The
controller owns accepted temporary conversation state; a future private
controller path must receive persistent history only after the manager opens
private storage. Existing controller/workers are not key-management authority.

Important external boundaries include:

- **Operating system and user account:** controls process memory, device nodes,
  files, swap, signals, debugging, and child process execution.
- **Local model and media processes:** Ollama, Whisper, Piper, `rpicam-still`,
  CPAL/ALSA/PipeWire, and Rodio may each have their own memory, caches, logs,
  permissions, and vulnerabilities.
- **Terminal and telemetry destinations:** stdout/scrollback/redirection are
  not confidential by default. An optional OTLP endpoint is an explicit export
  boundary, even if the app's attributes are redacted.
- **Physical environment:** a person can observe a screen, photograph paper,
  hear a speaker, plug in a keyboard, remove storage, or use a microphone's
  acoustic environment.
- **Backups and removable media:** an attacker can copy them indefinitely and
  attempt offline attacks without application rate limits.

## Attacker classes and assumptions

| Attacker | Capabilities assumed | Not assumed automatically |
| --- | --- | --- |
| Opportunistic finder/thief | Steals powered-off device or removable storage; can inspect copied files | Knows PIN or recovery phrase |
| Offline storage attacker | Copies disk/SD card/backup and tries unlimited guesses offline | Can read RAM from a currently unlocked device |
| Nearby observer | Sees screen/paper/keyboard, hears speech/speaker, may speak near microphone | Root access or copied storage |
| Local unprivileged program | Reads files allowed to the user, influences input or environment, consumes resources | Kernel-level memory access |
| Malicious software or compromised account | Reads process memory, files, child-process inputs, terminal output, and device data while allowed | Hardware-attested boot or isolation |
| Compromised OS/kernel/firmware | Controls scheduling, memory, drivers, and I/O | Any app-only secret protection while unlocked |
| Supply-chain attacker | Malicious dependency, update, model artifact, package, or build machine | Review/build controls that have not yet been added |
| Backup recipient/thief | Holds encrypted export and metadata for indefinite offline attack | The recovery phrase unless separately obtained |

The project should not claim protection against a stronger attacker merely
because encryption works against a weaker one.

## Attack scenarios

`Existing` describes the code today. `Required` describes future design work;
it is not implemented by this document.

| Scenario | Attacker capability and path | Assets targeted | Existing protections | Required protections | Residual risk |
| --- | --- | --- | --- | --- | --- |
| Physical theft, powered off | Take laptop/Pi/SD card and inspect files | Future private store, envelopes, backups | No persistent app session today; no encryption | App-level authenticated encryption; optionally LUKS2/secure storage; PIN/recovery envelopes | Weak PIN may still be guessed offline; device may contain plaintext logs/models elsewhere |
| Storage removal/offline inspection | Copy storage without device | Same plus backup metadata | None for future storage | Unique KDF salts, costly Argon2id, AEAD envelopes, backup encryption | Unlimited offline attempts; phrase+envelope is sufficient by design |
| PIN guessing/bypass | Try PIN at UI or on copied envelope | PIN-wrapped DEK | No PIN yet | Online rate policy; Argon2id parameters per device class; optional hardware support | App rate limits do not constrain copied storage; short numeric PIN remains weak |
| Recovery phrase exposure | Photograph/write down/read phrase | Recovery-derived key | No phrase yet | Display-only ceremony, no transcript/log/model path, explicit warning, user-controlled paper handling | Phrase is a high-value bearer secret; social/physical theft remains |
| Phrase plus encrypted device/backup | Obtain both recovery phrase and envelope/ciphertext | DEK and all protected data | Not applicable yet | This combination intentionally recovers the DEK; warn clearly | Complete compromise of protected data; cannot be prevented without a second factor or different recovery model |
| Device and backups lost | Destroy/remove all ciphertext and envelopes | Availability | No backups today | Explain limits during setup; export verified encrypted backup | Recovery phrase alone cannot reconstruct absent data |
| Malicious local program | Spy on terminal, filesystem, process, environment, audio device, or replace executable | Any data while unlocked; phrases/PIN at entry | Shell avoidance and bounded worker APIs help only narrowly | Least privilege, hardened release process, reduce plaintext logs, process isolation where feasible | No app-level defense is sufficient against a hostile OS/user session |
| Compromised OS/kernel/firmware | Read memory and intercept I/O | DEK, PIN, phrase, live media | None | Document as out of app scope; optional verified boot/TPM/layered OS hardening per device | Still cannot trust an unlocked compromised system |
| Dependency/model supply chain | Backdoored crate, package, model, Piper/Ollama binary, build artifact | All assets and correctness | Cargo.lock pins resolution; no reproducible-build policy yet | Dependency review, advisory scanning, provenance/SBOM plan, pinned hashes/releases, reproducible-build path | Review cannot prove absence of compromise |
| Physical observation/listening | See words on screen/paper; hear recovery words or speaker echo; plug input device | Recovery phrase, PIN, transcript, photo context | Explicit capture and current no persistent audio | Private ceremony UX, mute output, no general ASR, no phrase display logging, safe physical controls | Shoulder surfing, hidden cameras, acoustic leakage remain |
| Logs/debug/telemetry/model exposure | Read scrollback, redirected stdout, stderr diagnostics, JSONL, OTLP, crash report, model request | Transcript, answer, media metadata, PIN/recovery phrase | Terminal interface plaintext; diagnostics/telemetry redact conversation content | Secret-safe types, no debug/display, logging policy, encrypted or opt-in logs, no secrets in model path | Terminal interface scrollback still exposes dialogue; third-party process logs remain |
| Unauthorized backup access | Copy/cloud-sync/export backup | Ciphertext, recovery envelope, metadata | No backup feature yet | AEAD backup format, no plaintext DEK/phrase, versioned manifest, restore authentication | Phrase theft plus backup decrypts by design; metadata leakage must be minimized |
| Tamper/rollback/corruption | Modify/delete old ciphertext/envelopes/backup | Integrity and availability | Model/camera validation only | AEAD, authenticated metadata, atomic writes, backup version/rollback policy | Availability cannot be guaranteed; rollback detection needs a trusted monotonic anchor or user-visible policy |
| Peripheral/input attack | Malicious keyboard/camera/audio USB or evdev injection | Controls, privacy, availability | Explicit device selection and exclusive evdev grab | Permission scoping, input validation, indicator/lock policy, physical-device guidance | A malicious USB device can emulate a trusted device; no hardware attestation today |

## Recovery-phrase warning

This warning must appear before the future recovery ceremony and backup export:

> Anyone who obtains your recovery phrase and a copy of your encrypted device
> data or backup may be able to access your information.

The inverse matters too: the phrase alone is not a copy of the encrypted data
and cannot reconstruct data that no longer exists. The mnemonic is an
authorization factor for an available encrypted key envelope, not a magical
backup.

## Security review evidence required before implementation

Before any future milestone says it protects an asset, it must show:

1. a state and ownership diagram that names where plaintext and key material
   exist;
2. test vectors and negative tests for every serialized cryptographic format;
3. no secret-containing `Debug`, `Display`, tracing, telemetry, crash-report,
   or model-prompt path;
4. cancellation, lock, shutdown, and error tests that do not resurrect stale
   plaintext or keys;
5. a migration/rollback story for versioned on-disk data; and
6. a manual test on each supported platform, clearly distinguished from fake
   tests.

The reusable operational list is in
[REVIEW_CHECKLIST.md](REVIEW_CHECKLIST.md).

## References

- [Trove threat model](https://trove.distrust.co/threat-model.html) — an
  example of explicit assumptions and staged threat-model levels; Independence
  adopts the method, not its organizational requirements.
- [Linux `cryptsetup` manual](https://man7.org/linux/man-pages/man8/cryptsetup.8.html)
  — LUKS keyslots and encrypted block-device concepts.
- [Linux `mlock(2)` manual](https://www.man7.org/linux/man-pages/man2/mlock2.2.html)
  — memory-lock limits and the fact that suspend can still save RAM to disk.
- [RFC 9106](https://www.rfc-editor.org/rfc/rfc9106.html) — Argon2id purpose
  and parameter-selection procedure.
- [BIP-39 specification](https://github.com/bitcoin/bips/blob/master/bip-0039.mediawiki)
  — 24-word mnemonic construction and standard seed derivation.
- [RFC 5869](https://www.rfc-editor.org/rfc/rfc5869.html) — HKDF
  extract-and-expand key derivation.
- [Libsodium AEAD guidance](https://doc.libsodium.org/secret-key_cryptography/aead)
  — authenticated encryption, associated data, and XChaCha20-Poly1305 nonce
  characteristics.
