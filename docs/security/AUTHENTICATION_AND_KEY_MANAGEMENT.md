# Authentication and key-management architecture

> **Status:** Architecture documented for review; no production authentication,
> encryption, key management, TPM integration, cloud backup, or recovery code
> exists yet.
>
> **Scope:** Milestone 6.2 is an architecture and product-decision milestone.
> It records the agreed security model that later implementation work must
> follow. It does not authorize implementation.

## Read this first

Independence has two deliberately different experiences:

- **Ephemeral Mode** is the default. It requires no authentication, uses
  temporary in-memory context, and has no private-storage capability.
- **Private Mode** will require successful startup authentication before
  private data becomes available.

Milestone 6.1 already implements this policy boundary in `src/security.rs`:
production launches can enter Ephemeral Mode, while Private Mode is rejected
because a real authenticator and private store do not exist. Nothing in this
document changes that behavior.

This document makes the future design explicit:

```text
                         private data and local backup source
                                      |
                                      v
                              random 256-bit DEK
                                      |
                  +-------------------+-------------------+
                  |                                       |
                  v                                       v
        Local-device protection                   Recovery protection
                  |                                       |
  PIN -> TPM/secure-element release          24-word BIP-39 phrase
                  |                                       |
  hardware secret + PIN component            recovery-derived key
                  |                                       |
          local wrapping key                  recovery AEAD envelope
                  |                                       |
                  +------ unwraps the same DEK -----------+
                                                          |
                                                          v
                                  encrypted cloud backup, with recovery envelope
                                  only (never the local PIN/TPM envelope)
```

A short PIN is a convenient authorization factor for **this physical device**.
It is not the data-encryption key, is not the recovery secret, and cannot
decrypt a copied cloud backup.

## 1. Product decisions and requirements

The following are established product requirements, not open alternatives.

| Decision | Required outcome |
| --- | --- |
| Two modes | Ephemeral Mode remains unauthenticated and non-persistent. Private Mode needs actual authentication before private data is accessible. |
| Startup authentication | Private Mode authenticates at application startup and remains unlocked until shutdown. No per-question PIN, arbitrary idle timeout, or camera-specific PIN prompt is added here. |
| Everyday PIN | A short numeric PIN is the normal local interaction. It authorizes hardware-backed local release; it does not directly encrypt data or decrypt cloud backups. |
| Future hardware | A future TPM or secure element protects device-specific material, enforces or participates in retry protection, and prevents copied local envelopes from being brute-forced elsewhere. |
| Random master key | A cryptographically random 256-bit master data-encryption key (DEK) encrypts private data. It is independent of the PIN and recovery phrase. |
| Independent recovery | A 24-word recovery phrase can recover the DEK without the original device, TPM, or operating system. It is never sent to Whisper, Ollama, telemetry, ordinary logs, conversation history, or recordings. |
| Cloud backup | Cloud storage is untrusted blob storage. A backup contains encrypted private data, necessary metadata, and a recovery-protected DEK envelope—never a plaintext DEK/phrase or a PIN-only envelope. |
| Independent access paths | Local: PIN → hardware authorization → local envelope → DEK. Recovery: phrase → recovery-derived key → recovery envelope → DEK. Neither depends on the other. |

### What this does not decide

This milestone does not choose a physical TPM module, secure-element vendor,
cloud provider, storage engine, exact envelope serialization, final Argon2id
parameters, or BIP-39 passphrase policy. Those are genuine technical choices
listed later; none changes the requirements above.

## 2. Relationship to the existing 6.2 research

This architecture uses and narrows the findings in
[Milestone 6.2 research](../milestones/MILESTONE_6_2_RESEARCH.md).

### Findings retained

The research remains applicable on these points:

- use one randomly generated 256-bit DEK rather than deriving data encryption
  directly from a PIN;
- use independent, versioned, authenticated envelopes for independent access
  paths;
- use standard BIP-39 derivation for the recovery phrase;
- use Argon2id where a human PIN is transformed into an authorization secret;
- use HKDF for labeled key separation and an AEAD such as
  XChaCha20-Poly1305 for application-level authenticated encryption;
- treat `zeroize`/`secrecy` as useful hygiene, not as a guarantee against a
  hostile OS or all memory copies; and
- distinguish application encryption from LUKS2 full-disk encryption.

### Product decisions that refine the research

The earlier research intentionally offered alternatives because product
decisions were pending. This architecture resolves two of them:

1. **The short PIN is not an offline backup password.** It is accepted as
   everyday UX only because the local path is designed around future
   hardware-bound authorization and retry protection.
2. **Cloud backups are recovery-only.** The prior research discussed a
   backup containing a PIN and/or recovery envelope. The established product
   requirement supersedes that option: a cloud backup contains only the
   recovery-protected DEK envelope, never the local PIN/TPM envelope.

### Remaining research gaps

The research did not benchmark Argon2id on target hardware, attach a FIDO2
key, probe a TPM, provision LUKS2, or verify a particular secure element. It
also did not select exact on-disk bytes for an envelope. This architecture does
not pretend those checks occurred.

## 3. Terms and security boundaries

| Term | Meaning here |
| --- | --- |
| **DEK** | A random 32-byte data-encryption key. It is the root secret for Independence private data. |
| **KEK** | A key-encryption key used only to wrap/unwrap the DEK or a scoped derivative. |
| **Local envelope** | Device-specific TPM/secure-element blobs, public KDF/policy metadata, and an AEAD-wrapped DEK. The hardware releases a local secret only after PIN authorization; that secret plus a separate PIN-derived component unwraps the DEK. |
| **Recovery envelope** | A portable, recovery-phrase-protected wrapping of the same DEK. It can be stored locally and in an encrypted cloud backup. |
| **Hardware protector** | An abstract local capability implemented later by TPM 2.0 or a suitable secure element. It owns non-exportable device material and applies its authorization/retry policy. |
| **Private storage capability** | The narrow authority the future `SecurityManager` grants after a valid DEK becomes available. It is not a raw key exposed to the controller. |
| **Cloud backup** | Untrusted ciphertext and metadata. The cloud account authorizes upload/download, not decryption. |

### Separation of responsibilities

```text
                          PIN / recovery phrase
                                   |
                                   v
                         authentication presentation
                       (not Whisper, not Ollama, not logs)
                                   |
                                   v
                         SecurityManager policy owner
                          /                       \
                         /                         \
     local HardwareKeyProtector                recovery-envelope codec
              |                                         |
      device-bound secret release                  phrase-derived KEK
                         \                         /
                          \                       /
                           private-storage authority
                                      |
                                      v
                         controller receives data, never DEK
```

The controller, microphone, camera, Piper, Ollama, terminal renderer, and
telemetry system never decide that someone is authenticated. The local
hardware protector never decides what conversation data to reveal; it only
releases device-bound protected key material after its policy is met.

## 4. Key hierarchy

### 4.1 Root key and key separation

At real initialization, Independence will generate one random 256-bit DEK from
the operating system CSPRNG. It is not derived from:

- the PIN;
- the recovery phrase;
- device serials, model names, user identity, or a model;
- a cloud account; or
- TPM public identifiers.

The DEK is the only root that must decrypt the private data set. It is assigned
a random, nonsecret `data_set_id` and `key_id` so envelopes and backups can
identify what they belong to without publishing the key.

```text
random DEK
   |
   +-- HKDF(DEK, data_set_id, "Independence/private-store/v1")
   |       -> private-store root
   |
   +-- HKDF(DEK, backup_id, "Independence/backup-content/v1")
   |       -> backup-content root
   |
   +-- HKDF(DEK, data_set_id, "Independence/backup-manifest/v1")
           -> backup-manifest authentication/root
```

The fixed labels are examples of **domain separation**: a key for one purpose
must not silently become a key for another. The exact strings/encoding require
a later format specification and test vectors.

### 4.2 Two wrappers for the same DEK

```text
                 same random DEK
                 /             \
                /               \
local two-component envelope       recovery AEAD envelope
 PIN + TPM/SE policy                BIP-39 phrase-derived KEK
 device-bound, nonportable          portable, cloud-backup eligible
```

The two paths use independent metadata, salts, identifiers, and nonces. They
are not two data keys and do not share a KEK.

### 4.3 PIN changes and key rotation

| Operation | What changes | What must not change accidentally |
| --- | --- | --- |
| PIN change | A fresh local hardware secret, TPM/SE object, Argon2id salt, and local envelope are created and verified before retiring the old local envelope. | The DEK, encrypted private records, recovery envelope, and cloud backup remain usable. |
| Recovery phrase replacement | A new phrase produces a new recovery-derived KEK and replacement recovery envelope. | The DEK remains the same until an explicit future DEK-rotation operation. |
| New/replaced TPM or secure element | A new local hardware envelope is created after recovery or existing authorized local access. | Old encrypted data and the recovery path remain valid. |
| DEK rotation | Requires deliberate migration/re-encryption of private records and backup generations. | It is **not** implied by a PIN or phrase change. It is deferred. |

A failed PIN change, phrase replacement, or hardware migration must preserve a
valid old path until the new path is durable and verified.

## 5. Local authentication and hardware protection

### 5.1 Revised selected local-envelope construction

The earlier draft described a sealed object containing the DEK directly. That
would bind the DEK to the TPM, but it left Argon2id with only a weakly defined
role and made compromise of the TPM's sealed-data boundary equivalent to
recovering the DEK. The revised architecture instead uses a **split-secret
local envelope**, based on the documented shape of systemd's TPM2 PIN
hardening: an Argon2id result is split between a TPM authorization value and a
separate component combined with a TPM-unsealed random secret.

This is an architectural construction, not a decision to reuse LUKS2 or
systemd code. The systemd source is useful evidence that the components can be
combined this way; Independence must still independently specify, test, and
review its own format.

```text
direct numeric PIN (direct local entry only)
                  |
          Argon2id, unique public salt, recorded parameters, 64-byte output
                  |
                  +---------------------------+
                  |                           |
                  v                           v
        A_tpm (first 32 bytes)       A_local (second 32 bytes)
                  |                           |
          TPM policy/HMAC proof               |
                  |                           |
TPM sealed random S (32 bytes) ---------------+
                  |                           |
                  +---- HKDF-SHA-256 ---------+
                               |
                       local wrapping key
                               |
                XChaCha20-Poly1305 local DEK envelope
                               |
                          same 32-byte DEK
```

At initialization, Independence creates a random 32-byte **hardware secret**
`S` and seals that value to the local TPM. It does not put the DEK itself in
the TPM sealed-data payload. A canonical `local_header` contains the public
envelope identity, TPM policy/object information, Argon2id salt/parameters,
and cipher-suite information. It is AEAD associated data. The plaintext of the
local DEK envelope contains `DEK || SHA-256(canonical local_header)`, so a
successful local unwrap must also confirm that the public metadata matches what
was sealed/encrypted at enrollment.

The selected local derivation is:

```text
A = Argon2id(PIN, local_argon_salt, params, output_length = 64)
A_tpm   = A[0..32]
A_local = A[32..64]
K_local = HKDF-SHA-256-Expand(
    HKDF-Extract(salt = S, IKM = A_local),
    info = "Independence/local-dek-wrap/v1" || suite ||
           data_set_id || key_id || local_envelope_id,
    L = 32)
C_local = XChaCha20-Poly1305_Encrypt(
    K_local, nonce, DEK || SHA-256(canonical local_header),
    AAD = canonical local_header)
```

The concatenations above mean a single length-delimited, canonical encoding;
they are not ad-hoc string joining. `S`, `A_tpm`, `A_local`, `K_local`, and the
DEK are transient secrets. Neither Argon2id half nor `S` is persisted in
plaintext. The PIN remains only in the direct-entry path long enough to derive
`A`, then is cleared best-effort.

[RFC 9106](https://www.rfc-editor.org/rfc/rfc9106.html) defines Argon2id and
requires selecting its memory, time, and parallelism costs for the deployment;
those values are not chosen by this document. [RFC
5869](https://www.rfc-editor.org/rfc/rfc5869.html) defines HKDF's
extract-and-expand construction and its application-specific `info` context.
The use of HKDF here is purpose separation after a successful hardware release,
not a claim that HKDF makes a weak PIN strong.

This keeps the established product boundary intact: the PIN does not encrypt
private records or a cloud backup. It participates only in one device's local
envelope.

### 5.2 Actual TPM 2.0 policy mechanism

The proposed TPM object is a sealed-data object made under a device-bound
storage parent. At enrollment, the caller supplies `S` as the sealed sensitive
data and `A_tpm` as the object's `authValue`; the TPM protects the object's
private area under its parent. The public template must use a SHA-256 name
algorithm so the 32-byte `A_tpm` fits the object authorization-value size.

Its required public attributes and policy are:

| Item | Required design | Why |
| --- | --- | --- |
| `authPolicy` | Digest produced in a **trial policy** by `TPM2_PolicyAuthValue()` followed by `TPM2_PolicyCommandCode(TPM2_Unseal)`; an approved PCR assertion may be added later. | Requires a policy session, proof of the object's auth value, and limits this policy to unsealing. |
| `userWithAuth` | **Clear.** | Prevents an alternate ordinary USER HMAC/password authorization path; use must satisfy the policy. |
| `noDA` | **Clear.** | Makes the object subject to TPM dictionary-attack protection. A set `noDA` bit would defeat the short-PIN premise. |
| `fixedTPM`, `fixedParent` | **Set**, with a non-migratable object template and verified device-bound parent. | Rejects a deliberately migratable local object; copied blobs alone must not become a second-machine oracle. |
| Authorization session | A policy/HMAC session satisfying `PolicyAuthValue`; never a raw TPM password session. | `PolicyAuthValue` puts the auth value into the authorization HMAC rather than asking the caller to send it as a password. |

`TPM2_PolicyAuthValue` is the important standard mechanism. It changes the
policy-session state so that using the policy requires an HMAC proving knowledge
of the object's `authValue`; it does not itself store or transform a PIN.
[TCG TPM 2.0 Part 1, published Revision 1.83, policy authorization](https://medias.cdn.trustedcomputinggroup.org/production/TPM_2_0_1_83_Part_1_Architecture_02a468b5a3.pdf)
describes the HMAC requirement, and the
[published TPM structures specification](https://medias.cdn.trustedcomputinggroup.org/production/TPM_2_0_1_83_Part_2_Structures_8a60a99d40.pdf)
defines `userWithAuth`, `fixedTPM`, `fixedParent`, and `noDA`.

At unlock, Independence starts a policy session, executes the policy assertions
in the enrolled order, then authorizes `TPM2_Unseal` with `A_tpm` in the HMAC
calculation. It receives `S` only on success, derives `K_local`, and then
authenticates/decrypts `C_local`. A raw password session such as `TPM_RS_PW`
is prohibited because it sends an authorization value as a password rather
than using the HMAC proof. This protects the value on the TPM protocol path;
it does **not** hide the PIN or derived values from a compromised host process.

### 5.3 Exact PIN and Argon2id role

The user supplies an ASCII numeric PIN through a dedicated local entry surface,
not through a terminal dialogue, microphone, Whisper, Piper, or Ollama. The
PIN itself never becomes a TPM `authValue`: host software first derives
`A_tpm` and `A_local` as shown above. This is a deliberate choice, not a
replacement BIP-39 derivation.

The following local-envelope fields are public and stored beside the TPM
object blobs: envelope version/ID; `data_set_id` and `key_id`; TPM parent
identity/name and public object blob; policy digest/version; `noDA` and other
required attribute expectations; Argon2id algorithm/version, salt, memory,
iterations, parallelism, and output length; local AEAD suite/nonce/ciphertext;
and canonical-header encoding/version. Lengths and parameter maxima are
validated before Argon2id allocates memory. The header is bound as AEAD AAD and
its hash is checked after local decryption. Public metadata is not secret, but
the application must never accept altered metadata as a valid envelope.

An attacker who copies only those bytes can reproduce the Argon2id computation
for a candidate PIN, but cannot verify a guess: they lack `S`, which is still
inside the original TPM. An attacker who has the original hardware gets an
**online TPM oracle**, not a portable offline oracle; the hardware retry policy
must constrain it. If an attacker obtains `S` through a TPM implementation
failure or physical extraction, `C_local` does become an offline PIN-verifier.
Argon2id then makes each candidate more expensive, but a short PIN remains a
low-entropy secret and may be guessed. This is useful defense in depth, not a
claim that Argon2id alone makes a short PIN safe.

The split construction is deliberately stronger than simply sealing the DEK:
even an attacker who learns only `S` still needs the independently derived
`A_local` to decrypt the local DEK envelope. The maintained
[systemd TPM2 implementation](https://github.com/systemd/systemd/blob/main/src/shared/cryptsetup-tpm2.c)
uses the analogous pattern of splitting an Argon2id result, supplying one half
to the TPM and combining the other with an unsealed secret using HKDF. Its
[user documentation](https://www.freedesktop.org/software/systemd/man/latest/systemd-cryptenroll.html)
also confirms that `--tpm2-with-pin=yes` hardens a PIN with Argon2id before it
is passed to the TPM. Independence borrows the security reasoning, not its
LUKS2 token format or runtime.

### 5.4 Dictionary attack protection, lockout, and the short-PIN gate

The required short-PIN protection is not a generic consequence of owning a
TPM. It is a three-part property:

| Layer | What is verified or required | What it does not prove |
| --- | --- | --- |
| TPM 2.0 standard | An object with `noDA` clear is subject to DA protection; `TPM_RC_LOCKOUT` means authorization of DA-protected objects is unavailable while the TPM is in lockout. | The standard does not choose safe retry values, provision the lockout hierarchy, or prove resistance to invasive attacks. |
| Candidate TPM and platform | Enrollment must read and test the relevant DA capability/state, confirm that incorrect `PolicyAuthValue` unseal attempts count toward lockout, and prove copied blobs do not unseal on another TPM. | One vendor's behavior does not establish every TPM's firmware quality, tamper resistance, or reset policy. |
| Independence | It must require `noDA` clear, use the reviewed policy/session, bound local retries, report lockout without retry loops, and refuse Private Mode if the hardware contract cannot be demonstrated. | It cannot protect a PIN typed into a compromised OS or prevent a malicious local process from causing a denial of service. |

TPM dictionary-attack state is global to the TPM, not scoped to one
Independence envelope. It has configurable maximum-failure and recovery
behavior, and another user/process can consume attempts. The TPM tools
documentation describes `max-tries`, recovery time, and lockout recovery time;
[`tpm2_dictionarylockout`](https://tpm2-tools.readthedocs.io/en/stable/man/tpm2_dictionarylockout.1/)
is a useful operational reference. The TPM commands specification says a
locked TPM returns `TPM_RC_LOCKOUT` for objects subject to DA protection and
may recover after waiting or after an authorized
`TPM2_DictionaryAttackLockReset()`.
[TCG TPM commands, published Revision 1.83 response codes](https://medias.cdn.trustedcomputinggroup.org/production/TPM_2_0_1_83_Part_3_Commands_cabd333ad3.pdf)

On lockout, Independence must remain locked (with Ephemeral Mode still
available), stop automatic attempts, and report that local unlock is
temporarily unavailable. It must not store a lockout-reset authorization in an
ordinary application envelope or silently reset the TPM. Recovery by phrase
remains independently possible. Provisioning ownership, lockout-reset
authority, values, persistence over resets, and a physical attack model are
candidate-TPM facts that must be verified before shipping short-PIN Private
Mode.

**Fail-closed product gate:** if the enrolled hardware cannot prove the policy
template, `noDA` setting, device binding, and credible lockout behavior, it is
not an Independence short-PIN protector. The device remains able to use
Ephemeral Mode; a software-only short-PIN fallback is prohibited. A different
high-entropy credential or hardware design would require separate owner
approval, not a silent downgrade.

### 5.5 Copying, migration, and TPM-dependent guarantees

The public/private blobs for the sealed object are intentionally stored with
the local envelope. Their confidentiality/integrity and non-portability depend
on the TPM parent hierarchy and object template, not on keeping the blobs
secret. `fixedTPM` and `fixedParent` are necessary template properties, but
they are not a magic statement that every TPM is equally resistant to physical
extraction or firmware compromise. The TPM specification itself notes that
`fixedTPM` describes hierarchy behavior rather than proving all key material
resides on one chip.

The enrollment acceptance test therefore must attempt only controlled,
non-production proofs: load the object on the enrolled TPM, fail several
incorrect authorization attempts, observe/report DA behavior, and demonstrate
that the envelope cannot be loaded/unsealed through a different TPM. It must
not treat a device node such as `/dev/tpmrm0`, a vendor name, or a successful
single unseal as evidence of the full security property.

### 5.6 PCR/boot-state binding

A TPM can add Platform Configuration Register (PCR) policy to a sealed object.
This can make a copied drive or unexpectedly modified boot path fail to release
the local DEK. It is not a free security upgrade:

- PCR selections must allow supported OS/firmware updates or users will be
  locked out.
- A valid boot state does not prove that the running OS is free of malware.
- A recovery phrase must remain able to recover the DEK even if PCR policy,
  firmware, or TPM state changes.

**Recommendation:** make PCR binding optional behind the same hardware
protector interface. Do not make it a required first hardware behavior until a
device provisioning and update policy exists.

### 5.7 Secure-element alternative

A secure element can satisfy the same architectural role only if it provides:

- non-exportable device-specific key material or a non-exportable protected
  object;
- a hardware-bound authorization/release operation;
- verifiable retry/lockout semantics for the user authorization;
- stable provisioning, replacement, reset, and driver behavior on Linux; and
- an API that does not require putting a plaintext long-term device key in
  Independence process memory.

A generic microcontroller, GPIO accessory, or USB mass-storage device that
merely stores a PIN or symmetric key does **not** meet this requirement.
Many secure elements do not offer TPM-equivalent policy, PCR, or retry
semantics; this is why the abstraction is capability-based rather than
branding a particular chip as “secure.”

### 5.8 Hardware-protector interface boundary

This is an architectural boundary, not a Rust trait or implementation
commitment. A backend must be able to answer these questions:

| Capability | Required behavior |
| --- | --- |
| Enrollment | Seal a fresh local hardware secret under a reviewed PIN policy and create a locally AEAD-wrapped DEK without writing the DEK or local secret to persistent app storage. |
| Release | After direct PIN authorization, release the local secret only if current hardware policy passes; let the `SecurityManager` authenticate the local DEK envelope. |
| Retry state | Report whether hardware-backed retry/lockout protection is active, unavailable, or locked out—without exposing secrets. |
| Device binding | Make copied local-envelope bytes unusable on another device unless an explicit, reviewed migration operation occurs. |
| Cancellation/error | Fail closed, clear transient data best-effort, and never leave Private Mode active after an uncertain release. |
| Replacement | Permit a fresh protector/envelope only after an already authorized local path or independent recovery path. |

This keeps the key hierarchy independent from one TPM library, TPM transport,
or secure-element vendor. It does **not** imply all future backends are equally
secure: capability evidence is part of enrollment.

### 5.9 Raspberry Pi 5 reality

A stock Pi 5 must be treated as **no hardware protector** unless a verified
TPM/secure element is attached and usable. The official product brief does not
list a TPM2 device; Pi secure boot is not a general TPM key-release API.
[Raspberry Pi 5 product brief](https://datasheets.raspberrypi.com/rpi5/raspberry-pi-5-product-brief.pdf)
[Raspberry Pi security architecture](https://www.raspberrypi.com/documentation/security/security-architecture.html)

Consequently, Private Mode must not claim the established short-PIN hardware
protection on the current prototype. Until suitable hardware is verified, the
existing Ephemeral Mode is the honest usable mode; any future software-only
fallback would need explicit owner approval because it weakens the short-PIN
threat model.

## 6. Recovery and cloud-backup protection

### 6.1 Recovery derivation: standard BIP-39, then Independence context

The phrase is a 24-word BIP-39 mnemonic generated from 256 bits of random
entropy. The mnemonic-to-seed operation is preserved **exactly** as BIP-39
specifies; Argon2id has no place in this path.

```text
24 BIP-39 words + optional BIP-39 passphrase
        |
PBKDF2-HMAC-SHA512, 2,048 iterations, 64-byte output
password = UTF-8 NFKD mnemonic
salt     = UTF-8 NFKD("mnemonic" || passphrase)
        |
64-byte BIP-39 seed
        |
HKDF-Extract(salt = recovery_envelope_salt, IKM = seed)
        |
HKDF-Expand(info = canonical recovery context, L = 32)
        |
recovery KEK
        |
XChaCha20-Poly1305 recovery envelope containing DEK || header digest
```

The canonical recovery HKDF context contains the fixed versioned label
`Independence/recovery-wrap/v1`, cipher suite, `data_set_id`, `key_id`, and
`recovery_envelope_id`, each length-delimited. Its distinct random
`recovery_envelope_salt` is public. This makes a seed-derived key for one
recovery envelope unusable as a key for another role or data set. The same
identities, version, suite, salt, nonce, and envelope type are authenticated as
AEAD associated data and repeated inside the decrypted plaintext for
cross-checking.

**Verified external fact.** BIP-39 specifies UTF-8 NFKD normalization,
PBKDF2-HMAC-SHA512 with 2,048 iterations, a 64-byte seed, and the literal salt
prefix `mnemonic`; no passphrase means the empty string. A 24-word mnemonic
encodes 256 bits of entropy plus its checksum.
[BIP-39 specification](https://raw.githubusercontent.com/bitcoin/bips/master/bip-0039.mediawiki)

The preferred initial product direction is an **empty BIP-39 passphrase**, but
this remains awaiting explicit owner approval. It means the written 24 words
are the complete recovery secret: easier to explain and recover, but anyone
who obtains the words and a recovery envelope/backup can derive the same seed.
A non-empty passphrase adds another independent secret, but it is not stored in
the envelope and every passphrase produces a syntactically valid BIP-39 seed.
Losing or mistyping it therefore looks like an ordinary recovery-envelope
authentication failure and permanently prevents recovery. It must never be
silently enabled or treated as a password-resettable account credential.

The recovery envelope stores only what is necessary to reproduce and bind this
derivation: format/cipher-suite/version; BIP-39 language/profile and
passphrase-mode indicator (not the passphrase); envelope/data-set/key IDs;
HKDF algorithm/label version and random salt; nonce; ciphertext/tag; and
bounded encoding lengths. It never stores the phrase, seed, passphrase, KEK,
or DEK in plaintext.

Future test work must use published BIP-39 test vectors (including the
specified non-empty `TREZOR` passphrase vectors) to verify the standard seed
step, then fixed vectors for the canonical HKDF context, XChaCha20-Poly1305,
and an end-to-end synthetic recovery envelope. Test vectors must contain only
fixture material, never a user phrase or real backup.

### 6.2 Cloud backup components and key separation

Cloud storage is untrusted transport and blob storage. A backup is a coherent
set of four things:

```text
outer public header
     +-- recovery envelope R (portable; recovery path only)
     +-- encrypted/authenticated manifest M
     +-- encrypted/authenticated payload chunks P[0..n]
```

The recovered DEK derives distinct backup-manifest and backup-payload keys with
HKDF purpose labels. The manifest key cannot be used as a payload key and vice
versa. The cloud backup contains encrypted private data, `R`, and the metadata
needed to identify and restore one generation. It never contains the local
TPM/secure-element object, local `C_local`, a PIN envelope/verifier, a
plaintext DEK, phrase, seed, or cloud-provider decryption credential.

The outer header is only a routing hint until cryptographic checks succeed. It
may state a backup ID and claimed generation, but it is not trusted merely
because the cloud returned it. The encrypted manifest is the authenticated
source of truth for all of these relationships.

### 6.3 Complete backup authentication before restore

Before a recovered backup can replace or augment local private data,
Independence must authenticate it as a complete, internally consistent set.
The required order is:

1. Parse bounded outer-header and recovery-envelope fields; reject unknown
   versions, kinds, cipher suites, sizes, duplicate IDs, and unsupported
   parameter values before expensive work.
2. Derive the recovery KEK from the phrase, authenticate/decrypt `R`, and
   verify its internal data-set ID, key ID, recovery-envelope ID/type, suite,
   and header digest against the public header. A wrong phrase, passphrase, or
   altered envelope yields one generic recovery/integrity failure.
3. Derive the manifest key from the recovered DEK; authenticate/decrypt `M`.
   Check that its data-set ID, key ID, backup ID, generation, format version,
   cipher suite, and recovery-envelope ID exactly match the authenticated `R`
   and the claimed outer values.
4. Verify that `M` binds the exact canonical recovery-envelope header and
   ciphertext identifier/digest expected for this backup, not merely an
   envelope with the same label.
5. Authenticate every payload chunk under the derived payload key. Each chunk's
   AEAD AAD includes data-set/key/backup IDs, generation, chunk ordinal, and
   format/suite. Compare the authenticated manifest's expected count, ordered
   chunk identifiers, lengths, and aggregate digest with the retrieved chunks.
6. Only after all checks pass, build restored private records in a fresh
   encrypted staging area. Do not activate, merge, or overwrite an existing
   private store during any earlier step.

The manifest must bind at least: `data_set_id`, `key_id`, `backup_id`,
generation, parent-generation reference if any, format/cipher versions,
recovery-envelope ID and canonical digest, payload chunk list/order/lengths,
payload aggregate digest, and creation metadata. Encryption or authentication
of an individual payload alone is not enough: a partial but valid set must not
be mistaken for a complete backup.

### 6.4 Replacement-device recovery: the product flow

The recovery experience on a replacement device is deliberately independent of
the original device, TPM, PIN, or cloud provider's ability to decrypt anything:

1. The user starts the replacement device in Recovery Required, not Private
   Mode. Ephemeral Mode remains available separately.
2. The user obtains the encrypted backup from their storage provider. It is
   downloaded as untrusted data into a staging location; the source backup is
   never modified by restore.
3. A dedicated recovery interface turns ordinary microphone capture off and
   accepts the 24 words without routing them through terminal dialogue,
   Whisper, Ollama, logs, or telemetry.
4. Independence performs the BIP-39, HKDF, and recovery-envelope checks in
   Section 6.1. It obtains the DEK only if `R` authenticates.
5. It performs every manifest/payload check in Section 6.3 before exposing or
   activating restored private records.
6. The user selects a new local numeric PIN. The replacement device creates a
   fresh local hardware secret, enrolls a new hardware-protected local envelope,
   and keeps the recovery envelope intact.
7. Independence verifies a fresh local unlock with the new PIN and hardware
   policy before it retires any previous local-envelope state on the replacement
   device.
8. Only then does it atomically activate the fully validated staged private
   data and permit the `SecurityManager` to grant Private Mode storage access.
9. The user can access restored private data; the old device's TPM object and
   PIN were never needed and are not copied.

Any failure leaves the source backup unchanged and does not overwrite a valid
destination data set. Phrase/AEAD/manifest failure discards only transient key
material and leaves the device locked. Download or payload incompleteness stays
in untrusted staging. Local TPM enrollment or its verification failure leaves
the recovered backup staged but inactive and retains the recovery route for a
safe retry; it must not create an unlocked Private Mode without a verified local
path. The final storage transaction requires its own later fault-injection and
durability specification.

### 6.5 Recovery outcomes

| Situation | Is recovery possible? | Required material and outcome |
| --- | --- | --- |
| PIN forgotten, original device intact | Yes, **if** the owner has the recovery phrase and a local recovery envelope or backup. | Phrase unwraps DEK; owner creates a new local hardware/PIN envelope. The old PIN is not recovered. |
| Original device lost/destroyed, phrase + encrypted cloud backup available | Yes. | Follow the replacement-device flow; recovery phrase and authenticated backup establish a new local envelope. |
| Original TPM/secure element fails | Yes, **if** phrase plus a local recovery envelope or cloud backup remains. | Recovery creates a replacement local envelope after authenticating the same DEK. |
| Recovery phrase lost, authenticated original device still available | Yes. | Owner deliberately creates and verifies a new phrase and recovery envelope, then retires the old envelope only after success. |
| Original device and recovery phrase both lost | No. | No hidden vendor/cloud/PIN bypass exists. Encrypted data remains inaccessible; Ephemeral Mode remains usable on a new device. |
| Recovery phrase exposed with a copied backup | Potentially compromised. | The phrase plus recovery envelope is intentionally sufficient to recover the DEK; incident re-encryption/migration needs separately designed product scope. |

The phrase alone cannot reconstruct data if every encrypted data copy and
recovery envelope is gone. Conversely, the backup alone cannot decrypt data
without the phrase.

### 6.6 Recovery ceremony containment

The recovery phrase is too sensitive for Independence's ordinary voice and
conversation flows.

The future ceremony must ensure that phrase material is never:

- captured as microphone audio;
- transcribed by Whisper;
- displayed in normal dialogue logging;
- placed in a prompt, model history, telemetry event, error string, or shell
  history; or
- stored as a “helpful” backup in an application setting.

Physical controls or a dedicated direct-input/display surface are the preferred
interaction. The microphone should be off during phrase display and verification
to avoid accidental capture of speaker output or spoken words.

## 7. Envelope, manifest, and rollback semantics

This describes security semantics, not final serialized bytes.

### 7.1 Recovery-envelope and backup fields

| Field | Visibility | Requirement |
| --- | --- | --- |
| Magic, format version, suite, envelope kind | Public but authenticated | Reject unknown/misinterpreted formats and distinguish a recovery envelope from local hardware material. |
| Data-set ID, key ID, recovery-envelope ID | Public but authenticated | Bind the DEK wrapper to exactly one intended encrypted data set and backup relationship. |
| BIP-39 profile/passphrase mode; HKDF label/version/salt | Public but authenticated | Reproduce the standard phrase derivation plus Independence purpose separation; never include phrase/passphrase/seed. |
| AEAD nonce | Public but authenticated | Fresh nonce for this envelope encryption. |
| Ciphertext/tag | Secret/authenticated | Contains DEK plus fixed internal version/identity/header confirmation. |
| Backup outer header | Public, then cross-checked | Carries claimed backup ID/generation/format and identifies `R`, `M`, and payload collection; it is never trusted until the manifest authenticates it. |
| Encrypted manifest | Secret/authenticated under a DEK-derived manifest key | Binds all identities, versions, `R`, and the complete payload set. |
| Encrypted payload chunks | Secret/authenticated under a distinct DEK-derived payload key | Preserve ordered record content and detect missing, substituted, or modified chunks. |

All interpreted public envelope fields are AEAD AAD. This stops an attacker
from relabeling a valid ciphertext as another kind of envelope or silently
moving it to another data set. The manifest adds cross-object consistency that
AAD on one object cannot provide.

### 7.2 Cryptographic operations

| Purpose | Recommended construction | Why |
| --- | --- | --- |
| Local PIN authorization | Argon2id split into TPM-auth and local-wrap components, coupled to a reviewed hardware release policy | It makes a TPM compromise less directly equivalent to a DEK release, but never becomes a cloud/record key. |
| Phrase to seed | Standard BIP-39 derivation | Interoperable mnemonic behavior; no custom phrase algorithm and no Argon2id substitution. |
| Derived purpose keys | HKDF-SHA-256 with versioned info labels | Separates local wrapping, recovery wrapping, store, backup payload, and manifest roles. |
| Small envelope/record encryption | XChaCha20-Poly1305 with fresh 192-bit nonce and authenticated AAD | Gives confidentiality plus modification detection; random nonce handling is practical. |
| Key generation/nonces/salts | OS CSPRNG through a maintained Rust interface | Avoids application-made randomness. |
| In-memory hygiene | `zeroize` and `secrecy`-style controls | Reduces accidental copy/log exposure; does not provide a secure enclave. |

**Verified external fact.** XChaCha20-Poly1305's large 192-bit nonce supports
safe random nonces in practice when generated by a CSPRNG, but nonces must
still never be reused with the same key.
[Libsodium XChaCha20-Poly1305 construction](https://doc.libsodium.org/doc/secret-key_cryptography/aead/chacha20-poly1305/xchacha20-poly1305_construction)

### 7.3 Corruption, replay, deletion, and initial rollback policy

Authenticated encryption and cross-object manifest checks detect corruption,
substitution, and incomplete/mismatched generations. They do **not** prevent a
cloud provider or account attacker from deleting a valid backup, withholding it,
or replaying an older complete valid backup. Those are three different
properties:

| Problem | Initial architecture outcome |
| --- | --- |
| Modification/corruption | Detected by envelope, manifest, and payload authentication before activation. |
| Old valid generation replay | Not cryptographically prevented without trusted monotonic state, a hardware counter, remote witness, or a separate user-verifiable policy. |
| Deletion/denial of access | Not prevented by encryption; availability needs retention and independent copies. |

The practical initial rollback policy is: write complete immutable,
generation-named backups; retain several authenticated generations according to
an owner-approved retention policy; include the generation and parent reference
in the authenticated manifest; and show a restoring user the authenticated
generation information before activation. A device that has previously accepted
a generation stores its last accepted generation locally and warns if presented
with an older valid generation. A brand-new replacement device has no trusted
prior state, so it can verify integrity but cannot prove that the cloud supplied
the newest backup. It must say so rather than silently claiming rollback
prevention.

Every write creates and verifies a complete new generation before making it
current. Interrupted writes/uploads remain incomplete generations. Restore
stages, verifies, and durably prepares the candidate data before an explicit
activation transaction; it never overwrites a valid local store merely because
a file downloaded successfully. Strong rollback prevention is deferred until a
trusted monotonic or independently witnessed state has an approved design.

## 8. Cryptographic-library recommendation

No dependency is added by this document. At a future approved implementation,
the candidate set remains:

| Candidate | Role | Important limitation |
| --- | --- | --- |
| [RustCrypto `argon2`](https://github.com/RustCrypto/password-hashes/tree/master/argon2) | Argon2id local PIN hardening. | Parameters need target-device measurement; it cannot make a low-entropy PIN independently suitable for cloud recovery. |
| [RustCrypto `hkdf`](https://docs.rs/hkdf/latest/hkdf/) and [`sha2`](https://docs.rs/sha2/latest/sha2/) | Standard HKDF-SHA-256 purpose separation. | Not a password KDF and not a storage format. |
| [RustCrypto `chacha20poly1305`](https://github.com/RustCrypto/AEADs/tree/master/chacha20poly1305) | XChaCha20-Poly1305 envelope/record AEAD. | Correct nonce, AAD, parser, and protocol design remain Independence responsibilities. |
| [`zeroize`](https://github.com/RustCrypto/utils/tree/master/zeroize) | Best-effort clearing of owned secret buffers. | Cannot erase arbitrary copies, swap, registers, core dumps, or hostile-OS access. |
| [`secrecy`](https://docs.rs/secrecy/latest/secrecy/) | Avoid accidental secret formatting and help scope sensitive values. | Does not lock memory or provide a hardware boundary. |
| [Rust Bitcoin `bip39`](https://github.com/rust-bitcoin/rust-bip39) | Standard mnemonic validation/generation/seed derivation. | It does not make a recovery UI safe; phrase containment is an application boundary. |
| [`getrandom`](https://docs.rs/getrandom/latest/getrandom/) | OS-backed random bytes. | Must handle failure rather than substitute predictable randomness. |
| [TPM2-TSS](https://github.com/tpm2-software/tpm2-tss) and [`tss-esapi`](https://github.com/parallaxsecond/rust-tss-esapi) | Future TPM2 transport/API candidate. | Native dependencies, hardware variability, policy complexity, and prior security fixes require exact-version review. |

All cryptographic primitives must be used through maintained libraries and
validated against fixed test vectors before any security guarantee is made.
Independence should not write a cipher, mnemonic algorithm, KDF, or ad-hoc
key-wrapping primitive.

## 9. LUKS2 and application encryption

LUKS2 remains valuable defense in depth for the device's storage. It protects
a powered-off filesystem or removed SD/NVMe volume, including data unrelated to
Independence. Application encryption is still required for this product
architecture because it:

- keeps the private/ephemeral distinction under `SecurityManager` control;
- makes portable encrypted backup/recovery possible;
- avoids treating an OS volume unlock as proof of Independence user
  authentication; and
- permits private data to remain encrypted under an application DEK even when
  a broader filesystem is mounted.

`systemd-cryptenroll` can enroll TPM2 or FIDO2 tokens for a LUKS2 volume,
including a TPM2 PIN policy. That is useful Linux infrastructure if a future
device-provisioning decision adopts LUKS2. It is not a substitute for the
local/recovery DEK envelopes in this document.
[systemd-cryptenroll](https://www.freedesktop.org/software/systemd/man/latest/systemd-cryptenroll.html)

No desktop environment is required by either concept. Conversely, Secret
Service/GNOME Keyring/KWallet should not be the root of Independence key
management: they depend on a provider/session ecosystem and do not define this
device/recovery/cloud architecture.

## 10. Threat boundaries and residual risks

| Situation | What this architecture protects | What it does not protect |
| --- | --- | --- |
| Device stolen while powered off | A reviewed device-bound TPM/secure-element envelope can prevent a copied local envelope from becoming an arbitrary-machine PIN oracle; its DA policy can limit attempts against the enrolled hardware. LUKS2 may protect broader storage. | A weak/misprovisioned TPM policy, an unverified lockout configuration, invasive extraction, or observed PIN. This protection is not claimed on the current Pi until actual hardware passes enrollment tests. |
| Storage card/drive copied | A copy lacks `S`, which remains in the enrolled hardware, so public metadata plus `C_local` does not by itself verify PIN guesses. | A copied recovery envelope plus exposed recovery phrase is sufficient by design. If `S` is later extracted, `C_local` becomes an offline PIN verifier; Argon2id only raises its per-guess cost. |
| Cloud backup or cloud account stolen | Cloud has ciphertext and recovery envelope, not PIN/TPM material. Account credentials alone cannot derive DEK. | Phrase theft plus ciphertext compromises recovery; cloud deletion/rollback hurts availability. |
| Offline PIN guessing | The local split-secret design requires the enrolled hardware to release `S`; configured DA behavior limits guesses against that hardware. Argon2id is defense in depth if a hardware secret leaks. | No credible short-PIN protection exists if the hardware contract is absent or misconfigured. PIN observation, malware, or a successful physical TPM extraction can defeat it. |
| TPM dictionary-attack lockout | Lockout stops local authorization attempts; Independence stops retry loops and leaves Ephemeral Mode available. Recovery remains independent of the TPM. | DA state is global to the TPM, so another process can cause denial of service. Lockout reset/recovery and its authorization are TPM-specific provisioning facts. |
| Envelope or backup modification | AEAD/AAD and complete manifest/payload cross-checks detect alteration, role/data-set mismatch, and incomplete generation before activation. | They do not prevent deletion or an older *valid* complete backup replay. |
| Backup replay or deletion | Immutable generation names, retained authenticated generations, parent references, and a last-accepted-generation warning make rollback visible where prior trusted local state exists. | A new replacement device cannot cryptographically prove which valid cloud generation is newest; no encryption design alone prevents provider/account deletion or withholding. |
| TPM fails, device lost, or device replaced | Phrase + valid recovery envelope permits DEK recovery independently of original hardware. | No recovery if phrase and all valid ciphertext/envelopes are lost. |
| OS compromised while unlocked | None of the key wrapping is expected to protect plaintext/decrypted history/entered PIN from malware with relevant access. | This is a major residual risk; a TPM does not make a hostile running OS trustworthy. |
| Application crash/shutdown | Design minimizes and clears app-owned key buffers best-effort. | Cannot prove every allocator, swap, terminal, driver, child process, or crash-dump copy is erased. |
| Physical PIN observation | Nothing cryptographic; private entry UX can reduce exposure. | A watched or keylogged PIN should be treated as compromised. |
| Recovery phrase exposure | Nothing once the attacker also obtains recovery envelope/ciphertext; that is the phrase's purpose. | User must rotate/re-encrypt through a future incident process; no automatic magic revocation exists for copies already made. |

## 11. Open technical questions for later review

These are implementation-critical details, not unresolved product substitutions.

1. Which TPM 2.0 module or secure element satisfies the hardware-protector
   contract on the future Pi hardware, including Linux drivers, provisioning,
   reset behavior, global DA ownership, lockout reset/recovery behavior, and
   verifiable `PolicyAuthValue` retry/lockout semantics?
2. What exact numeric-PIN policy is acceptable for the product (minimum length,
   enrollment rules, and entry UX) once the candidate hardware's DA behavior is
   measured? A short PIN remains conditional on the fail-closed hardware gate.
3. Should the first hardware policy use only PIN authorization, or PIN plus
   an approved PCR/secure-boot policy? How will supported firmware/OS updates
   avoid accidental lockout?
4. What Argon2id parameters are reliable on each supported device class when
   used as local authorization hardening?
5. Should BIP-39 use the recommended empty passphrase initially, or should a
   second recovery secret be accepted with its loss/support risk?
6. What canonical versioned encoding should represent local/recovery envelopes:
   fixed binary, canonical CBOR, or another narrowly specified format?
7. What storage engine and record/chunking format will hold encrypted private
   conversations, photos, and settings?
8. What backup generation, retention, deletion, and user-visible rollback
   policy is acceptable for untrusted cloud storage?
9. What operational guidance or product controls are required for swap,
   hibernation, core dumps, terminal access, software updates, and device
   physical security?

## 12. References

### Existing Independence documents

- [Milestone 6.1 — Security Foundation and Ephemeral Mode](../milestones/MILESTONE_6_1_SECURITY_FOUNDATION_EPHEMERAL_MODE.md)
- [Milestone 6.2 research — Authentication and Key Management](../milestones/MILESTONE_6_2_RESEARCH.md)
- [Security architecture](ARCHITECTURE.md)
- [Threat model](THREAT_MODEL.md)
- [Security review checklist](REVIEW_CHECKLIST.md)

### Primary external references

- [RFC 9106 — Argon2](https://www.rfc-editor.org/rfc/rfc9106.html)
- [RFC 5869 — HKDF](https://www.rfc-editor.org/rfc/rfc5869.html)
- [BIP-39 specification](https://raw.githubusercontent.com/bitcoin/bips/master/bip-0039.mediawiki)
- [TCG TPM 2.0 Library Part 1 — Architecture, published Revision 1.83](https://medias.cdn.trustedcomputinggroup.org/production/TPM_2_0_1_83_Part_1_Architecture_02a468b5a3.pdf)
- [TCG TPM 2.0 Library Part 2 — Structures, published Revision 1.83](https://medias.cdn.trustedcomputinggroup.org/production/TPM_2_0_1_83_Part_2_Structures_8a60a99d40.pdf)
- [TCG TPM 2.0 Library Part 3 — Commands, published Revision 1.83](https://medias.cdn.trustedcomputinggroup.org/production/TPM_2_0_1_83_Part_3_Commands_cabd333ad3.pdf)
- [systemd-cryptenroll](https://www.freedesktop.org/software/systemd/man/latest/systemd-cryptenroll.html)
- [systemd TPM2 PIN implementation](https://github.com/systemd/systemd/blob/main/src/shared/cryptsetup-tpm2.c)
- [tpm2-tools dictionary lockout](https://tpm2-tools.readthedocs.io/en/stable/man/tpm2_dictionarylockout.1/)
- [cryptsetup / LUKS2](https://gitlab.com/cryptsetup/cryptsetup)
- [Libsodium XChaCha20-Poly1305 construction](https://doc.libsodium.org/doc/secret-key_cryptography/aead/chacha20-poly1305/xchacha20-poly1305_construction)
- [Raspberry Pi 5 product brief](https://datasheets.raspberrypi.com/rpi5/raspberry-pi-5-product-brief.pdf)
- [Raspberry Pi security architecture](https://www.raspberrypi.com/documentation/security/security-architecture.html)

## Review conclusion

This architecture preserves the established user experience—Ephemeral Mode
always works; Private Mode authenticates once at startup and remains unlocked
until shutdown—while separating a convenient local PIN from independent cloud
recovery.

The central invariant is:

> A short PIN can authorize one enrolled hardware-protected device, but it
> cannot decrypt a cloud backup. A recovery phrase can recover the same DEK
> without the original device, but it is never an everyday spoken or logged
> credential.

The direct sealed-DEK sketch is superseded by the split-secret local envelope
in Section 5. It is deliberately conditional: no shipping claim about a short
PIN is justified until a particular TPM/secure element demonstrates the
required policy, device binding, and lockout behavior.

No production behavior has been changed. Milestone 6.2 remains **awaiting
owner review and approval**; a later implementation milestone may be scoped
only after that review.
