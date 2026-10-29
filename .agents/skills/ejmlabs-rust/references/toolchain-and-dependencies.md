# Toolchain, lints and dependencies

Read this before changing a toolchain or tool pin, a lint table, a dependency, `deny.toml`,
unsafe code, or a forked crate.

## Toolchain

- `rust-toolchain.toml` pins an exact release and the components the gate needs:

```toml
[toolchain]
channel = "1.99.1"
components = ["clippy", "rustfmt"]
profile = "minimal"
```

- `[workspace.package]` sets `edition = "2024"` and a `rust-version` equal to the pinned
  channel. An older compiler is not a supported configuration.
- `rustfmt.toml` sets `edition = "2024"` and `max_width = 100`. A repo may add options.
- Quality tools have exact versions (for example `cargo-deny` and `cargo-machete`), installed
  with `cargo install --locked --version X NAME`. The contributor guide and CI name the same
  versions; change every site in one patch.
- Choose a toolchain or tool release that has been out at least two weeks. Automated
  dependency updates use a cooldown of at least seven days.
- A missing tool or a stale lock is a failed gate, not a skipped step.

## Lint layer

The org strict tables, pasted into `[workspace.lints]` (or `[lints]` for a single crate):

| Table           | Denied                                                                                                                                                                            |
| --------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `rust`          | `unsafe_code`, `unused_must_use`                                                                                                                                                  |
| `clippy` groups | `all` and `pedantic`, both at `priority = -1`                                                                                                                                     |
| `clippy` lints  | `unwrap_used`, `expect_used`, `panic`, `todo`, `unimplemented`, `unreachable`, `dbg_macro`, `exit`, `mem_forget`, `undocumented_unsafe_blocks`, `allow_attributes_without_reason` |

- The org lint sync check compares these tables with each manifest and reports drift.
- A repo may be stricter (`unsafe_code = "forbid"`, `warnings = "deny"`, `missing_docs`), never
  looser. Never set a group to `warn` and rely on `-D warnings` alone: the manifest is the record.
- An exception is `#[expect(clippy::lint, reason = "...")]` on the smallest item. `expect`
  fails the build when the lint stops firing, so stale exceptions surface. The reason names a
  stable contract (a wire schema, a framework signature) or a measured tradeoff.
- Test modules carry one expectation for their fixtures:

```rust
#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "fixtures fail at the assertion site")]
mod tests {
```

- Never allow a lint group, lower a level, or exclude a file from a tool to make a patch pass.

## Unsafe code

- Project code has no unsafe. Native identity, page size, file descriptors and signals go
  through safe wrappers such as `rustix`.
- A new exception needs a documented package gap, the invariant that makes it sound, a platform
  test plan and review. It carries `#[expect(unsafe_code, reason = "...")]` and a `// SAFETY:`
  comment on the block. A repo that forbids unsafe has no exceptions.
- A native call never turns monitoring into enforcement. Describe a restriction as enforced,
  monitored or unsupported, per operating system.

## Dependencies

- `Cargo.lock` is committed. Every gate command passes `--locked`.
- Shared versions live once in `[workspace.dependencies]`; members write `name.workspace = true`.
- Turn off default features where a crate offers them and name the features you use.
- Pin with `=` only where an exact match is required: a fork, or a framework whose Rust crate
  must equal its JavaScript package (as Tauri's do). Record why next to the pin.
- Before adding a crate, check the standard library and existing dependencies. Then review the
  candidate's maintenance, security record, license, platforms, binary size, startup cost and
  transitive features. "Fewer dependencies" alone never justifies owning a parser, protocol or
  security primitive.
- Update one crate at a time: `cargo update -p NAME --precise VERSION`, adapt call sites, then
  run `cargo deny --locked --all-features check` and `cargo machete --with-metadata`.
- The advisory check needs the RustSec database. A network failure leaves it unverified.

## cargo-deny

- `deny.toml` denies duplicate versions (`multiple-versions = "deny"`), wildcard requirements,
  unknown registries and unknown git sources, and licenses outside an explicit allow list.
- Converge versions by upgrading first. A `skip` is the last resort: it names the exact crate
  and `=version` and a comment cites the upstream constraint. Remove it when the constraint
  lifts. Recheck skips whenever you bump the related crates.

```toml
skip = [
    # oauth2 5 requires thiserror 1; current transport crates use thiserror 2.
    { name = "thiserror", version = "=1.0.69" },
]
```

- A license exception names the exact crate and version and preserves the notice text.
- Never add a broad tree exception or a permanent advisory ignore without a recorded decision.

## Sibling path dependencies

- A workspace that depends on a sibling checkout by path uses only that sibling's public API.
  Never edit the sibling from here; tell its owning session what you need.
- After a sibling bump, refresh this repo's lock and build. A removed, renamed or retyped public
  item in a library you own is announced to every host that depends on it.

## Release profile

```toml
[profile.release]
lto = "thin"
codegen-units = 1
strip = true
```

## Forked crates

- Fork only for a reviewed gap upstream will not close in time. Keep the patch narrow.
- Publish under a distinct package name and keep the library name, so consumers write
  `name = { package = "ejm-NAME", version = "=X.Y.Z-ejm.N" }` and source stays unchanged.
- The fork records the upstream URL, the checksum-pinned base, the patch, the upstream pull
  request and the migration plan. Track upstream security fixes.
- Retained upstream source and lints stay as imported. The house lint layer applies to code we
  own; do not restyle forked source.
- Migrate consumers back when upstream ships an equivalent contract that passes the same tests.
