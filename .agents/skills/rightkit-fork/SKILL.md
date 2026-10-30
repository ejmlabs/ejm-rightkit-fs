---
name: rightkit-fork
description: Use before you change ejm-rightkit-fs, including src/, tests/, upstream.patch, rightkit-fs.source.json, rightkit-fs.provenance.json, Cargo.toml, README.md or FORK.md, or before you backport an upstream fix, rebase on a new upstream release, bump the version, or release.
---

# Maintaining the rightkit-fs fork

The fork is a narrow, recorded patch on a checksum-pinned upstream release. Every byte that
differs from upstream is in `upstream.patch` and the two records, and every claim in `FORK.md`
and `RELEASE-VERIFICATION.md` comes from a run.

## What may change

- Fork-owned files: the `[package]` metadata in `Cargo.toml` (name, version, description,
  repository, `include`), the part of `README.md` above `## Original upstream documentation`,
  `FORK.md`, `RELEASE-VERIFICATION.md`, `Cargo.lock` (through Cargo only), the macOS diagnostic
  under `.github/` and the agent guidance.
- Forked source: `src/`, `tests/`, the upstream part of `README.md`, `Cargo.toml.orig` and the
  `LICENSE-*` files. Change it only to backport an upstream security fix or to fix the patch's
  own scope: direct permanent empty-file creation, opt-in same-handle owner-only reads, the
  bounded debug-only creation seam, and their docs and contract tests.
- Everything else goes upstream, not here. The patch is proposed upstream as
  [bogusyogi/rightkit#876](https://github.com/bogusyogi/rightkit/pull/876).
- Add no dependency, feature, public API outside the patch scope, or production background
  work (thread, task, timer, listener, watcher).
- Never edit `Cargo.toml.orig` or the licenses. In `Cargo.toml`, keep the features, `[[bin]]`,
  `[[test]]`, dependency and `[package.metadata.rightkit]` tables as imported.
- Never add, remove, rename or split a forked file. The records script refuses a new file;
  adding one is a scope change for the owner.
- A change to forked source ends FORK.md's claim that source and tests are byte-identical to
  the reviewed import. Rewrite that sentence to say what changed, and ask a human maintainer to
  review the patch diff before merge.

## Keep upstream's shape

- Forked files keep upstream's edition 2021, `rust-version = "1.98.0"`, module layout, style,
  `#[allow]` attributes and `unsafe` blocks. The house lint tables, rustfmt settings and
  no-unsafe rule in `ejmlabs-rust` apply to code ejmlabs owns, not to forked files. Patch code
  inside a forked file follows that file's existing style.
- Never run `cargo fmt`, `cargo clippy --fix` or any formatter over forked files: the diff must
  stay the patch, so it ports to newer upstream releases.
- Retained upstream README text keeps its wording and links, even links to files that only
  exist upstream.
- `.gitattributes` marks `upstream.patch` `-whitespace`. Never let an editor trim it: a blank
  context line is a single space.

## Record every change

A change to forked source lands in one commit with its records:

1. Edit the tree.
2. `bash .agents/skills/rightkit-fork/scripts/records.sh write`. It rebuilds the upstream base
   offline from `HEAD` (its tree with its own patch reversed), checks it against the upstream
   `reference` hashes, then rewrites `upstream.patch`, the `proposed` entries and `utc` in
   `rightkit-fs.source.json`, and `patch.sha256`, `source_manifest.sha256` and
   `source_bytes.patched` in `rightkit-fs.provenance.json`.
3. Update FORK.md, the provenance `scope` when the scope changes, and the version (Releases).

- Never hand-edit a hash, a byte count or the patch. Rerun the script.
- Leave the provenance `patch.path` and `publication_gate` fields as they are: they describe the
  historical harness vendor import, as FORK.md says. Record new facts in FORK.md.

## The gate

The macOS guard ACL diagnostic in `.github/workflows/macos-acl-probe.yml` runs on pull requests
that change its probe or workflow files, or when manually dispatched. It does not replace the
local gate. Run these from the repository root and paste each result into the PR:

```sh
export CARGO_BUILD_JOBS=2 CARGO_INCREMENTAL=0
bash .agents/skills/rightkit-fork/scripts/records.sh
cargo test --locked --features testing --test state_durability --test acl_macos
cargo package --locked --features fs
```

- Also run the test files that cover the module you changed (README "Tests" maps them), with
  the `required-features` that `Cargo.toml` lists for each `[[test]]`.
- `acl_macos` compiles to zero tests off macOS. A run proves only the platform it ran on: macOS
  evidence never covers Linux or Windows, and both stay unverified until a native run.
- A change to fork-owned docs only needs the records script and a link check.

## Upstream fixes and migration

- Before each release, check upstream releases after the pinned base
  ([bogusyogi/rightkit](https://github.com/bogusyogi/rightkit), crates.io `rightkit-fs`) and
  RustSec for fixes to files this crate ships. Backport a small fix and record it; a larger one
  means a rebase.
- Migrate consumers back when an upstream release supplies the equivalent contract and passes
  the same concurrency, creator-death and privacy checks. Archiving the fork is the owner's call.

## Releases

- The version is `UPSTREAM-ejm.N` in `Cargo.toml`: `0.4.4-ejm.2` after `0.4.4-ejm.1`, and
  `X.Y.Z-ejm.1` after a rebase. Change the README `toml` snippet and `Cargo.lock` in the same PR.
- Add a section to RELEASE-VERIFICATION.md in its existing form: date, OS and architecture, Rust
  version, Cargo job limits, each command and result, the `cargo package` archive SHA-256, and
  the publication status.
- Publishing (`cargo publish --locked --features fs`) and tags need the user's explicit request.
  Never read, enter or ask for a crates.io token.
- Never call a version published until the registry serves it
  (`cargo info ejm-rightkit-fs@VERSION`). Source on GitHub is not a published package.
- Consumers pin the fork with `=`. Each consumer moves to a new release in its own repository.

## Read next

- [references/records.md](references/records.md): what each record field holds, what a records
  script failure means, and how to rebase on a new upstream release.
