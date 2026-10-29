# Dependencies

Read when you add, update, replace or remove a dependency, tool pin or toolchain.

## Choosing

- Standard library, platform API and installed dependencies first. Then an established package
  that meets the exact need. Then, with a recorded reason, custom code.
- Before adding a package, check its exact name and publisher (typosquats), license, current
  maintenance, open security advisories, platform support and size. Use the official package of
  the service you integrate with.
- One package per job. Do not add a second library that overlaps one already in use.
- Never add a dependency for a few lines of obvious code.

## Pinning

- npm manifests pin exact versions (`"vite": "6.4.5"`, not `^6.4.5`). Python and Cargo pin
  through the committed `uv.lock` and `Cargo.lock`; tools and lint versions are pinned exactly.
  Every ecosystem installs from its lockfile in CI and scripts (`npm ci`, `uv sync --locked`,
  `cargo --locked`).
- Never hand-edit a lockfile. Regenerate it with the package manager.
- New or updated versions are at least two weeks old, unless the update fixes a security
  advisory that affects us.
- Pin toolchains and tools in the repository (`rust-toolchain.toml`, `.nvmrc` or `engines`,
  `requires-python`, the strict lint layer versions). A missing tool or a stale lock is a failed
  gate, not a skipped check.
- GitHub Actions are pinned to full commit SHAs from approved sources.

## Reviewing

- Read the changelog for breaking changes before a MAJOR update. Run the full gate.
- Dependabot PRs go through the same human gate as any other PR.
- License: permissive licenses (MIT, Apache-2.0, BSD, ISC, Zlib, OFL for fonts) are fine.
  Anything else, or a license change, needs the owner's review. Keep `THIRD_PARTY_NOTICES`
  current when you ship a dependency's code or assets.
- Rust uses `cargo-deny` for advisories, licenses, bans and sources. A deny exception names the
  exact crate, version, upstream constraint and removal condition.

## Recording exceptions

When you write custom infrastructure instead of using a package, record next to the code or in
the repository's design notes: the capability the packages lack, the alternatives you checked,
and the evidence. When you touch existing custom machinery, check again whether a maintained
package now fits.
