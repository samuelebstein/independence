# Security and privacy review checklist

Use this checklist before approving any milestone that touches private data,
authentication, storage, audio, vision, device access, networking, telemetry,
or logging. A checked item needs evidence: code path, test name, manual result,
or an explicit reason it is not applicable. “Local” is not a substitute for a
review.

> Do not put a recovery phrase, PIN, master key, derived key, recording,
> photograph, private transcript, or real secret in this checklist, a test
> fixture, a bug report, or a milestone report.

## 1. Scope and data map

- [ ] The objective states exactly which data is newly collected, processed,
  stored, exported, or deleted.
- [ ] The document distinguishes current implementation from proposal and model
  guidance from application-enforced behavior.
- [ ] Each data item has an owner, lifetime, size/retention bound, and cleanup
  path.
- [ ] Input, model, camera, microphone, speaker, child process, filesystem,
  telemetry, backup, and network boundaries are named.
- [ ] The change preserves existing accepted-history, cancellation, and
  stale-result safeguards unless an approved design changes them.
- [ ] New configuration/environment variables are documented with precedence,
  privacy impact, safe defaults, and validation behavior.

## 2. Threat model

- [ ] Assets, trust boundaries, attacker capabilities, and goals are updated in
  the threat model.
- [ ] Powered-off theft, removable/offline storage, malicious local software,
  compromised OS, accidental logging, and backup theft are considered.
- [ ] Physical observation, microphone/speaker echo, camera indicators, and
  peripheral/input spoofing are considered when relevant.
- [ ] Required protections, existing protections, and residual risks are stated
  separately.
- [ ] No claim extends beyond the assumed attacker model.
- [ ] Material trade-offs and unresolved choices are presented for owner
  approval rather than silently encoded in defaults.

## 3. Secrets and authentication

- [ ] Secrets use narrowly scoped types; they have no casual Debug, Display,
  Serialize, clone, error, model-prompt, telemetry, or terminal-log path.
- [ ] PIN/recovery input is not sent to Whisper, Ollama, Piper, camera, normal
  conversation history, ordinary logs, telemetry, clipboard, or shell history.
- [ ] Authentication failures are generic enough not to reveal which secret or
  envelope field was correct.
- [ ] PIN entropy, offline guessing, KDF calibration, online retries, cooldown,
  and availability trade-offs are documented.
- [ ] Biometrics/hardware authenticators authorize protected material; they are
  not silently used as raw encryption keys.
- [ ] Lock, cancellation, shutdown, sleep, wake, and error paths revoke
  authorization and clear secret-containing application state best-effort.
- [ ] A hostile or unlocked operating system is explicitly listed as a residual
  risk.

## 4. Cryptography and serialized formats

- [ ] The design uses established primitives and maintained libraries; no custom
  cryptographic algorithm is introduced.
- [ ] The exact algorithm suite, library/version policy, key sizes, nonce
  strategy, salts, KDF parameters, AAD fields, and format version are specified.
- [ ] Randomness comes from an OS CSPRNG through a maintained library.
- [ ] Every encryption has authenticated integrity; metadata that changes meaning
  is bound as associated data.
- [ ] Parsers reject unknown versions, oversized parameters, malformed lengths,
  and unsafe algorithm choices before expensive work.
- [ ] Nonce uniqueness/freshness is enforced by one accountable boundary.
- [ ] Envelope/data formats have fixed test vectors, negative/tamper tests,
  migration tests, and atomic-write/crash tests.
- [ ] Changing a credential is tested to re-wrap only the intended envelope and
  not silently destroy data.
- [ ] Secret-memory claims are accurately limited; zeroization/locking does not
  get described as a secure enclave.

## 5. Storage and backup

- [ ] Private data cannot be loaded by the controller while Locked.
- [ ] Plaintext, ciphertext, metadata, indexes, filenames, timestamps, and
  sizes have been considered separately for leakage.
- [ ] Backup/export contains no plaintext DEK, PIN, recovery phrase, or derived
  wrapping key.
- [ ] Restore is staged and authenticated before it changes existing data.
- [ ] Restore is nondestructive by default; replacement/merge is explicit.
- [ ] Corruption, deletion, stale rollback, partial write, missing envelope, and
  format migration behavior are specified and tested.
- [ ] A recovery phrase is documented honestly: phrase plus an available
  encrypted envelope can recover; phrase alone cannot reconstruct lost data.

## 6. Audio, vision, model, and hardware privacy

- [ ] Capture requires explicit user action and a visible state/indicator.
- [ ] Audio/image buffers are bounded; their retention and cleanup are tested.
- [ ] No recording/photo payload is put in diagnostic logs, telemetry, test
  snapshots, or source control.
- [ ] A spoken phrase or transcript cannot become a terminal command or gain
  new device authority without an explicit, reviewed adapter.
- [ ] Speaker echo and physical observation are documented; do not claim echo
  cancellation unless it exists and was tested.
- [ ] Camera/microphone release happens before unrelated long work when possible;
  cancellation and device-disconnect behavior are tested.
- [ ] Hardware device names/paths/permissions are treated as potentially
  sensitive configuration where appropriate.

## 7. Networking, logging, telemetry, and dependencies

- [ ] Every outbound path is explicit; no cloud fallback or exporter is enabled
  by default without user configuration.
- [ ] Local loopback, SSH tunnels, and separate local processes are described
  honestly as separate trust boundaries.
- [ ] Logs have a retention/location policy and do not contain secrets or raw
  media. Terminal scrollback/redirection is considered.
- [ ] Telemetry fields are bounded, redacted, and tested; failed redaction is
  not treated as an encryption boundary.
- [ ] Dependencies, model artifacts, native tools, and build steps are reviewed
  for provenance, updates, advisories, licenses, and platform support.
- [ ] Cargo.lock/release inputs are reviewed; the milestone says what supply-chain
  verification actually occurred and what remains future work.

## 8. Verification and handoff

- [ ] Tests exercise the production boundary with fakes; they do not require a
  private real recording/photo/secret or live cloud service.
- [ ] Tests include success, cancellation, stale completion, malformed input,
  tampering, missing devices/files, and failure cleanup as relevant.
- [ ] Manual verification commands say what they change, where they run, and
  what successful and failure outcomes look like.
- [ ] Hardware/model/manual tests are reported separately from mocks.
- [ ] Documentation, data lifecycle, threat model, latest-milestone map, and
  operator instructions have been updated.
- [ ] The completion report lists unverified behavior and owner approvals
  honestly, then stops rather than beginning the next milestone.

