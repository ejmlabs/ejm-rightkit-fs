# Agents, tools, and execution

Read before you add or change a tool, an agent or child agent, an approval, a sandbox or runner,
a package host, file access for a model, or a claim about native restrictions.

## Count the legs of each run

- A run is open to data theft when it holds private data, reads untrusted content, and can send
  data out. For each new tool, write in the PR which legs it adds: private read, untrusted read,
  outward write.
- A run that holds all three needs an exact human approval, or remove a leg. A prompt, a filter,
  or a detector is not a control.

## Enforce outside the model

- Permission, budget, approval, and destination decisions run in deterministic code at the point of
  execution. Text in a prompt or tool description is guidance, not enforcement. Never describe it
  as a control.
- Untrusted text may fill an argument that policy allows. It never picks the tool, the recipient,
  or the destination.
- Registering a tool is the grant. Write, patch, and shell tools need an explicit host setting.
  No repository file, model output, or session option adds a tool or an effect.
- Enforce allowlists and argument constraints at execution, for delegated calls too. A child agent
  inherits its parent's tools, identity, budget, and controls, and may only narrow them.

## Approvals

- Only a live human approves, through a direct message or a signed-in browser session. Service
  identities, plugins, automatic callbacks, and agents never approve and never create approval
  evidence. Agent reviews never count as an approval, and a merge is not one.
- An approval binds the exact action, arguments or artifact, target, and policy version. Store a
  hash and consume it once, atomically (one `UPDATE ... WHERE state = 'approved' ... RETURNING`).
- Recheck revocation and expiry at execution.
- Record intent before an external effect. Never replay an effect whose result is uncertain; stop
  it for human review.

## Credentials and models

- Never put a credential in model input, tool results, sandboxes, or logs. Give the model a
  reference, attach the value on the server, send it only to its saved destination, and scrub the
  reply, because a model can repeat what it sees.
- Tokens are audience-bound. Never pass a token received for one service through to another.

## Model-written and untrusted code

- Model-written, plugin, package, and customer code never runs in the server process or the
  browser. It runs in an isolated runner with no credentials, no host files, bounded resources, and
  no network or an allowlist with everything else denied.
- Render untrusted files in an offline sandbox.
- A public package is unreviewed community code. Installing or updating it never activates new
  native code without explicit local trust. A hash proves the bytes, not their safety.

## Native restrictions

Use these three words in code, docs, and reports:

| Word        | Meaning                                                                                                         |
| ----------- | --------------------------------------------------------------------------------------------------------------- |
| enforced    | The OS blocks it, and the check runs before launch.                                                             |
| monitored   | The program observes it (sampling, process groups) and may stop the work after the fact. It is not containment. |
| unsupported | This platform cannot provide it. Say so.                                                                        |

- A required restriction that cannot be enforced rejects the launch.
- A claim names the platform it was checked on. Evidence from one OS says nothing about another.
- Native execution on a user's machine is never called a sandbox.

## File access for tools

- Reach files through a directory capability for the session workspace.
- Reject `..`, absolute paths, prefixed paths, and symlinks that leave the workspace. Check the
  resolved parent, not only the requested string.
- Protect `.git` and the tool's own state directory, matched case-insensitively.
- An edit carries a hash precondition from a prior read. A stale or missing precondition fails the
  edit. Never overwrite on a guess.
- Keep every limit visible to the model. A truncated result says it was truncated.

## Terminal output

- Treat control sequences in model and tool output as untrusted. Strip or escape escape characters
  and other control characters before writing to a terminal, so output cannot move the cursor,
  rewrite earlier lines, set the title, or write the clipboard.
- Restore terminal state after errors, cancellation, and panic.
