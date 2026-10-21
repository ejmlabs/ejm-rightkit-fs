# Release verification: 0.4.4-ejm.1

2026-10-21, macOS arm64, Rust 1.99.1.

- All 69 production source and test files are byte-identical to the reviewed
  patched harness vendor tree. The retained source manifest identifies it.
- `cargo test --locked --features testing --test state_durability --test acl_macos`
  passed: 25 state tests and 10 macOS ACL tests.
- Existing harness creator-death and concurrency evidence remains historical
  downstream evidence. This standalone run does not repeat or replace it.
- Native Linux and Windows runs remain unverified. No broader platform claim.

Cargo jobs: two. Incremental compilation disabled. Shared CLI target reused.
Publication status must be checked in the registry; source availability alone
is not package publication.

`cargo package --locked --features fs` passed. Cargo built the unpacked
registry archive successfully. Archive SHA256:
`b0be009076fc51f0a933e9ba0ea058a9cf196ebb0eb46526813c1bb4f72fb9b9`.

`cargo publish --locked --features fs` stopped before upload because no
crates.io token is configured. GitHub source publication succeeded.
