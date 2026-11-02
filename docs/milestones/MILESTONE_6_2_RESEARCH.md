# Milestone 6.2 research — Authentication and key management

> **Status:** Research for owner review. This document is not an implementation
> plan and does not approve a cryptographic format, dependency, or product
> policy by itself.
>
> **Research date:** 2026-11-01. Library releases, operating-system packages,
> and hardware support must be rechecked when an implementation milestone is
> approved.

## Executive summary

Independence already has the right **policy boundary** for this work: the
process starts in Ephemeral Mode, `SecurityManager` owns the lifecycle, and
production code has no way to enter Private Mode or obtain private-storage
authority. It does **not** yet have a PIN, a key, encryption, private storage,
or a recovery phrase. That is an honest and useful starting point.

The research supports this direction for the future private-data design:

```text
PIN (temporary input)                 24-word BIP-39 phrase (future, never stored)
          |                                           |
     Argon2id + salt                          BIP-39 seed + HKDF context
          |                                           |
       PIN KEK                                  recovery KEK
          |                                           |
          +------- independently unwrap -------+
                          one random 256-bit DEK
                                      |
                          encrypted private records
                          and encrypted backups
```

- Generate one random 256-bit **data-encryption key** (DEK), then protect
  separate copies of that same key with independently derived key-encryption
  keys (KEKs). A PIN change re-wraps the DEK rather than re-encrypting all
  personal data.
- Use a versioned, authenticated key envelope. A strong candidate is
  Argon2id for the PIN KDF, HKDF-SHA-256 for labeled subkeys, and
  XChaCha20-Poly1305 for authenticated encryption. These are established
  primitives with maintained Rust implementations; they are building blocks,
  not a complete authentication product.
- A short numeric PIN is inherently weak against someone who copies the
  envelope and guesses offline. Argon2id makes each guess more expensive, but
  cannot create entropy that was not in the PIN. This makes PIN length,
  Argon2 calibration, retry behavior, and full-disk encryption product
  decisions—not implementation details.
- LUKS2 protects a powered-off system disk or removed SD/NVMe device. It does
  not provide Private-versus-Ephemeral application semantics, portable
  encrypted backups, or protection from a compromised running OS. Application
  encryption and LUKS2 address different layers and can eventually complement
  one another.
- Raspberry Pi 5 documentation describes a CPU cryptographic extension and
  secure-boot facilities, but its published board components do not list a
  TPM 2.0 device. Do not assume one exists. TPM-backed protection needs a
  supported physical TPM and Linux driver; FIDO2 needs a compatible USB/NFC
  security key. Both are useful future optional factors, not prerequisites for
  an initial PIN design.
- Do not make GNOME Keyring, KWallet, or a desktop Secret Service provider the
  root of Independence security. They are useful desktop credential services,
  but add a session/D-Bus/provider dependency and do not give a headless Pi a
  portable, independently recoverable data-encryption architecture.

**Recommended direction for discussion, not an approved implementation:**
keep the current `SecurityManager` ownership boundary; use an application-level
envelope and encrypted private store for the data Independence owns; consider
LUKS2 separately during device provisioning; defer FIDO2 and TPM choices until
there is actual target hardware and a recovery story. Require a measured
Argon2id profile for each supported device class before selecting a numeric PIN
policy.

## How to read this report

| Label | Meaning |
| --- | --- |
| **Current implementation** | Verified by inspecting this repository at the research date. |
| **Verified external fact** | Supported by an upstream specification, official documentation, or primary project source linked nearby. |
| **Recommendation** | A reasoned direction for the owner to discuss; not approved or implemented. |
| **Assumption** | A working premise that must be checked for a specific device or product decision. |
| **Owner decision** | A consequential choice that should be explicitly approved before implementation. |

This is intentionally not a Milestone 6.2 build plan. It records the
security properties a later approved milestone should preserve and the
questions it must answer.

### What this research did not verify

No PIN, key, phrase, envelope, disk format, or authentication factor was
created while researching. This report also did not benchmark Argon2id on the
actual Linux Mint computer or Pi, attach a FIDO2 key, probe a TPM, provision
LUKS2, or perform a fresh-device recovery test. Those are future validation
questions, not evidence supplied by the source links.

---

## 1. Current Independence boundary

### What exists now

**Current implementation.** `src/security.rs` owns a process-local
`SecurityManager`, with `Uninitialized`, `SetupInProgress`, `Locked`,
`Unlocked`, and `RecoveryRequired` lifecycle states. It separately names
`OperatingMode::Ephemeral` and `OperatingMode::Private`. The controller cannot
manufacture private-storage authority.

**Current implementation.** Normal typed, dictation, and voice launches obtain
an opaque Ephemeral session. `--mode private` is deliberately rejected before
model, microphone, camera, terminal, or Piper initialization. Production code
has no route to `Unlocked`. `src/controller.rs` retains accepted conversation
turns and fresh camera photos in memory for the running session; orderly
shutdown and `/reset` clear application-owned context. Ephemeral Mode rejects
`/look` before file I/O but still permits an explicit fresh camera capture.

**Current implementation.** Voice capture, Whisper transcription, local
Ollama requests, Piper output, and camera photos are bounded in-memory work in
the existing prototype. The application intentionally displays dialogue in the
terminal. Existing telemetry redaction excludes dialogue from configured JSONL
and OTLP paths, but terminal scrollback, redirection, the OS, drivers, the
local model process, swap, and crash dumps remain outside an
application-memory-clearing guarantee. See the current
[security architecture](../security/ARCHITECTURE.md) and
[threat model](../security/THREAT_MODEL.md).

### What does not exist now

**Current implementation.** There is no generated DEK, PIN creation or
verification, recovery phrase, authenticated envelope, encryption,
private-storage adapter, persistent unlock state, backup, FIDO2 enrollment, or
TPM integration. This report does not turn any of those into implemented
behavior.

### Why this matters

The later authentication system should grant a narrow capability to private
storage only after it verifies a real unlock factor. It should not make the
controller, a CLI mode flag, a hardware button, Whisper, Ollama, or a model
response an authentication authority. Ephemeral Mode must remain independently
usable and must never receive the capability.

---

## 2. Scope, assets, and trust boundaries

### Assets worth protecting

| Asset | Why it matters | Desired property |
| --- | --- | --- |
| Private conversation history and personal memories | Can disclose relationships, health, plans, beliefs, and identity. | Confidentiality and integrity at rest; unavailable while locked. |
| Private photographs and visual information | Can expose people, documents, homes, screens, and location. | Explicit capture/load, integrity, encrypted persistence if retained. |
| Audio, transcripts, and voice-associated context | Can expose speech, environment, identity, and sensitive content. | Bounded handling; encryption if persisted; never use recovery/PIN audio. |
| Personal settings and model/device configuration | May disclose habits or endpoints and affect safety. | Integrity; confidentiality when personally revealing. |
| Master DEK | Unlocks all encrypted Independence private data. | Never serialize, log, model-submit, or expose outside the security boundary. |
| PIN, derived PIN key, and recovery phrase/seed | They unlock an envelope. | Never persist or transmit in plaintext; minimize lifetime. |
| Encrypted envelopes and backups | Enable future recovery but are valuable offline-guessing targets. | Confidentiality/integrity, versioning, backup and restore policy. |
| Device identity and security state | Determines whether an unlock target is the intended data set. | Authenticated binding, tamper/corruption detection, explicit recovery state. |

### Trust boundaries

```text
user input / physical controls
             |
             v
      authentication UI      <-- must not use Whisper/Ollama/logging for secrets
             |
             v
       SecurityManager       <-- only owner of unlocked DEK / storage authority
             |
             +--> private storage / envelopes / backup codec
             |
             +--> narrow lease to controller for private history only

controller <--> microphone / camera / Piper / Ollama / terminal
             (sensitive but not authentication authorities)
```

The OS, kernel, drivers, device firmware, local Ollama process, child Piper
process, physical observers, and a user who has access to an unlocked terminal
are outside an application-only trust boundary. Local-first removes an
unnecessary cloud party; it does not make a compromised local machine safe.

---

## 3. Threat model and candidate protections

The following compares what a mechanism can and cannot do. It is not a claim
that Independence currently provides these protections.

| Scenario | Attacker capability and target | Useful protection | What remains exposed / residual risk |
| --- | --- | --- | --- |
| Powered-off device theft | Takes laptop, Pi, SD card, or NVMe. Targets data and envelopes. | LUKS2 full-disk encryption plus a well-designed application envelope; strong passphrase/PIN and recovery hygiene. | Weak PINs remain guessable offline; boot or hardware attacks may be possible; unencrypted boot metadata can still reveal some facts. |
| Storage removal/offline copy | Reads or copies raw storage and envelope files. | LUKS2 prevents raw filesystem reads when locked; app AEAD protects app records/backups independently. Argon2id slows PIN guesses. | An attacker can copy and retry forever; application UI cooldowns do not rate-limit an offline copy. |
| Envelope theft | Has public headers, salts, nonce, ciphertext/tag. | AEAD rejects modified ciphertext/AAD; Argon2id slows PIN guessing. | A PIN with low entropy is still an offline target; recovery phrase plus recovery envelope intentionally unlocks data. |
| Envelope alteration, swap, or downgrade | Replaces, truncates, rolls back, or corrupts files. | Versioned authenticated headers/AAD, strict parsing, atomic writes, stored data-set/key IDs, backups. | AEAD detects changes but cannot prevent deletion, denial of service, or rollback unless a trusted monotonic state exists. |
| Power loss during setup/PIN change | Interrupts a write. | Staged/atomic replacement and preserving an old valid authorization path. | Cannot guarantee disk availability or recover data without a valid envelope/backup. |
| Compromised OS while unlocked | Reads process memory, input, screen, or calls as current user/root. | Least privilege, update hygiene, FDE at rest, optional TPM/FIDO can reduce some offline risk. | App-level encryption, `zeroize`, and a PIN do **not** protect plaintext or a loaded DEK from a hostile unlocked OS. |
| Application crash while holding a DEK | Leaves copies in memory, core dump, swap, or debugging state. | Minimize copies; redaction; `zeroize` best-effort; disable/secure dumps and swap operationally. | No portable Rust feature proves all copies are erased or inaccessible after a crash. |
| PIN forgotten | Owner loses PIN but still has recovery material and encrypted data. | Independent recovery envelope can rewrap the same DEK under a new PIN. | If the phrase/envelope/backup is unavailable, data cannot be recovered honestly. |
| Recovery phrase lost | Owner loses phrase but remembers PIN. | PIN envelope still unlocks; owner can create a replacement recovery envelope after authenticating. | If both PIN and all recovery material are lost, neither app nor vendor should have a hidden bypass. |
| Physical observation/listening | Watches entry, reads terminal, hears speaker, records spoken words. | Private entry UX; avoid voice recovery/PIN; screen/privacy practices. | Shoulder surfing, terminal scrollback, room microphones, and camera observation are not fixed by encryption at rest. |

### Security goals versus non-goals

**Recommendation:** define the first security goal narrowly and honestly:
protect Independence private data from an attacker who obtains powered-off
storage or a copied backup, subject to the strength of the unlock material and
the chosen KDF. Preserve integrity enough to reject malformed/tampered records.

This does **not** mean protection from malware or a hostile administrator while
the device is unlocked, secret recovery from an owner who lost all factors and
data, protection from physical observation, or cryptographic proof of secure
RAM erasure.

---

## 4. Password and PIN authentication research

### How established systems turn a password into a key

**Verified external fact.** KeePassXC's documented KDBX4 database format uses
an Argon2-based key-derivation step over a composite master key. Its user guide
recommends Argon2/Argon2id, describes the memory-hardness advantage against
GPU/ASIC attackers, and encourages calibration to a tolerable delay on the
user's hardware rather than a universal fixed setting.
[KeePassXC User Guide — database encryption](https://keepassxc.org/docs/KeePassXC_UserGuide#_database_encryption)

The useful pattern for Independence is *not* copying KeePassXC's database
format. It is separating a human secret from a random data key:

1. The person enters a PIN/password; it is never stored.
2. A password-hardening KDF derives a KEK from that input and a unique public
   salt.
3. The KEK authenticates/decrypts one small wrapped DEK.
4. The random DEK, rather than the PIN-derived value, encrypts data.

This permits changing a PIN by replacing only its wrapper. It also permits a
future recovery factor to unwrap the same DEK independently.

### Argon2id

**Verified external fact.** [RFC 9106](https://www.rfc-editor.org/rfc/rfc9106.html)
defines Argon2 and recommends Argon2id when an application is unsure which
variant to choose or needs both Argon2i's side-channel-aware first pass and
Argon2d's resistance to tradeoff attacks. The RFC gives two *example* profiles:
one pass with 2 GiB and four lanes, or, when memory is constrained, three
passes with 64 MiB and four lanes. It explicitly frames parameters as a trade
off between available memory and acceptable runtime.

**Recommendation:** use Argon2id, a distinct random salt per PIN envelope,
and record the full KDF algorithm/version/parameters/output length in the
authenticated envelope. Do not derive a KEK by applying raw SHA-256 or a fast
hash to a PIN. Fast hashes make cheap, parallel offline guessing easier.

#### Raspberry Pi 5 calibration

**Assumption:** a Pi 5 may have 4 GB, 8 GB, or more RAM and will have very
different headroom from a Linux Mint laptop. The current project also runs
Whisper, Piper, and an optional local model, so a KDF profile cannot be chosen
from a laptop benchmark alone.

**Recommendation for discussion:** start evaluation with RFC 9106's
memory-constrained 64 MiB / 3-pass profile as a baseline *measurement*, then
test higher memory costs on each supported Pi memory size and laptop class. A
later product choice could target a deliberate unlock delay (for example,
roughly 0.5–2 seconds on an otherwise idle supported device) while leaving
enough memory for a reliable startup. Parallelism should be measured rather
than assumed to help a four-core Pi.

No parameter makes a weak PIN strong. The selected profile must be stored per
envelope, because a new device may be faster or slower than the device that
created it. A product should cap accepted parameters before allocating memory,
so an attacker cannot turn a corrupted header into a memory-exhaustion request.

### PIN entropy and offline guessing

Each decimal digit contributes at most `log2(10)`, about 3.32 bits, when it is
chosen uniformly at random. Human-picked values such as birthdays, sequences,
and repeated digits have less entropy than this table.

| Numeric PIN length | Maximum search space | Maximum entropy | Research interpretation |
| ---: | ---: | ---: | --- |
| 4 | 10,000 | 13.3 bits | Not suitable for a copied encrypted envelope. |
| 6 | 1,000,000 | 19.9 bits | Common but still a small offline search space. |
| 8 | 100,000,000 | 26.6 bits | Better UX/security balance, still modest for a determined offline attacker. |
| 10 | 10,000,000,000 | 33.2 bits | A plausible **minimum candidate** only if generated/random and paired with a measured Argon2id profile. |
| 12 | 1,000,000,000,000 | 39.9 bits | Preferred numeric-PIN candidate if the user accepts it; a passphrase can offer more entropy/usability. |

**Verified external fact.** NIST distinguishes online throttling from offline
attack resistance: an offline attacker with a copied verifier is limited by the
secret's entropy and the cost per guess, while rate limiting works only where
the verifier can enforce it. [NIST SP 800-63B](https://pages.nist.gov/800-63-4/sp800-63b.html)

**Recommendation:** do not represent a 4–8 digit PIN as adequate standalone
offline protection. Consider a minimum 10-digit *randomly generated* numeric
PIN and recommend 12 digits, or allow a memorable multiword passphrase. The
owner must approve the usability/security threshold. Local attempt cooldowns
are still valuable against a person at the device, but must never be presented
as protection against a copied envelope.

### Failed attempts

**Recommendation:** distinguish two controls:

- A local retry/cooldown policy slows casual guessing at the device and avoids
  repeated expensive KDF work.
- Argon2id makes every offline copy attempt costly.

Neither substitutes for recovery. Never erase data automatically after a small
number of failed PIN attempts: accidental lockout and malicious triggering are
too harmful for Independence's recovery goal.

---

## 5. Linux device security research

### LUKS2 and cryptsetup

**Verified external fact.** [cryptsetup](https://gitlab.com/cryptsetup/cryptsetup)
is the established Linux implementation/tooling for dm-crypt and LUKS.
LUKS2 supports multiple keyslots protecting a volume key; cryptsetup's
[FAQ](https://gitlab.com/cryptsetup/cryptsetup/-/blob/main/FAQ.md) explains
that current LUKS2 uses Argon2 for improved resistance to GPU/high-parallelism
password attacks. Its documentation also emphasizes header backups and notes
that loss or damage of headers/keyslots can make data irrecoverable.

| Layer | What it protects | What it does not protect |
| --- | --- | --- |
| LUKS2 full-disk/full-volume encryption | Filesystem contents on a powered-off or removed drive until the volume is unlocked. It also protects non-Independence data on that volume. | Data while the volume is mounted; a compromised or authorized running OS; Independence-specific private/ephemeral separation; a portable encrypted backup format. |
| Application-level encrypted private store | Independence data and backups independent of filesystem layout; allows `SecurityManager` to withhold data from Ephemeral Mode. | Other files on disk; plaintext in a running unlocked process; hostile OS/root; metadata not deliberately encrypted. |
| Both | Defense in depth: FDE for device/storage theft plus app encryption for app-level authority and backups. | Neither magically protects a compromised unlocked operating system or physical observation. |

**Recommendation:** treat LUKS2 as a separate device-provisioning choice, not
as a replacement for application encryption. The later application design
should work on an existing LUKS2 system but not require a desktop environment
or control boot setup. For a product image, offering documented LUKS2/FDE is
worth discussing because the Pi's removable storage is an unusually accessible
offline-theft target.

### systemd-cryptenroll

**Verified external fact.**
[systemd-cryptenroll](https://www.freedesktop.org/software/systemd/man/latest/systemd-cryptenroll.html)
can enroll passphrases, recovery keys, FIDO2 authenticators, PKCS#11 tokens,
and TPM2 devices for LUKS2 volumes. Its FIDO2 support requires an authenticator
with the `hmac-secret` extension. Availability and supported options vary by
the installed systemd/cryptsetup version, so a target image must be inspected
rather than assumed.

This can be attractive if Independence eventually uses LUKS2: it lets Linux
own boot-volume unlocking and established token plumbing. It does not by itself
solve Independence's in-process DEK, ephemeral/private boundary, encrypted
backup, or recovery ceremony.

### TPM2 on Linux Mint and Raspberry Pi 5

**Verified external fact.** The Rust `tss-esapi` crate is a high-level Rust
wrapper for the TPM2 Software Stack's ESAPI; it relies on the native TPM2-TSS
stack. See [tss-esapi](https://docs.rs/tss-esapi/latest/tss_esapi/) and
[tpm2-tss](https://github.com/tpm2-software/tpm2-tss).

**Verified external fact.** The official Raspberry Pi 5 product brief lists
the BCM2712, RP1 I/O controller, RAM, wireless, microSD, PCIe, MIPI, GPIO,
RTC, and power facilities, but no TPM2 device.
[Raspberry Pi 5 product brief](https://datasheets.raspberrypi.com/rpi5/raspberry-pi-5-product-brief.pdf)
Raspberry Pi also documents a separate secure-boot chain rooted in boot ROM and
OTP-configured signing keys; secure boot is valuable but is **not** evidence of
a general TPM2 key-storage interface.
[Raspberry Pi security architecture](https://www.raspberrypi.com/documentation/security/security-architecture.html)

**Recommendation:** assume neither a TPM2 nor a secure enclave on the current
Pi 5 until a real device shows a supported TPM resource-manager node (commonly
`/dev/tpmrm0`) and working TPM2 tooling. A TPM-backed future feature would
require an actual compatible TPM2 module/firmware path, kernel driver/device
tree support, provisioning policy, and a recovery factor. Do not confuse the
Pi's secure boot capability with a built-in TPM.

**Assumption:** many Linux Mint PCs may expose a firmware or discrete TPM, but
that must be probed per machine. The app should not make Private Mode depend on
one, because the Pi target cannot currently assume it.

### Cross-target capability matrix

| Approach | Linux Mint laptop | Raspberry Pi OS / Pi 5 | Additional hardware or operating-system condition |
| --- | --- | --- | --- |
| App PIN + encrypted envelope | Yes, independent of desktop environment. | Yes, independent of desktop environment. | No special hardware; needs an OS CSPRNG, safe filesystem behavior, and a chosen private-store format. |
| LUKS2 | Generally available through cryptsetup and initramfs support. | Available on a suitable Pi OS/storage layout, but must be deliberately provisioned and tested. | Storage planning and boot/initramfs support; it is an OS/device choice, not an app switch. |
| systemd FIDO2 enrollment for LUKS2 | Possible if installed systemd/cryptsetup are recent enough. | Possible in principle on a compatible Pi OS release; verify actual package feature set. | A FIDO2 key with `hmac-secret`, USB/HID access/udev permission, and a matching LUKS2 setup. |
| App-level FIDO2 factor | Possible through a chosen native library/binding. | Possible through the same Linux USB HID path. | A USB/NFC-capable FIDO2 key; native dependencies and safe device permissions. |
| TPM2 sealing | Possible only if the particular computer exposes a usable firmware/discrete TPM2. | Not safe to assume on stock Pi 5 hardware. | A real TPM2 with kernel/TSS support; Pi generally needs a separately supported TPM module/board path. |
| Secret Service/keyring | Often practical inside a configured desktop session. | Possible only if a provider and session bus are deliberately installed/managed. | A running provider and stable session behavior; not appropriate as the required root. |

The table distinguishes compatibility from a product recommendation. “Possible”
does not mean Independence currently includes it, or that it is equally usable
on a headless Pi.

---

## 6. Hardware authentication research

### FIDO2 security keys

**Verified external fact.** FIDO2/CTAP2 security keys communicate over
transports such as USB HID, NFC, and Bluetooth LE; the
[FIDO CTAP specification](https://fidoalliance.org/specs/fido-v2.2-rd-20241003/fido-client-to-authenticator-protocol-v2.2-rd-20241003.pdf)
defines this client-to-authenticator protocol. A USB key can provide a
possession factor and, depending on protocol/token support, user presence or
user verification.

**Verified external fact.** [libfido2](https://github.com/Yubico/libfido2) is
Yubico's actively maintained C library/tool set for FIDO U2F/CTAP1 and
FIDO2/CTAP2 over USB/NFC. It is BSD-2-Clause licensed, supports Linux, and
lists a Rust binding. It brings native dependencies (including libcbor,
OpenSSL, zlib, and on Linux libudev) and appropriate `hidraw`/udev permissions.

| Possible role | Benefit | Limitation / recovery implication |
| --- | --- | --- |
| Alternative unlock factor | A physical key can unlock a DEK envelope without relying solely on a memorized PIN. | Lost or damaged key must not make data unrecoverable; keep PIN and/or recovery envelope. |
| Supplement to PIN | Possession + PIN can make theft of either factor alone insufficient. | More interaction, provisioning, replacement, and support complexity. |
| LUKS2 boot-volume unlock | `systemd-cryptenroll` can integrate compatible FIDO2 tokens with LUKS2. | Does not grant Independence app-level key authority; requires supported OS/token configuration. |

**Recommendation:** treat FIDO2 as a future optional factor, not the initial
authentication foundation. The product needs enrollment/revocation, replacement
and loss workflows, and a guarantee that recovery remains possible. It should
never make a USB key the only copy of an otherwise unrecoverable private key.

### Rust FIDO alternatives

| Project | What it is | Research assessment |
| --- | --- | --- |
| [libfido2](https://github.com/Yubico/libfido2) plus its Rust binding | Mature native FIDO library with direct USB/NFC access. | Strongest upstream maturity in this survey, but imports a native stack and device permissions. Worth evaluating only when hardware auth is approved. |
| [`fido2-rs`](https://crates.io/crates/fido2-rs) | Rust bindings for libfido2. | A wrapper can reduce FFI work, but maintenance/API/safety must be reviewed at the time of adoption. Do not assume a wrapper inherits a complete security review from the C library. |
| [`webauthn-authenticator-rs`](https://docs.rs/webauthn-authenticator-rs/latest/webauthn_authenticator_rs/) | Rust CTAP2 client/authenticator library. | Its own documentation says it is pre-1.0, not FIDO certified, lacks a thorough security review, and has edge cases that can permanently lock an authenticator. Do **not** select it for Independence's first security-key implementation without a fresh review and a very strong reason. |
| [`webauthn-rs`](https://docs.rs/webauthn-rs/latest/webauthn_rs/) | Rust WebAuthn server-side library. | Useful for web relying parties, not the direct device-unlock/key-wrapping problem. Avoid treating it as a local USB-token solution. |

### TPM2-backed key storage

A TPM can hold or seal cryptographic material so a copied storage device cannot
unseal it elsewhere, potentially with a boot-state policy (PCRs). This is a
useful physical-device binding, not a cure for every threat:

- It is strongest against offline storage theft when the TPM policy and boot
  chain are correctly configured.
- It does not make an unlocked, compromised OS trustworthy.
- Firmware updates, measured/secure-boot policy, and PCR changes can create
  recovery/support problems.
- A lost/broken device requires an independent recovery envelope or backup.

**Recommendation:** no TPM-only path. If future hardware supports it, use it
as an additional envelope/slot or LUKS2 enrollment route with a separately
maintained recovery option. The owner should decide whether device binding is a
desired product promise before hardware-specific work begins.

---

## 7. Existing secret-management systems

### Secret Service, GNOME Keyring, and KWallet

**Verified external fact.** The
[Secret Service API](https://specifications.freedesktop.org/secret-service/latest-single/)
is a D-Bus protocol for collections/items of secrets. It establishes sessions
over the session bus; the specification explicitly does not mandate a master
password or a particular access-control model. It also cautions that secrets
passed through the D-Bus service can move through multiple processes and be
cached by the OS.

**Verified external fact.** [KWallet](https://github.com/KDE/kwallet) describes
itself as desktop-wide password storage for KDE workspaces and can provide or
proxy the Secret Service interface. GNOME Keyring is another common provider.
Both are useful desktop integration points, not a cross-platform, headless
device-root specification.

| Option | Fit for Independence | Reason |
| --- | --- | --- |
| Make Secret Service/GNOME Keyring/KWallet the master-key authority | **Avoid.** | A standalone Pi may not have a graphical session, session D-Bus, provider, or predictable unlock semantics. It also would not define Independence backup/recovery or its Ephemeral boundary. |
| Offer an optional desktop integration later | Possible, separately evaluated. | Could store an auxiliary token/credential on an owner-managed Linux desktop, but becomes an optional convenience and must not be sole recovery path. |
| Own a small application envelope/private-store boundary | **Recommended direction.** | Keeps lifetime, recovery, backup, mode isolation, and cross-target behavior under explicit Independence policy. It still requires careful design/review. |

These services do not remove the need for a real key hierarchy. They shift
where one secret is stored and add a desktop/runtime trust dependency.

---

## 8. Rust cryptographic building blocks

The following are *candidate building blocks*, not a complete authentication or
key-management system. Versions must be pinned through Cargo.lock and reviewed
with advisories, licensing, features, MSRV, and target builds when a later
milestone proposes dependencies.

| Library / standard | What it provides | Maturity, license, and evidence | Fit / limitation |
| --- | --- | --- | --- |
| [`argon2`](https://github.com/RustCrypto/password-hashes/tree/master/argon2) (RustCrypto) | Argon2i/d/id password hashing and raw KDF APIs; parameter validation. | Active RustCrypto `password-hashes` repository, Apache-2.0 or MIT; current changelog and docs show maintained releases and a KDF interface. [Docs](https://docs.rs/argon2/latest/argon2/) | **Recommended candidate** for PIN-to-KEK derivation. The standard/parameters matter more than the crate alone; no crate can compensate for a weak PIN. |
| [`hkdf`](https://docs.rs/hkdf/latest/hkdf/) (RustCrypto) / [RFC 5869](https://www.rfc-editor.org/rfc/rfc5869.html) | HMAC-based extract-and-expand key derivation with labels/context. | RustCrypto, Apache-2.0 or MIT; implements established RFC construction. | **Recommended candidate** for domain-separated subkeys from a DEK or BIP39 seed. Do not use it as a password hardener. |
| [`sha2`](https://docs.rs/sha2/latest/sha2/) (RustCrypto) / [FIPS 180-4](https://csrc.nist.gov/pubs/fips/180-4/upd1/final) | SHA-2 digests and HMAC dependency material. | RustCrypto, Apache-2.0 or MIT; documented pure Rust implementation. | Appropriate underneath HKDF/BIP39-compatible operations. **Not** a PIN hash by itself. |
| [`chacha20poly1305`](https://github.com/RustCrypto/AEADs/tree/master/chacha20poly1305) (RustCrypto) | ChaCha20-Poly1305 and XChaCha20-Poly1305 authenticated encryption. | RustCrypto AEAD collection is Apache-2.0 or MIT. A 2020 NCC Group review covered a historical MobileCoin/RustCrypto AEAD scope; it is evidence, not a perpetual certification of current releases. [Audit report](https://pentestreports.com/reports/nccgroup/NCC_Group_MobileCoin_RustCrypto_AESGCM_ChaCha20Poly1305_Implementation_Review_2020-02-12_v1.0.pdf) | **Recommended candidate** for compact envelope/record AEAD, subject to protocol review. XChaCha is widely deployed but is an extension rather than the RFC 8439 96-bit-nonce construction. |
| [`zeroize`](https://github.com/RustCrypto/utils/tree/master/zeroize) (RustCrypto) | Best-effort zeroing primitives using volatile writes/compiler barriers. | RustCrypto, Apache-2.0 or MIT; maintained utility crate. | **Recommended hygiene candidate** for PIN/DEK buffers and derived material. Does not stop copies, swap, a debugger, a malicious kernel, or CPU remanence. |
| [`secrecy`](https://docs.rs/secrecy/latest/secrecy/) | Secret wrappers that reduce accidental `Debug`/display exposure and zeroize on drop when configured. | Documented as Apache-2.0 or MIT; maintained Rust crate. Its docs explicitly say it does not provide `mlock`/`mprotect`-style guarantees. | **Recommended ergonomic companion**, not a memory-security boundary. Avoid `Clone`, logging, serialization, and broad ownership regardless. |
| [`bip39`](https://github.com/rust-bitcoin/rust-bip39) | BIP-39 mnemonic parsing/generation/seed derivation, including 24 words. | Rust Bitcoin ecosystem; package/docs list CC0-1.0. [Docs](https://docs.rs/bip39/latest/bip39/) show a maintained API and supported word counts/languages. | **Recommended candidate** only for the required standard mnemonic behavior. It is not a general data-encryption system and should not receive the phrase from voice/transcript paths. |
| [`getrandom`](https://docs.rs/getrandom/latest/getrandom/) | OS CSPRNG access. | Widely used Rust OS-randomness adapter, Apache-2.0 or MIT. | Candidate for generating DEKs, salts, and nonces; do not invent a random generator. |

### Library diligence and security-history limits

This research checked primary project pages, current documentation, licenses,
and visible release/source activity. It did **not** perform a source audit,
reproduce an external audit, or assert that a project has no vulnerabilities.
That distinction matters more for cryptographic code than for ordinary utility
code.

| Candidate | Documentation and maintenance assessment | Security-history assessment for this research |
| --- | --- | --- |
| RustCrypto `argon2` | Clear Rust API/docs, parameters and password-hash/KDF traits; active upstream changelog and Apache-2.0/MIT licensing. | It implements the Argon2 standard; this report relies on RFC 9106 for algorithm guidance. No claim is made here of a complete independent audit of the exact future pinned crate version. |
| RustCrypto `hkdf` / `sha2` | Small documented implementations in the same maintained ecosystem, dual licensed, with standard references. | Their use is deliberately narrow: HKDF for separation, SHA-2 underneath standard constructions. A future review must check version advisories and test vectors; raw SHA-2 is rejected for PIN hardening. |
| RustCrypto `chacha20poly1305` | Maintained AEAD collection with documented `XChaCha20Poly1305` support and dual licensing. | A public NCC Group review exists for a historical RustCrypto/MobileCoin AEAD scope, but it does not certify all later releases or an Independence envelope protocol. The protocol and nonce/AAD use still need review. |
| `zeroize` / `secrecy` | Well-documented Rust crates for zeroization hygiene and accidental-log avoidance. | Their own limitations are material: no proof against allocator copies, swap, crash dump, debugger, or malicious OS. They reduce accidents; they are not a secure enclave. |
| Rust Bitcoin `bip39` | Direct BIP-39-oriented API/docs, CC0 licensing, and ecosystem adoption. | It must be checked against BIP-39 vectors and exact normalization/word-list requirements at adoption. It is not evidence that a surrounding recovery ceremony is safe. |
| `tss-esapi`, libfido2, and their bindings | Mature upstream stacks exist, but integration adds native libraries, drivers, device permissions, and hardware-specific behavior. `tss-esapi` is Apache-2.0; libfido2 is BSD-2-Clause. | The trust boundary grows substantially. [`tss-esapi`'s changelog](https://github.com/parallaxsecond/rust-tss-esapi/blob/main/CHANGELOG.md) records a 2022 fix for an auth-session nonce vulnerability, illustrating why exact versions matter. A Rust wrapper's safety and maintenance must be evaluated separately from its C upstream; security-key/TPM flows need hardware loss and recovery testing. |

**Recommendation:** at implementation approval, record the exact version,
license, transitive native dependency, supported target architecture, upstream
security-reporting channel, advisory scan result, and known test-vector source
for every security-critical dependency. This is a dependency-review gate, not
a promise that a general-purpose crate supplies a complete key-management
system.

### AEAD choice: XChaCha20-Poly1305

**Verified external fact.** XChaCha20-Poly1305 uses a 192-bit nonce. The
[Libsodium construction documentation](https://doc.libsodium.org/doc/secret-key_cryptography/aead/chacha20-poly1305/xchacha20-poly1305_construction)
explains that the large nonce allows random nonces to be used safely in
practice, while still requiring no nonce reuse with the same key. AEAD also
authenticates optional associated data before plaintext is returned.

**Recommendation:** XChaCha20-Poly1305 is a good fit for an
Independence-specific envelope because it is compact, portable to both target
architectures, and makes a fresh random nonce per small record practical. It
is not a substitute for correct unique-nonce generation, versioned AAD,
bounded parsing, or a coherent storage/backup protocol.

**Alternative:** AES-256-GCM is broadly standardized and can use CPU
acceleration on some machines, but has a much smaller nonce space with stricter
nonce-management concerns. It could be a valid choice if a future
interoperability requirement demands it. No such requirement is currently
documented, so this research does not recommend switching merely for fashion.

### Secure-memory limitations in ordinary language

`zeroize` helps tell the compiler not to optimize away an explicit clear.
`secrecy` helps prevent accidentally formatting a secret into a log. Neither
can prove that temporary allocations, stack copies, registers, swap, hibernation
images, core dumps, backups, a debugger, or a compromised kernel no longer
contain a key. This is why the design should minimize the number and lifetime
of copies and treat OS hardening/operational settings as a separate layer.

---

## 9. Recommended key hierarchy for discussion

### Master key

**Recommendation:** generate a random 256-bit DEK from the operating system
CSPRNG during real first-time setup. It should be an opaque byte value, not
derived from the PIN, recovery words, a device serial number, model name, or
user identity.

Why this helps:

- A high-entropy DEK can encrypt records efficiently without requiring a user
  to remember a high-entropy secret.
- Multiple independent unlock factors can encrypt the same DEK.
- Changing a PIN changes a small wrapper, not every private conversation or
  photo.
- Recovery can restore access to the same data without a vendor-held key.

### PIN and recovery wrappers

```text
          generated once: random 32-byte DEK
                         |
        +----------------+----------------+
        |                                 |
PIN input                         24-word recovery phrase
  |                                   |
Argon2id(salt, params)          BIP-39 standard seed operation
  |                                   |
PIN root key                     HKDF-SHA-256(recovery context + salt)
  |                                   |
XChaCha20-Poly1305(DEK)         XChaCha20-Poly1305(DEK)
  |                                   |
PIN envelope                     recovery envelope
```

**Recommendation:** keep the envelopes independent: distinct salt, nonce,
envelope ID, KDF context, and authenticated metadata. They intentionally
contain the same encrypted DEK but should never share a KEK or nonce.

### HKDF and domain separation

**Verified external fact.** [RFC 5869](https://www.rfc-editor.org/rfc/rfc5869.html)
specifies HKDF as extract-and-expand key derivation; the `info` field is meant
to bind derived output to an application/context.

**Recommendation:** use one fixed, versioned label for each distinct purpose,
such as `Independence/private-store/v1`, `Independence/backup/v1`, and
`Independence/recovery-wrap/v1`. This prevents a key derived for one role from
silently becoming a key for another. If an envelope only needs one 32-byte KEK,
it may use a directly derived Argon2id output; HKDF adds value when separating
multiple purposes from the same high-entropy source. The exact choice should
be specified once, not improvised at each call site.

### Holding the DEK at runtime

**Recommendation:** only the future security component owns the unlocked DEK.
It lends a narrow, revocable private-storage capability to the storage adapter;
the controller should receive decrypted history through that adapter, not the
raw DEK. Avoid `Clone`, `Debug`, `Display`, serialization, logging, model
requests, subprocess arguments/environment, or broad async task capture.
Use small secret wrappers/zeroization hygiene and revoke access on normal
shutdown or future lock.

This reduces accidental exposure. It does not defeat a compromised running OS.

---

## 10. Key-envelope research

### A proposed semantic contract

The eventual byte encoding is an **owner-approval and design-review decision**.
The following is a recommended semantic contract, not an implemented format:

| Field | Public or secret | Reason |
| --- | --- | --- |
| Magic value, format version, suite identifier, envelope kind | Public and AEAD-authenticated | Reject unknown/downgraded formats; distinguish PIN/recovery/hardware slots. |
| Data-set ID, key ID, envelope ID | Public and authenticated | Bind a wrapper to one intended data collection and support independent slots. |
| KDF identifier, version, parameters, salt | Public and authenticated | Reproduce a legitimate KDF safely; bound allocation before execution. |
| AEAD nonce | Public and authenticated | Unique per encryption under its KEK. |
| Ciphertext plus authentication tag | Secret/authenticated | Contains exactly the DEK and small fixed internal versioning material. |
| Creation/replacement metadata | Public and authenticated | Supports migration/audit reasoning without granting access. |

**Recommendation:** authenticate all interpreted header fields as AEAD
associated data (AAD). Otherwise an attacker could relabel or swap a valid
ciphertext between roles/data sets even if they cannot decrypt it. Validate
sizes, enums, version, data-set/key identity, KDF bounds, nonce length, and
encoding before costly KDF work.

### Envelope behavior

| Event | Security behavior to preserve |
| --- | --- |
| Correct PIN or recovery factor | Derive the appropriate KEK, AEAD-authenticate/decrypt the DEK, then grant limited private-storage authority. |
| Wrong PIN/phrase or modified envelope | Report a generic authentication/integrity failure; do not reveal which secret/field was right. Do not modify data. |
| PIN change | Verify existing authority; create a new PIN envelope; preserve recovery envelope and encrypted data. |
| Recovery | Verify recovery envelope; create a new PIN envelope only after successful recovery; do not destroy old data by default. |
| Corrupt/missing slot | Enter an explicit recovery-required/error path, not a fake fresh initialization. |
| Interrupted write | Preserve at least one valid authorization path through staged/atomic replacement and recovery copies. |

### Atomicity, corruption, rollback, and availability

**Recommendation:** use a staged write protocol appropriate to the chosen
filesystem: write a complete new authenticated record to a temporary location,
flush file data and metadata as required, atomically rename/switch it, and
flush the parent directory. Preserve a valid old slot until the new one is
durable. Test power-loss cases before claiming a guarantee.

AEAD detects byte tampering, but not deletion or a replay of an older valid
envelope. Preventing rollback requires trusted monotonic state, a remote
witness, a TPM monotonic facility, or a carefully designed user-visible backup
policy—none currently exists in Independence. This is a residual risk that
must be documented rather than hidden.

**Verified external fact.** LUKS2 itself has metadata locking/redundancy
mechanisms, but its documentation still stresses header backup and recovery
planning. That is a useful lesson for Independence's small envelope: integrity
checks do not create availability.
[cryptsetup FAQ](https://gitlab.com/cryptsetup/cryptsetup/-/blob/main/FAQ.md)

---

## 11. Authentication lifecycle research

The intended experience is authenticate at startup, stay unlocked for normal
use, clear app-owned key material on shutdown, and offer Ephemeral Mode without
authentication. The following describes required semantics—not a user-interface
or code plan.

| Lifecycle point | Recommended semantic behavior | Important caveat |
| --- | --- | --- |
| First initialization | Create data-set identity, DEK, PIN envelope, and eventually recovery envelope only after a successful verified ceremony. End in `Locked`, not an unlocked session. | Must be atomic enough that power loss cannot silently create an ambiguous half-initialized private store. |
| Startup | Read public state/envelopes; offer ephemeral use without any unlock material; offer private unlock only with a real authentication factor. | An ordinary CLI flag, phrase in a transcript, or model output must never count as authentication. |
| Successful PIN unlock | Derive KEK, authenticate envelope, retain DEK only in security boundary, and authorize private storage. | A successful UI event does not protect against a hostile unlocked OS. |
| Failed PIN attempt | Generic error, local cooldown/attempt policy, no state mutation granting access. | Cooldown does not protect an offline copy. |
| PIN change | Existing unlock/recovery authority rewraps DEK under new PIN. | It is not a data-key rotation and must not silently invalidate recovery. |
| Normal shutdown | Stop private work, revoke capabilities, clear buffers best-effort, end process. | Cannot promise secure erasure outside app ownership. |
| Unexpected power loss | On next startup, parse/verify state; if no valid consistent path exists, enter recovery-required/error rather than initialize a new private identity. | Availability depends on durable envelope/backups. |
| Missing/corrupt metadata | Do not guess or overwrite. Preserve forensic/recovery opportunity and make Ephemeral Mode available. | A recovery phrase alone cannot reconstruct missing ciphertext or a missing envelope. |

### Suspend, hibernation, and the “stay unlocked” experience

The stated product experience is one startup authentication followed by normal
unlocked use—not recurring prompts or a dedicated lock button. That leaves an
important **owner decision** for suspend and hibernation:

- Suspend normally retains RAM, so a DEK may still exist while the device is
  asleep. A local screen lock is not equivalent to cryptographically clearing
  it.
- Hibernation writes RAM state to storage. It should not be treated as safe for
  a loaded application key unless the hibernation/swap path is itself protected
  and the associated operational assumptions are understood.
- A product can choose to remain unlocked through suspend, clear keys before
  suspend, or disable hibernation for this device role. Each choice trades
  convenience, availability, and theft exposure. The present milestone does
  not decide it.

### Relationship to Ephemeral Mode

Ephemeral Mode is not a failed Private Mode and is not a temporary
authentication state. It must stay available from `Uninitialized`, `Locked`,
and `RecoveryRequired`; it should start with empty in-memory conversation
context and no private-storage capability. It can use local models, camera,
voice, and temporary conversation context under the existing consent rules,
but must not load a private history merely because the device has a valid
envelope on disk.

---

## 12. Future recovery phrase research

### BIP-39 facts and fit

**Verified external fact.** [BIP-39](https://github.com/bitcoin/bips/blob/master/bip-0039.mediawiki)
defines a mnemonic representation of entropy. A 24-word English mnemonic
represents 256 bits of entropy plus checksum bits. Its standardized
mnemonic-to-seed operation uses PBKDF2-HMAC-SHA512 with 2,048 iterations,
UTF-8 NFKD normalization, and salt `"mnemonic" + passphrase`, yielding a
64-byte seed.

**Recommendation:** preserve that BIP-39 derivation exactly through a
maintained BIP-39 library; feed its resulting seed into a labeled HKDF step to
obtain an Independence recovery KEK. Do not use BIP-39's PBKDF2 as the PIN
KDF and do not invent a different phrase hash.

The phrase is a recovery secret, not a backup of the actual encrypted data:

> Anyone who obtains your recovery phrase and a copy of your encrypted device
> data or backup may be able to access your information.

The phrase alone cannot recreate data after every device and encrypted backup
is lost.

### BIP-39 passphrase

**Owner decision.** BIP-39 permits an optional passphrase. It can add a secret,
but it creates a second unrecoverable thing to remember and a high-risk support
failure mode. The initial recommendation is an empty BIP-39 passphrase, with
future consideration only through an explicit, separately designed recovery
experience. It must never be silently added.

### Spoken recovery ceremony risk

The current app has microphone capture, Whisper, Piper, terminal dialogue, and
local inference. Those are excellent reasons *not* to speak, transcribe, or
model-submit recovery words.

| Risk | Recommended future ceremony constraint |
| --- | --- |
| Whisper transcribes a recovery word | Never route phrase words through Whisper or the ordinary dictation pipeline. |
| Piper/room audio is re-recorded by microphone | Keep microphone capture disabled during phrase display/verification; prefer physical buttons/display-only navigation. |
| `next` voice command causes incidental capture | If any spoken navigation is considered, isolate it from the phrase UI, discard audio immediately, prohibit transcription/logging, and validate that it cannot transition into phrase capture. A physical control is safer. |
| Terminal, telemetry, logs, or model prompt records words | Phrase material must be redacted by construction: no `Debug`, logs, telemetry, conversation history, error messages, or model prompts. |
| Observer photographs/screen-records words | Present one word at a time, give privacy guidance, allow pause/cancel/restart, and verify random selected positions without speaking the words. |

**Recommendation:** use physical/display interaction rather than voice for
the actual phrase ceremony. The existing phrase-by-phrase concept (one word at
a time; user-controlled advance; selected-word verification; pause/cancel) is
sound only if recovery text never enters normal audio, terminal dialogue, or
model paths.

---

## 13. Encrypted backup and restoration architecture

### Recommended shape for later discussion

```text
private records --encrypted under--> DEK-derived store key
       |                                      |
       +--> encrypted backup payload <--------+
                         |
          backup manifest (version, data-set ID, key ID, integrity data)
                         |
       matching PIN and/or recovery envelope (never plaintext DEK)
```

**Recommendation:** an encrypted backup should contain encrypted private data,
public version/identity metadata, and enough encrypted-envelope material to
recover the same DEK with an authorized PIN/recovery path. It must never carry
a plaintext DEK, PIN, recovery phrase, or unencrypted transcript/photo/audio.

The owner should be able to restore nondestructively onto another device: read
and authenticate the backup first, keep the destination's data intact until
validation succeeds, then establish a new local PIN envelope. A recovery phrase
plus a valid encrypted backup/envelope can unlock the same DEK; a phrase with
no surviving encrypted data cannot recreate data.

### Backup trade-offs

| Choice | Benefit | Cost / risk |
| --- | --- | --- |
| Include a recovery envelope in backup | A recovered device can access its original encrypted data. | Theft of both backup and phrase compromises privacy. |
| Exclude a recovery envelope | Limits one kind of backup exposure. | Makes cross-device recovery harder or impossible without another authorized envelope. |
| Keep multiple backup versions | Helps recover from corruption/ransomware/user mistakes. | Increases retained sensitive ciphertext and rollback-management complexity. |
| Encrypt only with device TPM | Strong device binding. | Breaks portability/recovery when hardware is lost; not acceptable as only backup key path. |

No backup implementation is proposed here. The format, destination, retention,
integrity/rollback policy, and restore UX need their own owner-approved scope.

---

## 14. Comparison of approaches and what to borrow

| Project/approach | Problem it solves | Maintenance/adoption and license | Independence lesson | Recommendation |
| --- | --- | --- | --- | --- |
| [KeePassXC](https://keepassxc.org/) | Local encrypted password database with a master-password KDF. | Mature desktop open-source project; GPL-2.0-or-later. | Borrow the random database-key / memory-hard KDF thinking and device-calibrated delay, not its database format or UI. | Learn from design; do not embed/copy format. |
| [cryptsetup/LUKS2](https://gitlab.com/cryptsetup/cryptsetup) | Linux block-device encryption/keyslots. | Mature Linux ecosystem project; GPL-2.0-or-later. | Use for FDE/device provisioning and study keyslot/atomic metadata concepts. | Consider separately as defense in depth; not app private-store substitute. |
| [systemd-cryptenroll](https://www.freedesktop.org/software/systemd/man/latest/systemd-cryptenroll.html) | Enrollment of FIDO2/TPM2/PKCS#11/recovery factors into LUKS2. | Maintained with systemd; platform/version dependent. | Leverage rather than recreate boot-volume FIDO/TPM plumbing if LUKS is chosen. | Future device-provisioning evaluation, not current app dependency. |
| [libfido2](https://github.com/Yubico/libfido2) | Direct FIDO2/U2F token communication. | Active upstream, BSD-2-Clause, native dependencies. | Prefer it or an adequately reviewed binding over inventing CTAP. | Future optional hardware factor only. |
| [TPM2-TSS](https://github.com/tpm2-software/tpm2-tss) / [`tss-esapi`](https://github.com/parallaxsecond/rust-tss-esapi) | TPM2 command stack / Rust ESAPI wrapper. | Established TPM software stack; C/native dependency; needs real TPM hardware. | Do not fake TPM semantics on Pi. | Future optional hardware support after hardware/product decision. |
| Secret Service / GNOME Keyring / KWallet | Desktop credentials via D-Bus services. | Established desktop integrations; provider/session dependent. | Useful ecosystem APIs but poor root of trust for a headless, portable device. | Avoid as primary authority. |
| RustCrypto building blocks | Cryptographic primitives/KDFs in Rust. | Active ecosystem, dual Apache-2.0/MIT; some historical scoped audits. | Compose only a small reviewed protocol; versions/features matter. | Strong candidate set for later proposal. |
| Custom cipher, custom mnemonic, direct PIN hashing, or ad-hoc envelope | Attempts to solve key management from scratch. | No established review or interoperability. | High risk of subtle confidentiality/integrity/recovery failures. | Reject. |

---

## 15. Recommended direction, alternatives, and limitations

### Recommended direction for owner discussion

1. Keep the existing security lifecycle and `SecurityManager` ownership model.
   It already prevents a controller or mode selection from claiming an unlock.
2. For future Private Mode, use a generated random 256-bit DEK rather than
   encrypting data directly with a PIN.
3. Use independently derived PIN and future recovery KEKs to wrap the same
   DEK in distinct authenticated, versioned envelopes.
4. Favor Argon2id, HKDF-SHA-256, XChaCha20-Poly1305, `zeroize`, `secrecy`, a
   BIP-39 library, and OS randomness—subject to a dependency/version review
   when actual implementation is approved.
5. Make numeric-PIN policy honest: require enough random digits or offer a
   passphrase, calibrate Argon2id on supported hardware, and explain that
   offline guessing remains a risk.
6. Treat LUKS2 as optional device-layer defense in depth, with
   systemd-cryptenroll considered for OS-level FIDO2/TPM enrollment if and
   only if a product provisioning decision adopts it.
7. Preserve a recovery route independent of optional hardware. Make phrase
   viewing/verification non-voice and non-model-mediated.

### Alternatives considered

| Alternative | Why it is not preferred as the initial foundation |
| --- | --- |
| Encrypt all data directly from a PIN | PIN changes require data re-encryption; no clean independent recovery factor; weak PIN becomes the data key. |
| Store the DEK only in GNOME Keyring/KWallet | Desktop/session/provider dependent; does not work predictably for a headless Pi or define backup/recovery semantics. |
| Depend solely on LUKS2 | Excellent at rest for a whole device, but cannot enforce Private/Ephemeral app authority or provide portable encrypted application backups. |
| TPM-only unlock | Current Pi 5 cannot assume a TPM; hardware loss/update/PCR policy can lock out data; does not replace recovery. |
| FIDO2-only unlock | Security key loss/breakage is an unacceptable single point of failure without recovery. |
| A 4–6 digit PIN plus cooldown | A copied envelope bypasses the local cooldown; entropy is too low for the desired theft threat. |
| Store a recovery phrase in the app “encrypted” | It defeats the purpose of an independent user-held recovery secret and creates a high-value local target. |

### Limitations that remain even with the recommended design

- It cannot protect data from a malicious OS/process while Independence is
  unlocked.
- It cannot prove every RAM copy disappears at shutdown/crash.
- It cannot restore data if all encrypted copies/envelopes are gone.
- It cannot make a low-entropy PIN immune to offline guessing.
- It cannot stop physical observation of a phrase/PIN or terminal dialogue.
- It cannot by itself establish that an external USB token/TPM/device firmware
  is trustworthy.

---

## 16. Decisions requiring owner approval

These are intentionally unresolved. Recommendations are included to make the
trade-offs concrete, but none should be silently selected.

1. **PIN policy:** Is the product willing to require a 10-digit minimum,
   recommend 12 random digits, or offer a passphrase? What usability cost is
   acceptable for materially better offline resistance?
2. **Argon2id calibration:** What target unlock latency and memory budget are
   acceptable on a 4 GB Pi 5, higher-memory Pi, and Linux Mint laptop? Should
   profiles be device-calibrated at setup or selected from audited presets?
3. **Retry/cooldown:** What local failure delay and feedback are humane without
   falsely claiming offline protection? Is there any policy for a device under
   physical attack?
4. **BIP-39 passphrase:** Should the first recovery design use the recommended
   empty BIP-39 passphrase, or introduce a second recovery secret with its
   added risk of permanent loss?
5. **Envelope encoding:** Canonical CBOR, a small fixed binary format, or a
   different reviewed encoding? What metadata must be visible and authenticated?
6. **LUKS2 product position:** Optional documented setup, required product
   provisioning, or out of scope for now? Which storage media/partition layout
   is supported?
7. **Hardware factors:** Are FIDO2 keys and/or external TPM2 modules desired
   future optional factors? What recovery/replacement support can the product
   realistically provide?
8. **Backup policy:** Where may encrypted backups live, how many versions are
   retained, and how should replay/rollback and restore verification work?
9. **Device identity/rollback:** Is detection of an old but valid envelope a
   required security property? If so, what trusted monotonic state is available
   without introducing cloud dependency?
10. **Operational hardening:** What should product guidance require for swap,
    hibernation, core dumps, SSH/local console access, updates, secure boot,
    and physical device handling?

---

## 17. Source notes and primary references

### Standards and official system documentation

- [RFC 9106 — Argon2 Memory-Hard Function](https://www.rfc-editor.org/rfc/rfc9106.html)
- [NIST SP 800-63B — Digital Identity Guidelines: Authentication](https://pages.nist.gov/800-63-4/sp800-63b.html)
- [RFC 5869 — HMAC-based Extract-and-Expand Key Derivation Function](https://www.rfc-editor.org/rfc/rfc5869.html)
- [FIPS 180-4 — Secure Hash Standard](https://csrc.nist.gov/pubs/fips/180-4/upd1/final)
- [BIP-39 — Mnemonic code for generating deterministic keys](https://github.com/bitcoin/bips/blob/master/bip-0039.mediawiki)
- [FIDO Client to Authenticator Protocol](https://fidoalliance.org/specs/fido-v2.2-rd-20241003/fido-client-to-authenticator-protocol-v2.2-rd-20241003.pdf)
- [LUKS2 format documentation](https://gitlab.com/cryptsetup/LUKS2-docs)
- [cryptsetup FAQ](https://gitlab.com/cryptsetup/cryptsetup/-/blob/main/FAQ.md)
- [systemd-cryptenroll manual](https://www.freedesktop.org/software/systemd/man/latest/systemd-cryptenroll.html)
- [Freedesktop Secret Service API](https://specifications.freedesktop.org/secret-service/latest-single/)
- [Raspberry Pi 5 product brief](https://datasheets.raspberrypi.com/rpi5/raspberry-pi-5-product-brief.pdf)
- [Raspberry Pi security architecture](https://www.raspberrypi.com/documentation/security/security-architecture.html)

### Primary projects and libraries

- [KeePassXC User Guide — encryption/KDF](https://keepassxc.org/docs/KeePassXC_UserGuide#_database_encryption)
- [cryptsetup source](https://gitlab.com/cryptsetup/cryptsetup)
- [libfido2](https://github.com/Yubico/libfido2)
- [TPM2 Software Stack](https://github.com/tpm2-software/tpm2-tss)
- [Rust `tss-esapi` bindings](https://github.com/parallaxsecond/rust-tss-esapi)
- [RustCrypto password-hashes / `argon2`](https://github.com/RustCrypto/password-hashes)
- [RustCrypto AEADs / `chacha20poly1305`](https://github.com/RustCrypto/AEADs/tree/master/chacha20poly1305)
- [RustCrypto `zeroize`](https://github.com/RustCrypto/utils/tree/master/zeroize)
- [Rust `secrecy` documentation](https://docs.rs/secrecy/latest/secrecy/)
- [Rust Bitcoin `bip39`](https://github.com/rust-bitcoin/rust-bip39)
- [Libsodium XChaCha20-Poly1305 construction notes](https://doc.libsodium.org/doc/secret-key_cryptography/aead/chacha20-poly1305/xchacha20-poly1305_construction)
- [KWallet source and Secret Service relationship](https://github.com/KDE/kwallet)

### Assessment caveat

Maintenance activity, source visibility, popular adoption, a security policy,
and historical audit reports are useful selection evidence. They are not a
guarantee that a particular version or composition is free of defects. A later
approved implementation should re-evaluate the exact pinned versions,
transitive dependencies, advisories, platform builds, and protocol-level test
vectors before claiming protection.

---

## Research conclusion

This research supports a conservative, local-first authentication architecture:
one random DEK, independently protected by a calibrated PIN envelope and a
future BIP-39 recovery envelope, with explicit mode/state ownership and no
invented cryptographic primitive. It recommends treating full-disk encryption,
FIDO2, and TPM2 as complementary layers whose value depends on actual hardware
and provisioning decisions.

The current Milestone 6.1 Ephemeral boundary should stay intact while these
decisions are reviewed. No Milestone 6.2 implementation work is authorized by
this document.
