# Independence

## What it is

Independence is a local-first personal computer for exploring the physical
world, learning, and becoming more capable. Its intended experience is not a
productivity chatbot that tries to complete everything for someone. It is a
patient, curious companion that can help a person notice something, understand
it, and keep going on their own.

The long-term experience may include a person pointing a camera at a tree,
asking a question naturally, and receiving a careful explanation that separates
what is visible from what is uncertain. That is a direction, not a claim about
the current prototype.

Today, this repository contains a deliberately narrow Rust terminal prototype.
It can accept typed or explicit hold-to-talk input, optionally attach an
explicit still photograph, use a configured local model, and present an answer
in the terminal or through optional local speech output. See
[ARCHITECTURE.md](ARCHITECTURE.md) for the code-grounded description.

## Purpose

Independence exists to make technology feel like a tool for attention,
understanding, and agency. It should help a person:

- observe before jumping to conclusions;
- ask a real question in ordinary language;
- understand an explanation rather than merely receive an answer;
- keep ownership of personal material and the machines that process it; and
- learn enough to make an informed next choice without becoming dependent on a
  hidden system.

## Values and design principles

### Local first, not local as marketing

The default prototype targets local processes and loopback-only Ollama HTTP.
It does not silently fall back to a cloud model. Local-first means that a
person should be able to understand where their material goes, choose when a
networked system is involved, and keep using useful capabilities without an
account or remote service whenever practical.

Local processing is not by itself a complete security guarantee. A local model
is still another process on the same computer; a compromised operating system,
terminal capture, removable storage, or an explicitly configured telemetry
exporter can still expose data. Independence documents those limits rather
than hiding them.

### Privacy and user control

Microphones, cameras, transcripts, photographs, conversation history, and
future personal memories are sensitive. The system should collect the minimum
needed for an explicit interaction, retain it only as long as the person
understands, and make device use visible. A future security layer must protect
private data while the application is locked and must not quietly route it to
outside services.

### Capabilities with clear boundaries

The project is organized around practical capabilities—listen, see, think, and
speak—rather than a generic “agent” framework. A small controller owns the
accepted session state and authorizes output. Hardware adapters and model
workers return results; they do not independently mutate conversation history
or decide what the application should do next.

### A teacher, not a persuasive authority

Model guidance asks for concrete, warm, brief answers that distinguish direct
observations from interpretation and uncertainty. This is guidance to a model,
not a proof that a model is correct. The application should never use a
confident tone to hide uncertainty, invent references, or turn a photograph
into a medical, safety-critical, or surveillance decision.

## What Independence should not become

- A cloud-first service that quietly trades personal context for convenience.
- A system that activates sensors, captures media, or makes purchases and
  external changes without clear user authorization.
- A hidden general-purpose agent framework, plugin marketplace, or collection
  of microservices before there is a concrete need.
- A replacement for a person's judgment, relationships, professional advice,
  or capacity to learn.
- A system that claims protection against threats it has not actually
  implemented and tested.

## How to read the project

Start with [VISION.md](../VISION.md) for the personal motivation, then
[ARCHITECTURE.md](ARCHITECTURE.md) for what exists now. The security design is
separate because it is not implemented yet:

- [Security threat model](security/THREAT_MODEL.md)
- [Proposed security architecture](security/ARCHITECTURE.md)
- [Milestone 5.5 decision record](milestones/MILESTONE_5_5_SECURITY_ARCHITECTURE.md)

Those documents intentionally distinguish agreed direction from recommendations
that still require the project owner's approval.
