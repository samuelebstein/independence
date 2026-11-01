# Milestone 6.1 — Security Foundation and Ephemeral Mode

**Status:** Implemented in the current working tree; automated verification is
complete. Physical Pi-button and live model/device checks remain manual checks,
not claims made by the automated suite.

## Objective

Make a useful unauthenticated Independence session explicit and safe by
default, while adding the state boundary that later authentication and encrypted
storage will use.

The central rule is simple: being unable to authenticate must not make the
device unusable. It means the device runs an **Ephemeral Mode** session instead
of accessing personal data.

## What this milestone implements

- `src/security.rs` owns a process-local lifecycle with these states:
  `Uninitialized`, `SetupInProgress`, `Locked`, `Unlocked`, and
  `RecoveryRequired`.
- It separately names `OperatingMode::Ephemeral` and `OperatingMode::Private`.
  A mode is an experience/session policy; it is not evidence that a device is
  authenticated.
- Every normal typed, dictation, and voice launch obtains an opaque Ephemeral
  Mode lease from `SecurityManager`. The production application has no way to
  create an `Unlocked` state or a private-storage capability.
- `cargo run` and `cargo run -- --voice ...` default to Ephemeral Mode.
  `--mode private` is accepted as an explicit request but fails before model,
  microphone, camera, terminal-input, or Piper initialization because no real
  authenticator exists yet.
- Ephemeral `Controller` sessions begin empty, retain only in-memory accepted
  conversation and freshly captured camera photos for the live process, and
  are cleared during orderly runtime shutdown.
- `/look` is unavailable in Ephemeral Mode. Reading an arbitrary local image
  could expose a saved personal photo or file. Explicit `/camera` still works:
  it captures one fresh temporary still only after the existing user action and
  model-capability checks.
- Optional OTLP export filters terminal dialogue events. JSONL telemetry keeps
  its existing redaction path. Terminal dialogue remains intentionally visible
  to the person using the terminal.

## Explicitly excluded work

This milestone does **not** create a PIN, a recovery phrase, a master key, key
derivation, cryptography, encrypted storage, a database, persistent lifecycle
state, backups, biometrics, or a login/unlock user interface.

`SetupInProgress`, `Locked`, `Unlocked`, and `RecoveryRequired` are tested
policy states. Current production startup always begins `Uninitialized`
because no protected on-disk state exists yet. Persisting a pretend lock state
before there is real authentication or protected storage would mislead users.

## Lifecycle and modes

```text
security lifecycle (owned by SecurityManager)       active mode

Uninitialized  -- future verified setup --> Locked  Ephemeral: allowed
      |                                              in Uninitialized, Locked,
      +-- current production launch --------------> or RecoveryRequired
                     Ephemeral lease                (does not change lifecycle)

Locked -- future authentication --> Unlocked        Private: requires Unlocked
Unlocked -- lock --> Locked                         (not attainable in production
                                                     in Milestone 6.1)
RecoveryRequired -- future recovery --> Locked
```

`SecurityManager` owns these decisions. `Controller` cannot decide that
private data is accessible. The future `PrivateStorageAccess` capability is
opaque and no production code can construct a `PrivateSession` or access it.

## Physical entry on Raspberry Pi button mode

For `--voice --input buttons`, Independence first opens only the selected,
exclusive evdev button source. Hold its existing Talk/Enter control for five
seconds and release it:

```text
selected evdev keyboard -> TalkPressed -> five-second hold -> TalkReleased
                                                        |
                                                        v
                                             enter Ephemeral Mode
                                             reopen normal button runtime
                                             (fresh Talk press starts recording)
```

This entry gate does not open the microphone, camera, Ollama client, Whisper,
Piper, or terminal input. It gives terminal/SSH-visible confirmation. The
original release is consumed, so a fresh press is needed to record. Escape,
short holds, input loss, and source failure never enter a session.

Terminal mode enters the explicit default Ephemeral Mode directly; it has no
physical button to gate. Button mode continues to avoid a stdin/Crossterm
reader, so keys typed in an SSH terminal cannot control the attached-device
interaction.

## Data lifetime and limits

| Data | Ephemeral Mode behavior | Important limitation |
| --- | --- | --- |
| Conversation context | Starts empty; kept only in the controller while the process runs; cleared on shutdown or `/reset` | Rust/OS allocation release is not a guaranteed secure erase |
| Camera stills | Fresh explicit captures only; in memory; cleared with session | Camera driver, model process, and OS may have independent buffers |
| Microphone samples | Existing bounded capture buffers; no application recording file | Drivers, swap, crash dumps, or another process are outside this guarantee |
| Transcripts and answers | Displayed intentionally in terminal; not placed in app JSONL or OTLP telemetry | Terminal scrollback, screen observation, shell redirection, or system logging can retain visible text |
| Ollama context | Each non-streaming request carries explicit accepted history; a fresh cleared controller sends no old history | The app cannot prove what an external/local model runtime retains internally |

There is no saved conversation, preference, photo, microphone recording, or
private store for Ephemeral Mode to load. The app does not claim to securely
erase every copy an operating system, allocator, model process, terminal, swap
file, or crash dump might retain.

## Automated verification

The ordinary Rust suite uses fake input, microphone, transcriber, camera,
speech, and Ollama components. It verifies:

- lifecycle transition policy, invalid transitions, recovery denial, and
  test-only lock revocation;
- default empty Ephemeral sessions, no `/look` file access, temporary context
  cleanup, and a fresh post-cleanup request with no history/photos;
- typed/controller, voice, camera, cancellation, stale-result, and speech
  regressions using the existing mocks;
- evdev key mapping/repeat suppression plus synthetic button routing;
- telemetry redaction and the terminal-only dialogue exporter filter.

No automated test opens a real microphone/camera, runs Piper/Whisper, sends a
request to a live Ollama server, or reads a real Pi button device.

## Manual verification

Run commands from the repository root.

### A. Default first startup and private-mode denial

```bash
cargo run
```

Observe `security_lifecycle=uninitialized` followed by an Ephemeral Mode
message. Ask a typed question and a follow-up. Then exit with `/quit`.

```bash
cargo run -- --mode private
```

Expect a clear refusal explaining that authentication does not exist yet. It
must not start a microphone, camera, Piper worker, terminal reader, or Ollama
request.

### B. Ephemeral session isolation

1. In `cargo run`, ask a question and a follow-up.
2. Exit with `/quit` (or use `/reset` to clear the current temporary session).
3. Start `cargo run` again and ask whether it remembers the earlier exchange.

The second process sends no previous application conversation history. A model
may still make a generic claim, so verify the application behavior through the
empty context rather than treating model wording as proof.

### C. Explicit camera behavior

In a vision-capable local-model configuration:

1. Run `cargo run`.
2. Use `/camera | What can you see?` and follow the existing camera selection
   behavior.
3. Ask a follow-up, then `/quit`.

The captured still is only the current in-memory session context. `/look` must
instead report that Ephemeral Mode does not load local files.

### D. Voice and optional speech

Use the existing configured voice command, for example:

```bash
cargo run -- --voice
```

Hold Enter through the listening cue, speak, then release. Verify the accepted
transcript, answer, and optional Piper playback. Exit with Ctrl+C or Escape as
appropriate. This preserves existing voice behavior; it does not make the
session authenticated.

### E. Pi physical entry (hardware check)

Launch the existing button profile over SSH with your explicit device IDs.
Before any normal Talk press, hold the attached keyboard's Enter key for five
seconds and release it. Expect a visible Ephemeral Mode confirmation; press
Enter again for the ordinary listening cue and recording. Short holds and
Escape must not start microphone/camera/model work. This check is pending until
performed on the actual hardware.

### F. Logging and persistence review

If JSONL is enabled, inspect it with the existing documented `jq` commands.
It should contain IDs, timings, and outcomes but not the question, transcript,
answer, audio, or image bytes. Inspect terminal scrollback separately: it can
contain displayed dialogue by design and is a residual exposure.

## Acceptance status and next milestone

The non-cryptographic M6.1 foundation is implemented and testable. The
security lifecycle is deliberately not persistent yet, and Private Mode remains
unavailable rather than faking authentication.

**Do not begin Milestone 6.2 automatically.** It requires explicit owner
approval of the key/recovery format decisions recorded in the security
architecture documents.
