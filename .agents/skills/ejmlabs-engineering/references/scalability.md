# Scalability, performance and cost

Read when you add a loop, queue, cache, fan-out, pool, background job, per-process state, or a
performance claim. A repository skill may hold stricter numbers for its own workload.

## Bound everything

- Every queue, channel, buffer, batch, list response, scrollback, log, context window and output
  stream has a named limit. Slow consumers must not grow memory without limit; apply
  backpressure or drop with a visible count, never silently.
- Every outbound call and every wait has a finite timeout. Every task has an owner that cancels
  and awaits it on shutdown. Every child process has a lifetime and is reaped.
- A limit is tested at the exact boundary and one past it.
- A truncated result says it is truncated. Never present a partial list or total as complete.

## Concurrency

- Share one governor or semaphore per resource (commands, model calls, pool connections, child
  agents) instead of separate local caps that multiply. Report model, agent and tool concurrency
  separately.
- Never hold a pooled connection or lock while waiting on another. Size semaphores against the
  pool they protect.
- No global lock or full-table count on a hot path. Prefer running totals, per-tenant claims
  (`SKIP LOCKED`) and atomic updates.
- Fan-out to many recipients is a bounded, paged, durable job, never a loop inside the request
  or a silent cap.

## State across replicas and restarts

- Assume more than one process. Local memory is never the authority for sessions, limits, work
  ownership or tenant data. List any per-process state and make it shared before it matters.
- Durable work uses claims or leases with fencing, idempotency keys, and explicit states for
  uncertain effects. After a crash, reconcile; never blindly replay.
- Reconnects fetch only what changed, with jittered backoff. Publish real transitions, not
  heartbeats.

## Caches

- Add a cache only for a measured cost. Give it a bound, an eviction policy (LRU, never
  clear-all), a freshness rule, and an owner. Never cache across an access check.

## Measure

- Measure before and after on a stated workload: hardware, seed or fixture, concurrency, and
  what was excluded. Software GL, a dev build, or a single local run is evidence of that only.
- Keep repeatable benchmarks and budgets in the repository (bundle size, startup time, frame
  cost, resident memory, latency percentiles) and check them in the gate where they are stable.
- Claims about speed, memory, scale or platforms cite the evidence and its limits. Unknown is
  reported as unknown.

## Cost

- Optimize total cost at the required quality: infrastructure, paid model and provider usage,
  retries, storage and transfer, and the engineering time to run it.
- Add a service, region, replica, partition or provider only for an observed bottleneck or a
  concrete isolation requirement. Record the evidence, the running cost and how to remove it.
- Background loops and pollers cost money while idle. Prefer events, and give every poller a
  bounded interval and a stop condition.
- Cost tuning never relaxes security, audit, quality gates or spending limits.
