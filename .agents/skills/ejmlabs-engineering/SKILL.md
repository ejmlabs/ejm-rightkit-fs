---
name: ejmlabs-engineering
description: Use before you design, write, refactor or review code in any ejmlabs repository. Covers the smallest complete change, following or defining a pattern, packages before plumbing, boundaries, errors, bounded resources, scalability and velocity.
---

# Engineering at ejmlabs

Solve the whole problem with the least new code, in the shape the codebase already uses. Uniform
code beats locally clever code.

## The smallest complete change

- Build only what the task needs (YAGNI). Write the simplest code that does the whole job
  (KISS). Give each behavior one home and reuse it (DRY).
- Before you change shared behavior, find every caller, test, fixture, config and doc it reaches,
  and update them in the same change. Keep unrelated edits out.
- Add no option, layer, cache, extension point or abstraction "for later". Add them for a current
  requirement or a measured bottleneck.
- Remove the code, imports, configuration and paths your change makes obsolete. Deletion beats
  addition.
- Projects in development add no legacy or backward-compatibility code unless the user asks.
  Keep compatibility only while a real caller still needs it.

## Follow or define a pattern

- Before you add a file, helper, module or structure, find how the codebase already does the same
  job and copy that shape: naming, layout, error style, test placement.
- When none exists, choose one, record it in the repository's conventions (its contributing
  guide or engineering skill) before you use it, and apply it everywhere it fits.
- Never add a second shape for something the codebase already does one way. Never copy a shared
  helper; extend it, keeping each call site's error text and failure behavior.

## Packages before plumbing

1. Check the platform, the standard library, and the dependencies already installed.
2. If none fits, evaluate established packages: maintenance, license, security record, platform
   support, resource cost, and the exact behavior needed. A familiar name or download count is
   not evidence.
3. Use the package that fits, behind a small adapter that keeps its guarantees.
4. Write custom code only for the product's distinct behavior and thin adapters. "Small",
   "simple" or "fewer dependencies" never justifies rebuilding a parser, protocol, transport,
   crypto, auth flow, process runner, test server or UI interaction. Record the gap, the
   alternatives and the reason next to the code or in a design note.

See [references/dependencies.md](references/dependencies.md) for pins, licenses and exceptions.

## Boundaries and design

- One owner per rule. Organize by capability, not by technical layer. Shared engines own shared
  behavior; hosts and apps keep thin adapters.
- Add a trait or interface only at a real seam: an external boundary or a real second variant.
  No dependency injection container, factory or wrapper for a single implementation.
- Prefer plain functions and data. A class or struct with methods owns state or enforces an
  invariant.
- Use an enum or union instead of a boolean mode flag. Name repeated limits, timeouts and file
  names as constants.
- Keep operations explicit at call sites. An empty value must never secretly mean a different
  action. Keep fallbacks apart from the main path.
- Give each module one responsibility. No `utils`, `helpers`, `common` or `managers` dumping
  grounds; name the module for what it does.
- Comment only the reason or constraint the code cannot show.

## Errors and inputs

- Validate untrusted input at the trust boundary; pass validated types inside.
- Fail clearly on missing configuration or data. Never hide an error behind an empty value, a
  silent default, or a success.
- Catch only where you can recover or add context. Keep the cause. Log a failure once, where it
  is handled, with safe identifiers and no secrets.

## Bounded and scalable

- Bound every queue, buffer, batch, retry, output, context, collection, process lifetime and
  remote response size. Every outbound wait has a finite timeout. Cancellation propagates.
- Retry only transient, safe-to-repeat failures, with a cap and backoff. Never replay an effect
  whose outcome is uncertain.
- Measure before you optimize, and state the workload with any performance claim.
- Detail: [references/scalability.md](references/scalability.md).

## Refactors

A refactor changes structure, not behavior. Keep output, messages, check order, limits,
cancellation, cleanup, wire and storage formats, command names and public paths. When code moves,
the old path re-exports it. A bug found during a refactor is its own change.

## Velocity

- Small, single-purpose PRs that each pass the gate alone.
- Focused checks while editing; the full gate once before handoff.
- Campground rule: leave the area you touch better than you found it, within scope. Record larger
  follow-ups instead of starting an unrelated rewrite.
- Decide reversible choices quickly with a sensible default; ask only when the choice changes
  correctness, security, spending or the requested outcome.
