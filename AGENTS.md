# Independence project instructions

These instructions apply to all work in this repository. They are deliberately
short enough to use while working, but they are not optional: Independence is a
local-first personal system, so a small convenience change can have a large
privacy consequence.

## Project principles

- Independence is a local-first, privacy-preserving personal AI intended to
  help people explore the physical world and become more capable.
- Privacy and user control are foundational requirements, not optional polish.
  Minimize collection, retention, copying, and exposure of private information.
- Prefer offline operation. Never silently add a cloud dependency, external
  data transmission, account requirement, or remote fallback.
- Be honest about what the software protects, what it merely requests of a
  model, and what remains outside the application's control.
- Preserve the distinction between current implementation, approved design,
  experimental work, and future ideas in code, documentation, logs, and
  handoffs.

## Engineering principles

- Inspect the current repository, applicable instructions, Git status, and
  relevant tests before proposing a change. Preserve user-owned uncommitted
  work and unrelated files.
- Prefer established, maintained libraries over reimplementing HTTP, audio,
  camera, cryptographic, concurrency, or serialization infrastructure.
- Keep shared application logic separate from platform-specific hardware
  adapters. Preserve the existing controller/event/state ownership unless an
  approved milestone explicitly changes it.
- Make small, focused, reviewable changes. Avoid unrelated refactors,
  speculative frameworks, and abstraction for hypothetical integrations.
- Explain an abstraction's problem, library-owned behavior, and
  Independence-owned behavior in accessible language before introducing it.

## Milestone workflow

- Work only from an approved milestone specification. Complete only the
  currently authorized milestone, then stop for review.
- Before substantial code changes, present a short implementation plan grounded
  in the existing code. State the relevant files, state ownership, and any
  material trade-offs.
- Add automated tests and a proportionate manual verification procedure for
  every behavior change. Never claim a command, device test, model test, or
  security property was verified unless it actually was.
- Report what changed, what was tested, what was not tested, and one useful
  experiment after each milestone.
- Update user-facing and learning documentation whenever implementation changes.
  Keep the README's "Teach Sam About the Latest Milestone" section current.
- Do not silently make significant architectural or security decisions. Put
  unresolved choices in an explicit owner-approval section.

## Security and privacy requirements

- Every milestone involving private data, authentication, storage, audio,
  vision, networking, telemetry, logs, or device access requires a security and
  privacy review.
- Never expose recovery phrases, master keys, PINs, derived keys, credentials,
  or secrets through logs, telemetry, recordings, inference prompts, model
  history, error strings, tests, shell history, or external services.
- Never implement custom cryptographic algorithms. Use established standards
  and maintained libraries, version formats and metadata, and obtain review for
  key-management or storage-format changes.
- Never claim an encryption, authentication, deletion, secure-memory, or
  privacy guarantee unless it is implemented and verified in the current code.
- Treat camera, microphone, transcript, photograph, conversation, and model
  requests as sensitive data. Explicit user action and visible state are the
  default for device use.
- Local software is not automatically a trusted boundary: account for the
  operating system, processes, dependencies, removable storage, backups, and
  optionally configured telemetry/export paths in reviews.
