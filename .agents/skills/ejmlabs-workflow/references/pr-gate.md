# The PR gate

Opening or updating a PR authorizes no CI or AI spending. Human decisions do.

## Commands

All three are standalone PR comments by a current human maintainer (GitHub maintain or admin),
naming the full 40-character head SHA. Agents never post them, and never write permission rules
or scripts that post them for an agent.

| Comment             | Effect                                                                 |
| ------------------- | ---------------------------------------------------------------------- |
| `/ready FULL_SHA`   | Admits that commit. Quality, Repository standards and AI review start. |
| `/approve FULL_SHA` | Records the maintainer's approval of that commit after review.         |
| `/revoke FULL_SHA`  | Cancels the admitted run and blocks merge.                             |

Hand the user the commands ready to paste, for example:

```sh
gh pr comment 41 --repo ejmlabs/ejmlabs --body "/ready 0123...full sha"
gh pr comment 41 --repo ejmlabs/ejmlabs --body "/approve 0123...full sha"
```

## Rules

- Draft PRs cannot be admitted. Mark the PR ready first (`gh pr ready N`).
- A new commit needs new decisions. Push your final commit before asking for the comments, so
  the user posts them once.
- Decisions must stay unedited. Editing or deleting one revokes it.
- Each command is consumed once. Actions reruns and replayed events are rejected. To retry a
  failed run: revoke, investigate, fix, push, and ask for a fresh `/ready`.
- Fork PRs are not admitted automatically.
- When `main` moves and the PR is behind, update the branch (`gh pr update-branch N`). That
  makes a new head commit, so it needs fresh comments.
- With strict up-to-date checks, land one PR per repository at a time. Every merge puts the
  others behind, and updating them all at once only queues duplicate runs.

## Required statuses

**Human readiness**, **Maintainer approval**, **Quality**, **Repository standards** and, where
the repository's caller runs it, **AI review**. Read the PR's checks for the current set. Native
GitHub review alone does not replace the comments.

- AI review runs at high reasoning effort. A missing key, an incomplete review, or the
  `Possible security concern` label fails it. A maintainer may remove the label after reviewing
  the concern; the removal is logged and re-evaluates the gate. Address other findings in a new
  commit.
- AI review does not see lockfiles, minified files, assets, binaries, or files clipped by its
  token budget (listed in its coverage footer). Point the maintainer at those files before they
  approve.
- Repository standards checks community files and versions against the pinned shared standard.
  Never edit synced files by hand; resync them from `ejmlabs/.github`.

## Limits

| Limit                   | Bound      |
| ----------------------- | ---------- |
| Changed files           | 200        |
| Added and deleted lines | 12,000     |
| Quality job             | 45 minutes |
| AI review job           | 12 minutes |

Split larger work into a sequence of PRs that each pass alone.

## Merge

```sh
gh pr checks N --repo ejmlabs/REPO
gh pr merge N --repo ejmlabs/REPO --squash --match-head-commit FULL_SHA
```

Merge only when every check is green. Never merge as an administrator around a failing or
missing check.
