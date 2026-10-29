---
name: ejmlabs-writing
description: Use before you write any prose in an ejmlabs repository or reply, including docs, README, CHANGELOG, comments, commit and PR text, UI copy, error messages, help text, site copy, or any claim about speed, security or compatibility.
---

# Writing at ejmlabs

Plain, exact words a reader can act on. Nothing that is not true.

## Orwell's six rules

Apply them to replies, summaries, status text, errors, logs, docs, comments and generated files.

1. Never use a worn-out metaphor, simile or figure of speech.
2. Use a short word where it means the same as a long one.
3. Cut every word that adds no meaning.
4. Use the active voice when it is clearer.
5. Use an everyday word instead of jargon or a foreign phrase.
6. Break any of these rules before you write something unclear or misleading.

## House style

- Never use em dashes. Use a period, comma, colon or parentheses. Keep exact quotes, code and data
  as they are.
- Sentence case for headings, titles, buttons and labels.
- Project names are lowercase in prose: ejmlabs, ejm-cli, ejm-harness, ejm-ide, ejm-ui, gerry.
  Public pages read "cli by ejmlabs", "harness by ejmlabs". Follow the repository's Names table.
- Write for the reader's next action: what happened, what it means for them, what to do.
- Link every repository, PR, issue, document, site and release you mention. Use known or
  retrieved URLs only; never guess one.

## Honest claims

- Never claim performance, security, privacy, compatibility, platform support or availability
  that the repository cannot prove. Cite the evidence and its limits.
- Say "unknown", "estimated", "sample" or "not yet verified" when that is the truth. Missing,
  sample, estimated and measured values are never interchangeable.
- Native execution is not a sandbox. A source release is not a published package. A passing
  check is not a deploy. Write exactly which one happened.

## Docs layout

- README: what it is, quick start, links. Details go in `docs/`, each guide listed from the
  README or `docs/README.md`.
- AGENTS.md holds rules that apply to every change; skills hold task detail; references hold deep
  detail. Link to the owning document instead of copying it.
- Update docs, help text and examples in the same change as the behavior.
- `CHANGELOG.md` has a dated section per release, written for users of that release.

## UI and error text

- Use only the words needed to understand the task, choose safely and recover. Remove repeated
  explanations. Keep accessible names, consent and security facts, and honest state.
- An error says what failed and what to do next, in the user's terms. Never leak secrets, tokens,
  internal paths or stack traces to users.
- Do not add word-count gates or tests that match incidental wording.

## Commits, PRs and reports

- Titles are short imperative sentences in sentence case: "Write gerry in lowercase, like the
  other project names".
- Bodies say why the change exists and what was verified, including what was not.
- Reports to the user lead with the outcome, then the evidence, then the open risks. No filler,
  no restating the request.
