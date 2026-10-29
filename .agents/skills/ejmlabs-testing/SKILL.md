---
name: ejmlabs-testing
description: Use before you add, change, or run tests, fixtures, or replay providers, when a test flakes, when you reproduce a bug, check a visible change in a browser, verify on staging or a live deploy, or report what a check run proved in an ejmlabs repository.
---

# ejmlabs testing

A test earns its place when it fails because behavior someone depends on broke. A check counts
only when it ran in this session, and the report says what it proves and what it does not.

## What to test

- Test behavior, contracts, and regressions: return values, rows written, messages sent, what the
  user sees. Never test private helpers, call order that is not the contract, or a dependency.
- Each bug fix gets one focused regression test named for the behavior. Watch it fail without the
  fix (revert, run, see red, restore). A test you never saw fail proves nothing.
- A new limit gets a test at the exact limit and one past it.
- A new wait gets a cancellation test. A new recovery state gets a crash or disconnect at the
  boundary that creates it. Assert what stays, what is cleaned up, and that no effect ran twice.
- For a failure, match the error type or message and assert that no side effect ran.
- Test the denial: another tenant's object, a missing permission, hostile input.
- Put each claim at the lowest layer that can falsify it. A unique key, lock, transaction, or
  atomic reservation needs the real database. A fake returns whatever you told it.

## Deterministic, bounded, cleaned up

- Never wait a fixed time. Wait on the event or state, with a timeout that only catches a hang.
- Pass the clock in. Seed random input and print the seed on failure.
- Each test owns its state: a temporary directory, a disposable schema or workspace, its own
  accounts. Clean up processes, tasks, files, and rows on every path. A test passes alone and in
  its file.
- For concurrency, assert an invariant that holds for every interleaving, not the order you saw.
- Fix a flaky test or quarantine it with a linked issue, the way the repository does. Never retry
  it into green and never delete it.
- Keep build and test parallelism within the machine's capacity and the repository's job limit.

## No paid calls or live services

- Tests never call a paid model, a live provider, or a live service, and never sign in for real.
- Use the repository's replay provider for model behavior. Use a maintained local fixture for HTTP
  (Wiremock, or the HTTP library's mock transport) and local subprocess fixtures for execution.
  Never handroll a test server or protocol parser.
- Fake at a library's own seam, such as its transport. Do not patch types you do not own.
- Remove provider keys from child process environments. Use placeholders and synthetic data only.

## The gate

- Each repository has one gate command, named in its AGENTS.md. Local work, the pre-commit hook,
  and CI run that same command. A repository that still splits its gate lists every command.
- Run focused tests while you edit. Run the full gate before handoff and after any later change.
- A missing tool, stale lock, or unavailable service is a failed gate, not a skip.
- Never weaken a test, lint, or threshold to pass, and never commit with `--no-verify`.
- A docs-only change needs its formatting and link checks. Do not claim app behavior from them.

## What a run proves

- Report each command, where it ran (OS, local or CI), and its result.
- A skipped, failed, interrupted, or unavailable step is unverified, never green. State the skip
  count. In CI, a missing database or service fails the run instead of skipping it.
- A local run covers one platform. A fixture that parses another platform's data does not prove
  that platform's behavior. Name the platforms each claim covers.
- A fake cannot prove a real database, sign-in, browser, or network path. Local green does not
  prove a release, a deploy, or production readiness.

## Bugs and visible changes

- Reproduce a reported bug before you fix it. Capture the visible symptom (the screen, the reply,
  the error page, the response), not only a failing test.
- Show the user the same flow before and after, as screenshots or a recording. Say whether it came
  from staging or local data.
- Check every visible change in a browser in light and dark themes at the widths that matter, and
  read the console. Keep screenshots and scripts out of the repository.

## Staging and live checks

- Where a repository has staging, staging is the true test. Once the change reaches staging, run
  the real flow there: sign in, act, read it back, revoke.
- A staging failure blocks done, even when local tests pass. If staging cannot show the behavior,
  say why.
- Use test accounts and synthetic data. Never read or print staging secrets.
- A repository may reserve live testing for the user. Then use their reported flow and the logs.
- After a deploy, check the live result: status, headers, the page in a browser, the console.

## Read next

- [references/test-design.md](references/test-design.md): choosing doubles and layers, failure,
  cancellation, recovery, races, time, flakes, and skips.
- [references/browser-checks.md](references/browser-checks.md): driving the browser, themes,
  widths, screenshots, and Playwright tests.
