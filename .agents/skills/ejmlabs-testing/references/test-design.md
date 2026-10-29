# Test design

Read when you choose what a test asserts, which double to use, or which layer a claim needs, and
when you test a failure, cancellation, recovery, retry, race, or duplicate event.

## Behavior over structure

- A test fails when behavior changes and stays green when you only reorganize the code. Assert
  return values, rows written, and calls at the boundary.
- Assert a call only when the call is the contract: an effect the user pays for or sees, or intent
  that must be recorded before an effect. Otherwise assert the result.
- Do not restate the implementation, and do not retest what a dependency already guarantees.
- Compute expected values independently. A test that copies the code's formula proves nothing.
- A private helper test is fine only when the helper holds a rule no public path can reach. Say why
  in a comment.

## Pick the double by what the claim needs

| Double                    | Use when                                                                                 |
| ------------------------- | ---------------------------------------------------------------------------------------- |
| The real thing            | It is cheap and deterministic.                                                           |
| Fake (a working shortcut) | The collaborator is slow or remote, and the claim is about branching, not its semantics. |
| Stub (canned answers)     | The test needs one fixed reply.                                                          |
| Spy or mock               | The call itself is the claim.                                                            |

- Fake at the library's own seam, such as an HTTP mock transport or an in-process app transport.
  Never patch the client class, and never write a wrapper just to mock it.
- A test that patches three internal names to reach one assertion needs a seam instead.
- Inject test dependencies. Production code never branches on a test mode.

## Layers: what a fake cannot prove

- Write many small tests and some integration tests. Put each claim at the lowest layer that can
  make it fail.
- A unique key, a lock, a lease, `ON CONFLICT`, row-level security, a transaction, or an atomic
  reservation cannot fail against a fake that returns whatever its handler says. Test it on the
  real database, with the restricted runtime role where access rules are the claim.
- A real sign-in, a real browser, and a real network path need staging or a live check. Say so in
  the report when you could not run them.
- Size predicts flakiness. Add a larger test only for a claim a small one cannot make.

## Failure and cancellation

- After an expected error, assert the side effect too: nothing written, nothing sent, nothing
  charged. Match the error type or message, so a different error cannot satisfy the test.
- For cancellation, cancel at the exact await (an event the fake waits on, not a sleep), require
  the cancel to propagate, then assert what stays (a kept reservation) and what goes (a held slot,
  a child process, a temporary file).
- A new wait without a cancellation test is unfinished.

## Recovery, retries, and duplicates

- Inject the fault at the step that creates the state: a crash before or after the effect, a lost
  lease, a disconnect. Then count effects: exactly one, or none and a review state.
- A recovery or retry test that ends with "status is done" and no effect count is incomplete.
- Send the same event twice, and send several copies at once. Assert one effect and one winner. Run
  it on the real database, because the unique key or lock is the thing under test.
- Assert an invariant that holds for any interleaving, such as "reserved plus spent equals the
  total" or "no duplicate row". Ask: would the assertion still pass if two operations swapped?

## Limits

- Test at the exact limit and one past it, for every new size, count, time, or retry bound.
- Check that a truncated result says it was truncated.
- A limit that must hold across many sessions gets a test or benchmark at that scale.

## Time and randomness

- Pass the clock in, so a test reaches midnight, a month end, and a period edge with no patch.
- Seed random input and print the seed on failure. For parsers, signers, paging cursors, and
  state machines, state the property (round trip, idempotence, an invariant after every step) and
  let a property-testing tool generate the cases. Pin known regressions as explicit examples.

## Waiting

- Wait on the event, a response, or a state. Poll to a deadline with a distinct timeout error
  only when there is no event.
- The timeout catches a hang. It does not time the code. Make it generous enough for a loaded CI
  runner. A test that times out once and passes on rerun gets a larger limit, not a retry.
- A wait that proves nothing arrives (a revoked session gets no frame) is the one fixed wait with no
  event to replace it. Name the constant and say why beside it.

## Isolation and cleanup

- Give each test its own starting state. Shared rows or singletons make order matter.
- Clean only what the test created. Never truncate shared tables or touch a deployment database.
- Make teardown fail loudly, and keep resource pools small so the leaking test fails, not a later
  one.
- Run a new test alone, then with its whole file. Passing alone and failing in the file means
  shared state.

## Skips, flakes, and quarantine

- A skipped test is not a pass. Report the skip count you saw.
- Gate tests that need a service so they skip locally when it is absent, and fail in CI.
- A flaky test is fixed, or quarantined with a linked issue and removed from quarantine in the
  fixing PR, through the repository's mechanism. Never delete it and never retry it into green.
- Before you blame the code for a timeout on a loaded machine, rerun the one failing test alone.
