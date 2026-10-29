---
name: ejmlabs-workflow
description: Use at the start of any change in an ejmlabs repository and before you commit, push, open or merge a PR, deploy, release, or hand off. Covers starting safely beside other agents, OpenSpec, branches, the human PR gate, shipping, and reporting.
---

# The ejmlabs workflow

Finish what you start. A requested change is done when it is merged, deployed where the
repository deploys, checked live, and reported honestly.

## Start

- Run `git status`, `git log -5` and `git worktree list` first. Other agents and sessions may be
  editing this repository or its siblings at the same time.
- Never edit a checkout that holds someone else's uncommitted work or feature branch. Do your work
  on a new branch in its own worktree or clean clone, cut from a fresh `origin/main`. Follow the
  repository's own worktree command when it has one.
- Change only the paths your task needs. Never revert, reformat, or commit changes you did not
  make. Never use bare `git stash` or `git stash pop`; the stash is shared across worktrees.
- Read the repository's AGENTS.md, then each skill whose trigger matches the work.
- If the repository has `openspec/`, feature and bug fix work goes through OpenSpec: explore,
  propose, apply, update the plan when it changes, sync specs, archive after verification. Use
  the matching `openspec-*` skill for each step. Do not implement from a plain request there.

## Build

- Follow an existing pattern, or define one and record it in the repository's docs or skill
  before you use it (see `ejmlabs-engineering`).
- Run focused checks while editing and the repository's single gate before you hand off.
- When stuck or the same error repeats, stop repeating the approach. Ask a stronger reviewer if
  one is available, or report the blocker with what you tried and the exact error.

## Commit and PR

- Branch names: `<kind>/<short-slug>`, where kind is one of `feat`, `fix`, `docs`, `copy`,
  `design`, `cleanup`, `deps`, `ci` or `release`. Follow a repository rule if it sets another.
- Stage files by name. Never `git add -A` or `git add .`. Never commit secrets, local logs,
  build output, or another agent's unfinished work.
- Commit and PR titles are short imperative sentences in sentence case, with no type prefix:
  "Add gerry's mark to its home row". The body says why, and what was verified.
- Never bypass hooks with `--no-verify`. Never push to `main`, force-push a shared branch, or
  rewrite published history.
- Keep PRs small and single-purpose: the gate admits at most 200 files and 12,000 changed lines.
- Fill in the PR template: summary, verification (what ran, where, and what did not), and
  security or compatibility effects.

## The human gate

Repositories on the shared `ejmlabs/.github` gate need two comments from a human maintainer on
the exact head commit: `/ready FULL_SHA` (admits CI and AI review) and, after review,
`/approve FULL_SHA`. Agents never post these. As soon as the PR is open, give the user both
commands in one message, with the full 40-character head SHA, and continue the moment they say
done. When the repository's AGENTS.md describes a different gate, follow it. Details, limits and
recovery are in [references/pr-gate.md](references/pr-gate.md).

## Ship

- When the work is built and checked, carry it through without asking at each step: commit,
  push, PR, gate commands, merge, deploy, live check. Stop only for a gate only a human can pass,
  a failing check, or an ambiguous product decision.
- Merge only when every check is green, including ones GitHub does not require. Never merge on a
  pending or failed check. Use `gh pr merge N --squash --match-head-commit FULL_SHA`.
- Never rerun failed Actions jobs to get green. Find the cause, fix it in a new commit, and get
  fresh gate comments.
- Deploy only from merged `main`, the way the repository's deploy skill says. Releases (tags,
  package publishing, signed installers) and approval-gated production deploys need the user's
  explicit request. See [references/releases.md](references/releases.md).
- After a merge, delete your branch and worktree. Do not leave finished work stranded.

## Report

- Say what changed, with links to the PR, commit, deploy or release.
- Say what you verified, with which command and on which platform, and what you did not.
  A skipped, failed or unavailable step is unverified, never green.
- Never claim a check passed, a test ran, or a site is live unless you saw it in this session.
- When the user corrects you or sets a standard that will apply again, propose the edit to the
  repository's AGENTS.md or the owning skill in the same task.

## Read next

- [references/pr-gate.md](references/pr-gate.md): admission, approval, revocation, required
  statuses, AI review labels, limits.
- [references/parallel-work.md](references/parallel-work.md): fanning work out to several agents
  and coordinating with sibling repositories.
- [references/releases.md](references/releases.md): versions, changelogs, draft releases, tags.
