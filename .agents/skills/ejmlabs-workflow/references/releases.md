# Releases

Each repository has its own [Semantic Version](https://semver.org/) in `VERSION`. A release is
a reviewed PR, a merge, and an explicit human step. Agents prepare it; publishing needs the
user's request.

## Version and notes

- MAJOR for breaking changes, MINOR for compatible features, PATCH for compatible fixes. Before
  1.0 a MINOR may break the API; a PATCH may not. Prereleases use `-alpha.1`, `-beta.1`, `-rc.1`.
- Change `VERSION` together with every package manifest that carries the version, in one PR.
  The Repository standards check rejects a mismatch.
- Add a dated section to `CHANGELOG.md` in the same PR. Write for the user of the release:
  what changed, what breaks, how to upgrade, and the platforms with evidence.
- A dependency pin moves on its own schedule and is not the consumer's version.

## Steps

1. Release PR: version, changelog, upgrade notes, platform evidence. Gate and merge as usual.
2. Copy the full merge commit SHA on `main`.
3. A human maintainer runs **Draft release** from `main` with that SHA and the version without a
   `v` prefix. It validates versions, dated notes, ancestry and checks, refuses an existing tag,
   and creates an immutable `vVERSION` tag and a draft release with source archives, SHA-256
   checksums and a provenance manifest.
4. Review the draft, then publish explicitly.

## Rules

- Source archives are not installers or registry packages. npm, PyPI, crates.io, native
  installers and deploys each need their own build, credentials and gate. Never claim one exists
  until it is built and verified.
- Native installers are built and signed on native OS runners, with checksums published.
- Never move a released tag or replace its assets. Fix a bad release with a new version and say
  why in the notes.
- Shared workflow updates land through reviewed PRs that change the full commit pins in each
  consumer.
