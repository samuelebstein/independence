# Milestone template

> Status: proposed | approved | in progress | awaiting owner approval | complete
>
> Date / owner / related issues or design documents:

## 1. Objective

State the smallest user-visible capability this milestone delivers.

## 2. Why the capability is needed

Explain the problem in ordinary language, the project value it supports, and
why this milestone is the appropriate next step.

## 3. Scope

List the files, components, interfaces, data, and platforms intentionally in
scope. State which existing behavior must be preserved.

## 4. Explicitly excluded work

Name tempting adjacent work that is *not* authorized. This prevents a focused
milestone from becoming a hidden redesign.

## 5. Expected behavior

Describe successful, cancelled, malformed, unavailable-device, and failure
paths from a person's point of view. Distinguish application-enforced behavior
from model guidance or platform behavior.

## 6. Implementation boundaries

Identify state owners, worker boundaries, cancellation/identity rules, storage
boundaries, relevant libraries, platform adapters, and compatibility concerns.
Do not introduce a speculative abstraction without naming the concrete problem
it solves.

## 7. Security and privacy threat review

- Assets and data touched
- Trust boundaries and external processes/services
- Attacker capabilities and attack paths
- Required protections and what is actually implemented
- Logging/telemetry/media/secret handling
- Residual risks and owner decisions

Link to [the project security review checklist](../security/REVIEW_CHECKLIST.md)
and state whether the review is complete or awaits approval.

## 8. Automated tests

List the production paths exercised with fakes/mocks, the invariants each test
proves, and the hardware/network/model dependencies deliberately avoided.

## 9. Manual verification procedure

Give exact commands, where to run them, what they change, expected successful
output, cleanup, and how to recognize a failure. Clearly label any step that
needs real hardware, a model, network access, elevated permission, or private
data.

## 10. Acceptance criteria

Use observable, testable statements. Include privacy/cancellation/error
criteria where relevant. Do not mark a criterion passed until evidence exists.

## 11. Documentation updates

List README, learning, platform, architecture, threat-model, and operator
documentation that must change with the capability.

## 12. Completion report

Record:

- files changed;
- commands actually run and their results;
- manual tests actually performed versus pending;
- known limits and follow-up work;
- unresolved owner approvals; and
- whether the milestone is complete or awaiting approval.

Stop here. A later milestone requires explicit authorization rather than being
implicitly started from a completion report.
