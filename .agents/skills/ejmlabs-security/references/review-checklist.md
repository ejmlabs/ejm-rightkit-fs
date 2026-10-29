# Security review checklist

Answer each question yes or no for the change in front of you. Fix each no, or report it as a
finding with the file and line, the condition that triggers it, and what a user or attacker gets.

## Checklist

1. Are secrets absent from arguments, logs, errors, events, URLs, fixtures, docs, and public assets?
2. Do logs hold only a fixed message and safe named fields?
3. Does identity come from the session or a signed job, never from the request body?
4. Does each read, write, tool call, callback, delayed job, and delivery recheck live access to the
   specific object?
5. Is input checked for size before parsing and parsed into typed values at the boundary?
6. Does every missing identity, key, setting, or policy answer deny? Does no error path allow?
7. Can untrusted text choose a tool, recipient, URL, grant, or approver anywhere? (It must not.)
8. Is model and tool output escaped or rendered as text, with control sequences stripped?
9. Does each URL a user or model can influence go through the shared outbound client?
10. Does sign-in and credential storage go through the product's one auth package?
11. Is each approval bound to the exact action and consumed once by a live human?
12. Is each native restriction described as enforced, monitored, or unsupported?
13. Is each new action pinned to a full SHA, and each new package pinned exactly, at least two
    weeks old, checked for name, publisher, and license, and covered by the lockfile?
14. Do workflow permissions stay read-only by default, with no secrets reachable by PR code?
15. Do site responses send the shared CSP and headers?
16. Does a test exercise the denial, the hostile input, or the escape attempt, not only success?

## Threat-model a new boundary

A boundary is new when a change adds an ingress route, a tool, a connector, an outbound
destination, a package source, a stored credential, a runner, or a public asset. Before you build
it, write these down in the PR's security section:

1. What crosses the boundary, and how it is classified (secret, personal, confidential, public).
2. Who can influence it: the user, another tenant, the model, repository content, the network.
3. Where authority is checked, by which code, and what the check reads.
4. Which legs one run gains: private read, untrusted read, outward write. All three need an exact
   human approval or the removal of a leg.
5. The bounds: size, time, count, retries, spend.
6. What happens when a dependency is down or a setting is missing. It must deny.
7. The tests that prove the denial and the hostile case.

## Report findings

- Separate confirmed defects from missing evidence and from design debt.
- Give each finding a severity, the evidence (source, test, or deployment), and the next check.
- A plan, an installed dependency, a fake test, or a local pass is not a compliance claim.
- Do not prescribe a rewrite or a new platform when an existing control can cover the gap.
