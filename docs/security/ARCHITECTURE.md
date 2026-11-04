# Security architecture

> **Status:** Milestone 5.5 design plus Milestone 6.1 foundation. The
> cryptographic, authentication, recovery, storage, and backup sections remain
> approval-required design; the lifecycle/ephemeral boundary is current code.

> **Implementation status:** Milestone 6.1 now implements the process-local
> `SecurityManager` lifecycle policy and default Ephemeral Mode described
> below. It does **not** implement keys, PINs, recovery phrases, encryption,
> private storage, or persistent authentication. Sections about those remain
> approved design direction or recommendations, not current protection.

## Read this first: three kinds of statement

| Label | Meaning in this document |
| --- | --- |
| **Agreed direction** | A requirement already supplied by the project owner |
| **Recommendation** | The proposed technical design and its rationale |
| **Approval required** | A material product/security choice that must not be made silently |

The proposed architecture uses a small number of well-known primitives in a
specific arrangement. It does not invent a cipher, hash, mnemonic algorithm, or
password KDF. The serialized formats and code still require a separate design
review, test vectors, and implementation milestones.

## Terms in plain language

- **DEK (data-encryption key):** a random secret that unlocks private
  Independence data. It is not a password and is never derived from a PIN.
- **KEK (key-encryption key):** a key used only to encrypt (“wrap”) the DEK.
  Independence will have one KEK derived from the PIN and another derived from
  the recovery phrase.
- **Envelope:** a small, versioned record containing a DEK encrypted under one
  KEK plus the public parameters needed to try to unwrap it.
- **AEAD:** authenticated encryption. It hides plaintext and detects changed
  ciphertext/metadata before returning plaintext.
- **Salt:** public random data that makes a KDF/envelope instance unique. It is
  not a password or a substitute for a key.
- **Associated data (AAD):** public metadata that AEAD authenticates but does
  not encrypt, such as format version and the dataset identity.

## 1. Agreed direction and the challenge to it

### Agreed direction

1. Generate a cryptographically random master DEK.
2. Use it to protect private application data.
3. Keep two independent encrypted copies of that *same* DEK: one unlocked from
   a PIN-derived KEK and one unlocked from recovery-derived key material.
4. Changing a PIN re-wraps the DEK; it does not re-encrypt all personal data.
5. A 24-word BIP-39 mnemonic can recover the DEK when an encrypted envelope is
   available, then establish a new PIN.

### What needs strengthening

The direction is sound in shape, but it is incomplete without answers to the
following:

- A PIN is often low entropy. Argon2id makes each guess expensive; it cannot
  turn a four- or six-digit PIN into a strong offline secret.
- A recovery phrase is a bearer secret. The phrase plus any valid recovery
  envelope intentionally defeats the PIN. This is a feature for recovery and a
  high-impact theft scenario.
- The application must bind an envelope to the right dataset and purpose. An
  unauthenticated envelope header can otherwise be swapped, downgraded, or
  misinterpreted.
- “Encrypt the data” must name a storage format, nonce strategy, integrity
  policy, migration policy, and backup policy; otherwise encryption is only a
  slogan.
- Keys live in an operating system process. Zeroing helps hygiene, but it does
  not make a hostile or unlocked OS safe.
- An app-level lock must control the controller's access to private session
  state—not merely hide a screen while a key remains usable elsewhere.

The rest of this document supplies a conservative proposal for these gaps.

## 2. Recommended key hierarchy

    PIN (entered, never stored)                 24-word BIP-39 mnemonic (never stored)
            |                                             |
     Argon2id + unique PIN-envelope salt       standard BIP-39 seed derivation
            |                                             |
        PIN-KEK (32 bytes)                     HKDF-SHA-256 + recovery-envelope salt/context
            |                                             |
     XChaCha20-Poly1305 PIN envelope           XChaCha20-Poly1305 recovery envelope
            \                                             /
             \---- same random 32-byte master DEK --------/
                                  |
                     HKDF domain-separated subkeys
                        /              |                \
              private store key   backup key       future purpose keys
                                  |
                        XChaCha20-Poly1305 encrypted records

### Master DEK

**Recommendation:** Generate exactly 32 random bytes using the operating
system's cryptographically secure randomness source through a maintained Rust
interface such as getrandom. Give the DEK a random nonsecret data-set ID and a
nonsecret key ID. The IDs identify which ciphertext/envelope belongs together;
they are not derived from, or a hash of, the raw DEK.

The DEK should not be treated as an ordinary Rust string, cloned into controller
state, serialized, logged, or sent to any model or child process. Only a future
SecurityManager should hold it during an unlocked period.

### PIN envelope

**Recommendation:** Derive a 32-byte PIN-KEK with Argon2id from the exact
allowed PIN byte representation and a fresh random salt unique to that PIN
envelope. Store the Argon2id version, memory cost, time cost, parallelism,
output length, and salt in authenticated envelope metadata. Use that KEK once,
only to encrypt the DEK in the PIN envelope.

The [Argon2id specification](https://www.rfc-editor.org/rfc/rfc9106.html)
recommends choosing parameters from memory and acceptable runtime, with a
128-bit salt sufficient for password/KDF use. Its two example profiles (2 GiB
for a broad default, or 64 MiB / three passes when memory constrained) are
starting points for measurement—not a universal Independence setting. A Pi with
4 GiB RAM, a laptop, and a future low-memory device require separate measured
calibration. The selected values must be recorded in the envelope so future
software can reproduce them.

**Important limitation:** the KDF slows an attacker but cannot rate-limit a
copied envelope. A short numeric PIN remains guessable; application cooldowns
only affect the local UI. This is why full-disk encryption and hardware-backed
options are defense in depth, not why a weak PIN becomes safe.

### Recovery envelope

**Agreed direction:** use a 24-word BIP-39 mnemonic. A 24-word mnemonic carries
256 bits of generated entropy plus an 8-bit checksum under the
[BIP-39 specification](https://github.com/bitcoin/bips/blob/master/bip-0039.mediawiki).
It is a human transcription of generated randomness, not a user-invented
sentence and not an encryption key used raw.

**Recommendation:** use BIP-39's standard mnemonic-to-seed operation with an
empty BIP-39 passphrase for the first product version. BIP-39 defines that
operation as PBKDF2-HMAC-SHA512 with 2,048 iterations and a salt formed from
the string `mnemonic` plus the passphrase; it yields a 64-byte seed. A future
implementation should call a maintained BIP-39 library's standard seed API,
not reproduce this derivation or replace it with an ad-hoc mnemonic hash. Then
use HKDF-SHA-256 with a unique recovery-envelope salt and fixed, versioned
context such as “Independence/recovery-wrap/v1 || data-set ID || envelope ID”
to obtain a 32-byte recovery KEK. HKDF's extract-and-expand construction is
specified in [RFC 5869](https://www.rfc-editor.org/rfc/rfc5869.html); the
explicit context prevents this seed from being silently reused for unrelated
purposes.

An optional BIP-39 passphrase is **not** recommended in the first version. It
creates a second recovery secret with a severe usability and loss risk. If the
owner later wants it, it needs an explicit recovery UX and backup policy; it
must never be added silently.

### Why two envelopes are independent

They carry the same DEK but use different derivation inputs, salts, contexts,
nonces, and envelope identifiers. Recovering the PIN envelope does not require
the mnemonic; recovering the recovery envelope does not require the PIN. They
are not two copies of the data and not two separate master keys.

Changing a PIN creates a new PIN envelope only after the existing DEK was
successfully unwrapped through a valid PIN or recovery flow. The recovery
envelope remains valid. A recovery flow unwraps the DEK, asks for a new PIN,
creates a new PIN envelope atomically, and never deletes existing ciphertext
as part of a failed recovery attempt.

## 3. Proposed envelope and record format

### Key-envelope contract

The concrete byte encoding needs its own 6.2 specification and test vectors.
This is the required semantic contract:

| Field | Visibility | Purpose |
| --- | --- | --- |
| format version, cipher suite, envelope kind | Public, authenticated | Reject unknown/downgraded format; distinguish PIN and recovery |
| data-set ID, key ID, envelope ID | Public, authenticated | Bind to one data collection and one specific envelope |
| KDF identifier, parameters, salt | Public, authenticated | Reproduce the correct PIN/recovery KEK without secret storage |
| AEAD nonce | Public, authenticated | Fresh 192-bit XChaCha20 nonce for this encryption |
| creation/replacement metadata | Public, authenticated | Audit/migration context; not a security decision by itself |
| ciphertext and authentication tag | Secret/authenticated | Exactly the 32-byte DEK plus fixed internal version bytes |

Before invoking a KDF, parsing must reject oversized/unknown parameters,
unknown suite/version, malformed lengths, and a mismatched data-set/key
identity. The entire protected header is canonicalized and supplied as AEAD
associated data. Thus a copied PIN envelope cannot be relabeled as a recovery
envelope or silently moved to another dataset.

**Recommendation:** use XChaCha20-Poly1305 with a new CSPRNG-generated
192-bit nonce for every envelope and every encrypted record. It provides
confidentiality and tamper detection in one operation. Its extended nonce makes
random nonces practical, but it does not excuse reuse. The
[Libsodium AEAD documentation](https://doc.libsodium.org/secret-key_cryptography/aead)
describes authenticated associated data and XChaCha's 192-bit nonce behavior.

The public encoding itself remains an **approval-required** choice: a carefully
specified canonical binary form is the simplest option; canonical CBOR is a
reasonable alternative. JSON is not suitable as an implicit cryptographic
canonicalization rule. Whichever is selected must be versioned, bounds-checked,
written atomically, and tested against fixed vectors.

### Private-data records

Do not repeatedly hand the raw master DEK to unrelated storage code. Derive
purpose-separated 32-byte subkeys with HKDF. For example:

    storage root = HKDF-SHA-256(DEK, salt = data-set ID,
                                info = “Independence/private-store/v1”)
    backup root  = HKDF-SHA-256(DEK, salt = data-set ID,
                                info = “Independence/backup/v1”)

Each private record uses a fresh nonce and AAD that binds the record type,
record identifier, format version, data-set/key ID, and revision. This provides
confidentiality and detects substitution/tampering. Large media must be bounded
and chunked through an established streaming/record construction rather than
loaded as one arbitrary ciphertext. The exact storage engine is intentionally
deferred to Milestone 6.4.

### Key lifecycle

| Event | Required result |
| --- | --- |
| First setup | Create data-set ID, random DEK, recovery envelope, PIN envelope, and initial encrypted-store metadata only after recovery ceremony verifies |
| Normal unlock | Read public metadata, derive one KEK, authenticate one envelope, retain DEK only in security manager, then grant storage capability |
| Wrong PIN/phrase/tampering | Generic authentication failure; do not reveal whether a phrase, PIN, or field was correct; do not alter data |
| PIN change | Verify current authority, create and atomically replace PIN envelope; do not re-encrypt private data |
| Recovery | Authenticate recovery envelope, create new PIN envelope, preserve old ciphertext; no destructive reset by default |
| Lock/shutdown | Revoke storage capability, cancel private work, clear caches, zeroize secret containers best-effort |
| DEK rotation | Deferred. Requires explicit data migration/backup compatibility and is not implied by a PIN change |

Atomic replacement means writing a fully authenticated new envelope, flushing it
and its parent directory as appropriate for the chosen filesystem, then
switching a small pointer/rename. A failed write must leave at least one valid
authorization path intact. This needs platform-specific fault-injection tests
before any availability guarantee is claimed.


## 4. Security states and ownership boundary

### Milestone 6.1 implemented foundation

`src/security.rs` now owns a small, dependency-free `SecurityManager`. It is
not a cryptographic manager yet: it has no key material, persistence, disk I/O,
PIN input, recovery input, or model/device authority.

| Item | Current implementation | Not implemented yet |
| --- | --- | --- |
| Lifecycle states | `Uninitialized`, `SetupInProgress`, `Locked`, `Unlocked`, `RecoveryRequired`, with tested transition checks | Persisting state or obtaining state from an enrolled device |
| Default launch | Starts a process-local `Uninitialized` manager, then obtains an opaque Ephemeral Mode lease | Any automatic setup or unlock |
| Ephemeral Mode | Allowed from `Uninitialized`, `Locked`, and `RecoveryRequired`; does not change lifecycle state | A durable profile, preferences, or personal-memory load |
| Private Mode | Named explicitly and denied unless lifecycle is `Unlocked` | Any production way to become `Unlocked` |
| Storage authority | An opaque future `PrivateStorageAccess` type exists behind the manager | A storage adapter, DEK, envelope, or actual data access |
| Cleanup | Runtime cancels/joins work, then clears controller-owned temporary history/photos on orderly exit | Secure erasure outside normal Rust/application ownership |

The deliberate current rule is: a CLI flag cannot authenticate someone.
`--mode private` fails before model, microphone, camera, terminal-input, or
Piper startup. Test-only lifecycle transitions use private marker types that
are not compiled into production authorization paths; they test policy, not a
fake PIN.

Current production runs start `Uninitialized` every process because there is
no protected metadata to persist. The manager still tests the Locked and
RecoveryRequired rules so later storage/authentication work has a stable
policy boundary. Calling the device “locked” today would be misleading: no
private data store exists to lock.

#### Ephemeral session policy

The controller is constructed as an Ephemeral controller for typed and voice
paths. It begins with empty history/photos; `/look` is rejected before file
I/O; explicit fresh `/camera` capture remains available under existing user
action/model-capability checks. Captured stills, microphone buffers, and
conversation context remain in memory only. On orderly shutdown the runtime
clears controller history/photos after its owned workers are cancelled or
joined. `/reset` clears the current temporary context earlier.

Ollama's current non-streaming API does not maintain an application session
identifier: Independence sends accepted history explicitly on each request.
After controller cleanup, the next request has no prior application history.
Model residency (`keep_alive`) or opaque model-server internals are not proof
of cryptographic or secure-memory clearing.

#### Physical entry policy

In Raspberry Pi `--voice --input buttons` mode, the selected exclusive evdev
Talk/Enter button must be held for five seconds then released before the
Ephemeral lease is obtained. The gate uses no microphone/camera/model/Piper
work and consumes that first release; a fresh press begins normal recording.
Terminal mode has no physical device to gate and enters default Ephemeral Mode
directly. This is a usability/intent gesture, not authentication.

### Proposed state machine

    Uninitialized
         |
         | explicit setup begins
         v
    Setup in progress ---- cancellation/failure ----> Uninitialized or Recovery required
         | verified recovery ceremony + envelope creation
         v
    Locked <---- lock, sleep policy, shutdown, timeout ---- Unlocked
         | PIN or recovery attempt                            |
         v                                                    |
    Unlocking -------------------------------------------------+
         | valid PIN envelope or recovery envelope
         v
    Unlocked
         |
         | envelope/data missing, corrupt, unsupported, or no usable authority
         v
    Recovery required

**Uninitialized** contains no private Independence store. Setup may create a
new data set, but it must not overwrite an existing one merely because setup was
started accidentally.

**Setup in progress** is a sensitive ceremony: generate the DEK and mnemonic,
show/review recovery words, verify the recorded words, and create both
envelopes. Cancellation clears ephemeral material and does not leave a
half-initialized store marked as usable.

**Locked** may read only public, minimum metadata necessary to render an unlock
screen and select compatible code. It cannot load conversation history,
photographs, transcripts, personal settings, or a DEK. It must not send any
private material to Ollama, Whisper, Piper, camera processes, logs, or
telemetry.

**Unlocking** has a narrow, cancellable secret-input window. It derives one
candidate KEK and authenticates one envelope. The UI should report a generic
failure after equalized handling where practical, rather than distinguish a bad
PIN, bad phrase, corrupt envelope, or swapped metadata.

**Unlocked** is the only state in which a future storage capability can expose
private state to the controller. It is not synonymous with “secure against a
compromised OS.” It is the period in which live plaintext and DEK material
exist, so device access and child-process policies matter most.

**Recovery required** means encrypted data may still be intact but an envelope
is missing, corrupt, unsupported, or there is no configured ordinary
authenticator. It must offer nondestructive recovery/export choices and never
silently erase data.

### SecurityManager, controller, and storage

| Component | Owns | Must not own |
| --- | --- | --- |
| SecurityManager (current foundation; future expanded) | Current lifecycle/mode policy and opaque session leases; later DEK lifetime, envelopes, authenticator policy, lock/recovery transitions | Conversation decisions, model calls, sensor activation, ordinary terminal parsing |
| Private storage adapter (future) | Encrypted record serialization, authenticated read/write, bounded plaintext buffers | PIN/recovery UX, raw DEK copies outside granted capability |
| Existing Controller | Accepted scene/history and current turn authorization after storage is open | PIN, recovery phrase, raw master key, envelope mutation |
| Existing media/model workers | One explicit capture/transcription/inference/playback operation | Long-lived private history or any authorization secret |
| UI/input adapter | User gesture and safe display/input forwarding | Secret logging, model routing, raw key material |

The key design point is capability flow, not class names: the controller asks for
an unlocked private session or storage transaction; it never receives a general
copy of the DEK. A lock transition revokes that capability before or while it
cancels controller/model/media work. Results returning after revocation must be
treated like today's stale TurnId results: harmless and unable to restore
private state.

### Common authenticator boundary

PIN, recovery, a future fingerprint, and a future hardware security key should
share a small policy boundary rather than make each feature reach directly into
storage. Conceptually, an authenticator reports its kind, whether it is actually
available/enrolled on this device, and an authorization outcome. The
SecurityManager decides which current envelope or protected key material that
outcome may unlock.

This keeps an important distinction honest: a biometric is an authorization
signal delivered by a platform authenticator; it is not a reproducible,
application-derived encryption key. Raw fingerprint/voice templates must never
become Independence key material. Hardware-backed capability detection belongs
in Milestone 6.6, after the common boundary and its Linux/Pi fallback behavior
exist.

### Locking, sleep, wake, and shutdown

**Recommendation:**

- Start locked once encrypted storage exists; authenticate before loading private
  history.
- On explicit lock, configured inactivity timeout, sleep, user switch, and
  orderly shutdown: stop/release microphone and camera, cancel app-owned model
  and speech work, revoke storage access, clear plaintext caches, and
  zeroize key containers best-effort.
- On wake: return to Locked, not silently to Unlocked, unless a future
  platform-specific authentication policy has been explicitly approved.
- On Raspberry Pi, a physical control must request a normal application lock;
  it must not pretend to instantly erase RAM or turn a consumer board into a
  secure enclave. Existing evdev keyboard controls are input adapters, not
  security tokens.
- On Linux, use normal screen/session locking as an additional user-interface
  boundary, but do not assume it clears Independence memory or protects data
  from the same compromised user session.

Cancellation can stop app-owned HTTP tasks and child processes, but cannot prove
a separate Ollama server, audio driver, operating system cache, or remote
tunnel stopped computing immediately. This limitation should remain visible in
the future lifecycle UX.

## 5. Recovery ceremony and voice-risk design

The recovery phrase needs a different interaction boundary from ordinary voice
dictation. The current DictationSession intentionally displays recognized text
through the terminal interface. That path is not allowed to process a recovery
phrase.

### Agreed ceremony requirements

- Show one BIP-39 word at a time, in order.
- Let the person advance with “next”.
- Let them write all 24 words down in order.
- Verify randomly chosen positions.
- Allow pause, cancellation, and restart.
- Never send phrase words to Whisper, Ollama, telemetry, conversation history,
  persistent audio, or ordinary logs.

### Proposed safe interaction

1. Before displaying words, show the exact recovery warning in the threat
   model and explain that the phrase is the second path to the same data.
2. Disable camera capture, normal conversation, Ollama requests, Piper output,
   ordinary transcript display, and terminal interface rendering for the ceremony.
3. Generate the mnemonic locally. Display exactly one numbered word at a time.
   Do not spell it through a speaker, copy it to the clipboard, print it to
   stdout, or write it to a screenshot-friendly log.
4. For “next”, use an isolated control-only input boundary that yields only an
   enum such as Next, Pause, Cancel, or NoMatch. It discards raw audio and
   partial/final words immediately. It must not reuse the current ordinary
   DictationSession display path until that path has been specifically redesigned and
   tested for phrase safety.
5. For verification, ask for selected word positions using a private typed or
   physical-button selector by default. Do **not** ask the person to speak the
   recovery word: doing so turns the microphone into a phrase recorder. A
   voice-only verification design requires a separate threat review.
6. Keep the generated phrase and derived seed in secret containers only for the
   active ceremony. If cancelled or failed, zeroize/drop them and require a
   fresh generated phrase on restart. Never retain a partial ceremony.
7. After verification, create and atomically persist the recovery envelope and
   PIN envelope. Mark setup complete only after both writes authenticate and
   can be reopened in a clean test path.

### Voice, echo, and accidental-capture risks

A microphone can capture more than the intended “next”: a person may read a
word aloud, a speaker might echo system audio, another person may speak nearby,
or an accessibility tool may repeat text. Local recognition is still exposure
to a process and its memory. Therefore the ceremony needs a visible microphone
indicator, a private-room/headset recommendation, no TTS of words, discarded
buffers, no partial-transcript events, and a safe non-voice fallback.

The owner specifically wants spoken “next”; the recommendation preserves that
goal but makes it a narrow control channel, not a general conversation or
transcription channel. It cannot protect against a person or hidden recorder in
the room.

## 6. Storage and encryption layers

### Recommended layered posture

| Layer | What it protects | What it does not protect |
| --- | --- | --- |
| Application-level authenticated encryption | Private Independence records while the app is locked, portable backup content, tamper detection | A compromised unlocked OS/process; plaintext already handed to a model |
| LUKS2 full-disk or volume encryption | Powered-off device/storage and broad OS files, swap, and model/log artifacts on that volume | An unlocked running OS or application-specific lock state |
| OS account/session lock | Casual interactive access to a running screen | Root/malware in the same OS; data already in memory |
| Optional hardware-backed unlock/attestation | Raises theft/offline-attack cost on hardware that actually supports it | A universal guarantee across Linux PCs and Raspberry Pis |

LUKS2 is valuable defense in depth. The Linux cryptsetup manual explains that
LUKS has independent keyslots protecting a volume key and supports changing
individual passphrases. It does not replace the application DEK: when a LUKS
volume is open, applications can read it, and Independence still needs a
meaningful locked state for private data.

### Storage-engine alternatives

| Option | Strengths | Costs / limitations | Recommendation |
| --- | --- | --- | --- |
| Application-level encrypted record store | Directly matches lock state and backup envelope; can minimize metadata and scope plaintext | Requires a carefully specified format, atomicity, migration, indexing, and robust test vectors | Viable for a small first store only with focused 6.4 design review |
| Encrypted SQLite through SQLCipher | Mature database semantics, transactions, queries, migration tooling | Native dependency/build/licensing/integration review; still needs DEK lifecycle and careful key passing | Evaluate in 6.4; not selected by this milestone |
| Plain SQLite plus manually encrypted fields | Familiar query layer | Easy to leak metadata/plain indexes and create inconsistent encryption policy | Not recommended as the first secure design |
| LUKS2 only | Established full-volume protection and low application complexity | No application lock after volume unlock; weak for portable backups and per-app access boundary | Defense in depth, not sufficient alone |
| Filesystem/OS keyring only | Can fit platform-specific secrets | Varies by desktop/Pi; does not solve portable recovery or common semantics | Optional future authenticator integration, not the root design |

For the first encrypted private store, the owner must approve whether to prefer
a small versioned record format or an encrypted database. The choice changes
the failure model and test burden enough that it should not be buried in code.

### Secure memory: useful but limited

Use secret wrappers and zeroization for the DEK, PIN input, mnemonic seed, KEKs,
and temporary plaintext where the chosen APIs permit it. The RustCrypto
[zeroize crate](https://docs.rs/zeroize/latest/zeroize/) protects the wipe from
being optimized away; the [secrecy crate](https://docs.rs/secrecy/latest/secrecy/)
makes secret exposure explicit and avoids common Debug/serialization mistakes.

Neither is a secure-enclave promise. Secrecy explicitly does not provide mlock
or mprotect-style memory protection. Linux mlock can keep pages out of ordinary
swap, but it is limited by permissions/resource limits and the Linux manual
notes that suspend can still write RAM to disk. Future code must say “best
effort key clearing,” never “keys can never remain in memory.”

## 7. Linux and Raspberry Pi posture

### Shared implementation

The envelope format, SecurityManager state machine, KDF/AEAD calls, storage
interface, backup format, redaction rules, and tests should be shared Rust code.
The current split between shared controller logic and platform adapters is a
good precedent: platform code reports capabilities; it does not change the
meaning of a DEK or recovery phrase.

### Linux computers

A Linux PC may have LUKS2, TPM2, UEFI/Secure Boot, FIDO2, desktop keyrings, or
none of these. Capability detection must query what is actually present and
configured; it must not infer a secure enclave or TPM from the operating-system
name. Where a TPM2/verified-boot deployment is intentionally configured,
systemd and cryptsetup can participate in layered disk-unlock policies. That is
operator setup, not a guarantee the app can invent.

### Raspberry Pi

A Raspberry Pi must be treated as an ordinary Linux computer with removable
storage unless its specific board, boot chain, external hardware, and setup are
verified. Do not assume a TPM, secure enclave, protected memory, or secure boot
because it is a Pi. The removable microSD card increases the importance of
application encryption and encrypted backups. An optional external TPM,
hardware security key, or encrypted data volume may later improve a particular
deployment, but the core recovery design must still work without them.

The existing selected keyboard, USB microphone/speaker, camera module, evdev
permissions, and rpicam process are functional hardware boundaries, not
authentication hardware. Device disconnect must safely lock/cancel an active
interaction, but it does not establish identity.

## 8. Encrypted backup and restoration

### Backup contract

A future backup is an explicit export, not an automatic cloud sync. It contains:

- encrypted private-store ciphertext and authenticated metadata;
- the format/version/data-set/key identifiers needed to parse it;
- at least the recovery envelope required to recover the DEK elsewhere; and
- an authenticated manifest describing completeness and supported format.

It must never contain a plaintext DEK, recovery phrase, PIN, PIN-KEK, or
recovery KEK. Use the backup-root derived from the DEK to authenticate/encrypt
a manifest and backup-specific data. Keep public file names and metadata
minimal; sizes/timestamps/transport destination can themselves leak facts.

### Restoration contract

1. The user chooses a backup explicitly; the app reads it into a staging area.
2. It validates bounds, versions, ciphertext authentication, and manifest
   completeness before modifying existing private data.
3. The recovery phrase plus included recovery envelope unwraps the DEK.
4. The restored store remains staged and inspectable. It does not delete or
   overwrite an existing data set by default.
5. The user selects an explicit import/replace/keep-both outcome after
   successful verification. The new device creates a local PIN envelope only
   after a new PIN is established.

This architecture means a backup plus recovery phrase can restore on a new
device; it also means theft of both can decrypt it. That trade-off is stated
openly, not hidden.

Rollback detection, backup retention, export media, off-device transport,
multiple data sets, and conflict/merge behavior are not yet selected. AEAD
detects tampering, but not an attacker replacing a valid backup with an older
valid backup unless a trusted monotonic state or user-visible version policy is
introduced.

## 9. Library and platform evaluation

No cryptographic dependency is added by this milestone. This is the shortlist
for a future, separately reviewed implementation. “Maintained” means there is
an active public project/release history visible during this review; it is not a
claim of a perpetual guarantee or a blanket security certification.

| Need | Candidate | Evidence and platform fit | Recommendation / caveat |
| --- | --- | --- | --- |
| CSPRNG | getrandom plus rand_core | Established Rust ecosystem path to OS randomness on Linux/aarch64 | Use a direct, current release after 6.2 dependency review; never make a custom RNG |
| BIP-39 | rust-bitcoin bip39 | Public BIP-39 implementation with standard word lists and optional zeroization support; pure Rust makes Linux and Pi integration simple | Preferred mnemonic parser/generator; pin version and test official BIP-39 vectors |
| PIN KDF | RustCrypto argon2 | Pure Rust Argon2 implementation from RustCrypto password-hashes; RFC 9106 defines Argon2id parameters | Preferred, using explicit Argon2id parameters rather than defaults; benchmark each target |
| Domain separation | RustCrypto hkdf plus sha2 | Direct implementation of the RFC 5869 construction; pure Rust | Preferred for recovery/store/backup contexts; define exact context bytes in format spec |
| AEAD | RustCrypto chacha20poly1305 | Pure Rust XChaCha20-Poly1305; a 2020 NCC Group review found no vulnerabilities in the scoped earlier ChaCha20-Poly1305 commit | Preferred candidate, but the audit is not a certification of a later chosen version; review advisories and version at implementation time |
| Secret hygiene | zeroize and secrecy | zeroize provides compiler-resistant wiping; secrecy makes exposure explicit and intentionally avoids false mlock claims | Use together for narrow secret types; document residual OS/RAM risks |
| Alternative AEAD stack | libsodium through a Rust binding | Mature native library with documented XChaCha20-Poly1305 and wide Linux support | Reasonable alternative, but adds FFI/system dependency and different release/audit surface |
| Encrypted database | SQLCipher through a maintained Rust binding | Mature SQLite encryption approach but introduces C/native distribution and licensing questions | Evaluate separately in 6.4; do not add merely because a database is convenient |

The [RustCrypto password-hashes repository](https://github.com/RustCrypto/password-hashes),
[RustCrypto XChaCha20-Poly1305 documentation](https://docs.rs/chacha20poly1305/latest/chacha20poly1305/),
and [rust-bitcoin bip39 project](https://github.com/rust-bitcoin/rust-bip39)
are useful project sources. The [NCC Group review](https://pentestreports.com/files/reports/nccgroup/NCC_Group_MobileCoin_RustCrypto_AESGCM_ChaCha20Poly1305_Implementation_Review_2020-02-12_v1.0.pdf)
is valuable audit evidence, but is explicitly limited to the commits reviewed in
2020. No claim here says every transitive dependency or future version has been
independently audited.

### Maintenance, audit, and integration assessment

| Candidate family | Maintenance evidence reviewed | Security-history evidence reviewed | Linux and ARM64/Pi fit | Integration cost |
| --- | --- | --- | --- | --- |
| RustCrypto primitives | Public RustCrypto repositories, changelogs, release policy/security files, and active documentation | The algorithm specifications are independent standards; the public NCC review is scoped to an earlier AEAD commit only | Pure Rust avoids a new system crypto library and is portable to the two current Linux targets | Low-to-moderate: several narrow crates and a format specification, but no FFI |
| rust-bitcoin bip39 | Public rust-bitcoin project, standard word-list support, public release/source history | BIP-39 defines interoperability; this review does not treat the crate as independently audited | Pure Rust and suitable for Linux/aarch64 | Low: generate/validate mnemonic and standard seed only |
| secrecy/zeroize | Current public documentation with deliberately narrow guarantees | Their value is accidental-leak reduction, not a claim of hostile-memory protection | Pure Rust; same API on both targets | Low: use only for narrowly typed secrets and avoid exposing/serializing them |
| libsodium binding | Mature native library and well-documented AEAD interface | Long-lived ecosystem, but a chosen binding/version/packaging path still needs review | Linux/aarch64 support depends on system package or bundled build policy | Moderate: FFI, native builds, release provenance, and a second toolchain surface |
| SQLCipher binding | Mature upstream encrypted-SQLite approach | Requires evaluating the chosen binding, SQLite build, and license/update policy at the time | Viable on Linux/Pi only after native build/distribution checks | Moderate-to-high: native dependency plus schema/key/migration policy |

“Maintained” is therefore a selection criterion, not a permanent conclusion.
Before adding any crate or native library, the implementation milestone must
record the exact version, license, advisory/security-policy check, supported
architectures, transitive-native dependencies, and test-vector evidence. The
current repository's Cargo.lock helps reproduce a resolution but is not a
supply-chain assurance program by itself.

## 10. Approval-required decisions

Do not begin implementation until the owner chooses or delegates these:

1. **PIN policy:** numeric-only versus passphrase, minimum length, retries,
   cooldown/lockout UX, and whether the app may offer a stronger passphrase
   option. Recommendation: do not market a short numeric PIN as strong
   standalone offline protection.
2. **Recovery passphrase policy:** recommendation is BIP-39 standard derivation
   with an empty optional passphrase in v1. Approve or reject adding a second
   recovery secret later.
3. **Envelope encoding:** canonical binary versus canonical CBOR, public
   metadata fields, atomic-write/fallback policy, and backward compatibility.
4. **Encrypted storage engine:** small application record store versus SQLCipher
   or another explicitly reviewed option.
5. **Lock behavior:** inactivity duration, sleep/wake behavior, model/media
   cancellation, and what remains visible on a locked terminal.
6. **Backup policy:** local export media, storage locations, retention,
   rollback/version display, restoration replacement/merge behavior, and
   whether any remote transport is ever allowed.
7. **Hardware-rooted protection:** whether to support TPM2/FIDO2/external
   hardware and on which verified platforms, without making it mandatory.
8. **Ceremony UX:** whether the v1 spoken “next” channel is approved only after
   its isolated privacy design is tested; private button/keyboard fallback is
   recommended.
9. **Release security bar:** dependency scanning, lockfile review, SBOM/provenance,
   reproducible-build target, independent review/audit scope, and security
   disclosure process before a persistent private-data release.

## 11. Next implementation boundary

Milestone 6.1 is complete as the non-cryptographic lifecycle and Ephemeral
Mode foundation. The next possible milestone is 6.2, but it is **not
automatically authorized**. It must first resolve the key/envelope and recovery
approval questions above. In particular, do not add a PIN, mnemonic, DEK,
database, or backup merely because the state types now exist.

## References

- [BIP-39 specification](https://github.com/bitcoin/bips/blob/master/bip-0039.mediawiki)
- [RFC 9106: Argon2](https://www.rfc-editor.org/rfc/rfc9106.html)
- [RFC 5869: HKDF](https://www.rfc-editor.org/rfc/rfc5869.html)
- [Libsodium AEAD and XChaCha20-Poly1305 guidance](https://doc.libsodium.org/secret-key_cryptography/aead)
- [Linux cryptsetup/LUKS manual](https://man7.org/linux/man-pages/man8/cryptsetup.8.html)
- [Linux mlock manual](https://www.man7.org/linux/man-pages/man2/mlock2.2.html)
- [secrecy crate security boundaries](https://docs.rs/secrecy/latest/secrecy/)
- [zeroize crate guarantees and limits](https://docs.rs/zeroize/latest/zeroize/)
- [Trove threat model](https://trove.distrust.co/threat-model.html)
