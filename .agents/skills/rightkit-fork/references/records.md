# The fork records

Read this before a rebase on a new upstream release, or when `scripts/records.sh` fails.

## What each record holds

| Record | Field | Holds |
| --- | --- | --- |
| `rightkit-fs.source.json` | `reference` | SHA-256 and byte count of every file in the upstream crates.io archive. Changes only in a rebase. |
| | `proposed` | The same files after the patch. `src/`, `tests/`, `Cargo.toml.orig` and the licenses match the tree; `README.md` matches the part below `## Original upstream documentation`. |
| | `utc` | When `proposed` last changed. |
| `rightkit-fs.provenance.json` | `upstream` | Registry URL, archive SHA-256 and license of the pinned base. |
| | `patch.sha256` | SHA-256 of `upstream.patch`. |
| | `source_manifest.sha256` | SHA-256 of `rightkit-fs.source.json`. |
| | `source_bytes` | Sums of the `reference` and `proposed` byte counts. |
| | `scope`, `dependencies`, `removal` | What the patch does, what it adds, and how the fork ends. Edit by hand when they change. |
| | `patch.path`, `publication_gate` | The historical harness vendor import. Leave them. |
| `upstream.patch` | | Python `difflib.unified_diff` of each file from the base to the tree, labels `a/PATH` and `b/PATH`, in sorted path order. That is the format of the reviewed patch, byte for byte. |

The `proposed` entries for `Cargo.toml`, `Cargo.lock` and `.cargo_vcs_info.json` hold the vendor
values from the import. The fork's own manifest and lock differ from them; the script ignores
those three files and the fork section of `README.md`.

## When the script fails

- `base differs from the upstream reference`: the commit the base is built from (`HEAD` by
  default) holds forked source that its own patch does not explain. If your last commit changed
  forked source without its records, run `records.sh write HEAD~1` and commit the records.
  Otherwise stop and report it: never rewrite `reference` to make it pass.
- `added forked files need a hand-made record and review`: a new file under `src/` or `tests/`.
  Adding a file is a scope change: ask the owner first. Its patch hunk needs a `--- /dev/null`
  header, which the script does not write.
- `upstream.patch is not the diff from upstream to the tree`, a `proposed` mismatch or a
  provenance hash: the tree changed without its records. Run `records.sh write`, read the diff
  of all three files, and commit them with the source change.

## Check the base against crates.io

The script proves the rebuilt base against `reference`. To prove `reference` itself, download
the pinned archive and compare:

```sh
crate=$(mktemp -d)
curl -fsSL --max-time 60 -o "$crate/up.crate" \
  https://static.crates.io/crates/rightkit-fs/rightkit-fs-0.4.4.crate
echo "32e10ada6eb1323be113c4a7ee181a877cf3a1698bf66e1d278362e439f0501a  $crate/up.crate" \
  | sha256sum -c -
tar -xzf "$crate/up.crate" -C "$crate"
```

Every file in `$crate/rightkit-fs-0.4.4/` must match its `reference` entry, and there must be
no other file.

## Rebase on a new upstream release

A rebase changes the base, so `records.sh` cannot write it. Do it in its own PR, with the
owner's review.

1. Pick an upstream release at least two weeks old. Fetch its archive from crates.io as above
   and check its SHA-256 against the `cksum` field for that version in the crates.io index
   (`https://index.crates.io/ri/gh/rightkit-fs`).
2. Start from the upstream port of the patch (PR #876 targets 0.4.6) or port `upstream.patch`
   by hand. Keep the same scope.
3. Replace `src/`, `tests/`, `Cargo.toml.orig`, the licenses and the upstream part of
   `README.md` with the patched files of the new release. Take the new archive's `Cargo.toml`
   and apply the fork's `[package]` metadata to it again.
4. Rewrite `reference` from the new archive and `proposed` from the patched tree. Set
   `upstream.registry`, `upstream.archive_sha256`, `version` and `source_bytes.baseline`, then
   `patch.sha256`, `source_manifest.sha256` and `source_bytes.patched`. Write both JSON files as
   `json.dumps(data, indent=2)` plus a final newline, the form they use now.
5. Commit, then run `records.sh`: it rebuilds the new base from `HEAD` and checks it against the
   new `reference`.
6. Version `X.Y.Z-ejm.1`, a new RELEASE-VERIFICATION.md section, and FORK.md updated to name the
   new base.
