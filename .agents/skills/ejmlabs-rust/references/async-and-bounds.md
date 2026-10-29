# Async, cancellation and bounded resources

Read this before adding or changing a task, channel, wait, child process, shutdown path or limit.
The repo's own skill may name its shared helpers; reuse them instead of writing another copy.

## Cancellation

- Pass a `tokio_util::sync::CancellationToken` into long work. Race it with `biased` so
  cancellation wins a tie and a ready future cannot starve it:

```rust
async fn cancellable<T>(cancel: &CancellationToken, work: impl Future<Output = T>) -> Result<T> {
    tokio::select! {
        biased;
        () = cancel.cancelled() => bail!("cancelled"),
        value = work => Ok(value),
    }
}
```

- Cancel on scope exit with `let _cancel_on_drop = token.clone().drop_guard();`, not a
  hand-written `Drop` type.
- Dropping a caller cancels the work it started. A spawned task holds a child token or lives in
  a `JoinSet` its owner drops; no task outlives the thing that asked for it.
- Cancellation covers every await on the path: model or network streams, tool calls, queue
  waits, retries, child processes and shutdown.
- Some awaits cannot be cancelled. Tokio's stdin read is one: shutdown must not wait for it.
  Finish cleanup, then shut the runtime down without joining an idle stdin reader.
- Put a deadline on every wait for something outside the process (`tokio::time::timeout`).

## Ownership and cleanup

- Model admitted resources as RAII guards (leases). Accounting stays correct on success,
  rejection, cancellation, a dropped future and a spawn failure.
- Release a lease only after the resource it owns has finished cleanup. Root `wait()` on a child
  does not prove its descendants are gone.
- Shutdown order: cancel, wait for idle with a deadline, then drop the runtime.
- Spawn child processes with `kill_on_drop(true)` and in their own process group when they may
  spawn descendants. Kill the group through a safe wrapper (`rustix`), not raw `libc`.
- Drain stdout and stderr together, so a full pipe cannot deadlock the child. Keep bounded
  tails. Exhausting an output quota still drains the pipes.
- Private input (secrets on stdin) stays out of serialized requests, logs and artifacts.

## Runtime and blocking work

- A binary builds its runtime explicitly with a small, fixed size, for example
  `worker_threads(2)` and `max_blocking_threads(4)`. The default sizes to the core count.
- Blocking filesystem, process or CPU work goes through `spawn_blocking`, or the repo's shared
  blocking helper, and passes the join error through.
- Do not create a background monitor, poller or long-lived shell per agent or per session.
  Share one, start it lazily, and stop it with its owner.

## Channels and events

- Use bounded channels: `mpsc::channel(n)` with `n` a named constant or a configured limit.
  Never `unbounded_channel`.
- Decide what a full channel does: wait with cancellation, reject with a clear error, or drop
  and report the drop. Silent loss is a bug.
- Live broadcasts are bounded hints. A slow consumer that lags recovers from the durable source
  (replay, a journal, a re-read), never from an unbounded buffer.
- A slow consumer must not grow memory: rendering, scrollback, history and callback state stay
  bounded while output keeps arriving.

## Shared admission

- One process has one admission point (a governor, semaphore or pool) shared by every session,
  tool, command and child agent. Per-agent limits multiply aggregate load.
- Keep admission fair: a small request must not bypass the oldest waiter indefinitely.
- Estimates for admission follow measurement. Do not lower an estimate to raise a throughput
  number.

## Limits

- Bound queues, buffers, reads, output, history, context, retries, steps, artifacts, archive
  entries and process lifetimes. Name each limit as a constant or a configuration field.
- Cap bytes before you allocate or parse them: bounded HTTP bodies, `read` with a maximum,
  `Read::take`, limits on decoded events as well as on each field.
- Cumulative limits matter as much as per-item limits: total response bytes, total decoded
  events, total artifacts across every executor.
- A truncated or partial result says so in the text its reader sees, with the reason and a way
  to narrow the request. The notice counts against the same bound.
- Size every limit so it still holds with a hundred concurrent sessions. Raising a limit needs
  that arithmetic in the change description.
- Test each new limit at the exact boundary and one past it.

## Effects and recovery

- Persist an effect boundary before executing an effect that cannot be repeated safely.
- After a crash, an effect whose outcome is uncertain is reported as uncertain. Never replay it.
- A confirmed result stays confirmed. Recovery must not depend on data that a bounded view (a
  context page, a trimmed history) may have dropped.
