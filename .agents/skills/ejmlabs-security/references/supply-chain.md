# Supply chain, CI, and going public

Read before you add or update a dependency, a tool pin, or a lockfile; change a workflow, its
permissions, or its secrets; respond to a leaked credential; or prepare a repository to go public.

## Add a dependency

`ejmlabs-engineering` covers whether to add a package. Before you add one, also:

1. Check the exact package name and its publisher. A near-miss name is a typosquat until proven
   otherwise.
2. Review its security record, license and redistribution terms, and transitive dependencies.
3. Pick an exact release at least two weeks old, unless the update fixes a security advisory that
   affects us. Say which in the PR.

## Pins and locks

- npm manifests use exact versions, never `^`, `~`, or ranges. Cargo resolves exactly through the
  committed `Cargo.lock`, and every build and test uses `--locked`. Python resolves through the
  committed `uv.lock` with `--locked`.
- Quality tools have an exact version and a documented install command. A pin that appears in
  several places (docs, workflow, tool file) changes in all of them in one patch.
- One package manager and one lockfile per ecosystem. Commit every lockfile. Never hand-edit one.
- Install with lifecycle scripts disabled: `npm ci --ignore-scripts`.
- A stale lock or a missing tool is a failed gate, not a skipped check.

## Audits and exceptions

- Rust: `cargo deny --locked --all-features check` (advisories, sources, licenses, duplicate
  versions) and `cargo machete` for unused dependencies.
- Other ecosystems: the repository's pinned scanner run on the exact candidate lockfile. A
  repository may require a scanner that keeps the dependency inventory local.
- The advisory database needs the network. A network failure leaves the audit unverified, not green.
- Prefer upgrading to converge versions over adding an exception.
- An exception names the exact package and version or advisory, the upstream constraint that forces
  it, an owner, and when it goes. Remove it once the constraint lifts. Never add a broad tree
  exception or a permanent advisory ignore.

## Licenses and notices

- Check license and redistribution obligations before you import code, models, datasets, fonts, or
  images. Keep the required notices in the third-party notices file.
- Never add or change a license on proprietary work without the owner's approval.

## GitHub Actions

- Pin every action to a full 40-character commit SHA from an allowed source.
- Default workflow tokens are read-only. Each job asks only for the permissions it uses.
- Never use `pull_request_target` with code from a fork or branch. Never give PR code a model,
  release, deploy, or source-read credential.
- CI and AI review spend nothing until a human maintainer admits the exact head commit.
- Model keys live only as secrets of a protected environment restricted to `main`, never as
  repository or organization secrets. A missing key fails the check closed.
- Private source access uses an expiring, fine-grained token with read access to one repository.
  Never a broad personal token. A successful checkout does not prove the token's scope or expiry.

## When a credential leaks

1. Stop the work that could spread it, and tell the user at once.
2. Do not rotate, delete, or rewrite history without their approval.
3. The owner revokes or rotates it at the provider, removes the stored value, audits the runs and
   logs that could have used it, and readmits work only after containment.
4. A deleted commit, file, or link does not remove forks, caches, logs, or release copies.

## Public repositories and archives

- Keep the names, domains, credentials, contracts, and metadata of unrelated private products out
  of public source, docs, fixtures, generated files, and release archives.
- A public example must not point at an endpoint that is not verified and deployed.
- Agents never change visibility. They prepare the checklist; the owner decides.

Public visibility checklist:

1. Audit current refs and full history, discussions, reviews, retained Actions logs and
   artifacts, and release assets for private data.
2. Review licenses, asset rights, and open vulnerability findings. Bind the review to exact source
   and output identities.
3. Pause Actions, restore the full-SHA action allowlist while paused, recheck for drift, then
   change visibility.
4. After the change: require approval for all outside contributors' workflows, keep fork secrets
   and write tokens off, and confirm read-only default tokens and branch protection.
5. Turn on secret scanning, push protection, private vulnerability reporting, dependency alerts,
   and security updates. Enable Actions last and read every setting back.
