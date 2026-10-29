# Rust tests and performance measurement

Read this before writing Rust tests or fixtures, or before measuring or claiming performance.
`ejmlabs-testing` holds the cross-language rules; this file holds the Rust forms.

## Where tests live

| Kind                           | Place                                                                 |
| ------------------------------ | --------------------------------------------------------------------- |
| Unit tests                     | `#[cfg(test)] mod tests` at the bottom of the file under test         |
| Large tests of a folder module | `<module>/tests.rs`, declared from the module root                    |
| Shared fixtures                | `<module>/test_support.rs`, declared `#[cfg(test)] mod test_support;` |
| Public API and binary tests    | `crates/<crate>/tests/*.rs`                                           |

- No catch-all test files. A fixture used by one file stays in that file's `tests` module.
- Existing test files keep their paths; docs and decision records cite them.

## What to write

- Write the smallest test that fails when the changed behavior breaks. Prove it fails without
  the change when you can. Never write a test that restates the implementation.
- Assert what a user or caller observes: final files and journal state, the request a provider
  received, the exit status and output of the real binary. Do not assert one exact internal call
  sequence.
- Test the boundary that creates a state:
  - a new limit at the exact limit and one past it;
  - a new wait with a cancellation test and a dropped-caller test;
  - a new recovery state with a crash or disconnect at that boundary;
  - failure and cleanup paths, not only success.
- Unicode text: test byte boundaries separately from display width.

## Fixtures, not services

- No network, no paid model calls, no live accounts. Use replay providers, Wiremock for HTTP and
  SSE, and local subprocess fixtures for execution.
- Never write your own HTTP server or protocol parser for a test.
- Binary tests run what Cargo built, `Command::new(env!("CARGO_BIN_EXE_<name>"))`, in offline or
  replay mode with a fresh `tempfile` state directory. Remove provider credentials and
  `*_API_KEY` variables from the child environment.
- A test that needs an external tool (a language server, a copied sample workspace) is
  `#[ignore]`d, and the subsystem doc gives the exact command that runs it.

## Determinism

- Every wait in a test has a timeout: `tokio::time::timeout(LIMIT, future)`.
- Test timers with `#[tokio::test(start_paused = true)]`, not real sleeps.
- Clean up temp directories, tasks and child processes, including on assertion failure
  (guards and `kill_on_drop`).
- A test that needs real parallelism asks for it with a fixed size:
  `#[tokio::test(flavor = "multi_thread", worker_threads = 2)]`.
- Performance tests check a limit or a regression with a stated tolerance, never an unexplained
  wall-clock cutoff.

## What a run proves

- Name the commands that ran and the platform. A local run covers one operating system. A
  parser fixture that runs everywhere does not prove another platform's runtime behavior.
- A skipped, unavailable or failed step is unverified. Never describe a partial run as the gate.

## Measuring performance

- Start from a defined workload. Record the command, compiler version and build profile,
  operating system, hardware, limits and workload with every result.
- Measure release builds built with `--locked`. Keep the raw report, including binary SHA-256
  hashes and every sample, beside the summary.
- Use deterministic replay or local fixtures. Never spend provider credits on a benchmark without
  explicit authorization. Remote inference time is reported separately.
- Separate cold startup, time to first event or paint, turn latency, cancellation latency, idle
  memory, active memory and tool execution. Report p50, p95 and maximum where queues, stalls or
  cancellation are involved, and peak resident memory of the whole process tree.
- Managed sessions are not concurrent work. A hundred parked sessions do not prove a hundred
  builds or streams at once.
- A sampler is a sample, not a proof of a peak. Invariants such as maximum concurrency are
  proven by tests, not by sampling.
- Compare before and after on the same host, compiler, profile, workload and limits. Report
  variance. One run cannot attribute a difference to a change.
- Add a workload to the repository's existing benchmark crate or script. Do not add a second
  performance framework.

## Improving performance

- Change the measured bottleneck, then rerun the same workload.
- Prefer less work, fewer allocations, bounded concurrency and shared resources before threads,
  caches or native bindings.
- Never gain speed by dropping required persistence, effect boundaries, bounds or cleanup.
- Report measurements as measurements and targets as targets. One developer machine does not
  establish a target machine's capacity.
