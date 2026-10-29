#!/usr/bin/env bash
# Check that upstream.patch and the source and provenance records describe the working tree,
# or rewrite them so they do. Run from anywhere in the repository:
#   records.sh [check|write] [REF]
# The upstream base is rebuilt offline from commit REF (default HEAD): its tree with its own
# patch reversed. It must match the upstream hashes in rightkit-fs.source.json.
set -euo pipefail
here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
usage() { echo "usage: records.sh [check|write] [REF]"; }
case ${1:-check} in
  check | write) mode=${1:-check} ref=${2:-HEAD} ;;
  -h | --help) usage; exit 0 ;;
  *) usage >&2; exit 2 ;;
esac
command -v python3 >/dev/null || { echo "Missing python3" >&2; exit 1; }
root=$(git -C "$here" rev-parse --show-toplevel)
cd "$root"
work=$(mktemp -d)
cleanup() { rm -rf "$work"; }
trap cleanup EXIT
# The upstream README is the part below the fork's own heading.
upstream_readme() { sed '1,/^## Original upstream documentation$/d' "$1" | sed '1{/^$/d}'; }
mkdir "$work/base" "$work/new"
git archive "$ref" src tests README.md | tar -x -C "$work/base"
git show "$ref:upstream.patch" >"$work/ref.patch"
upstream_readme "$work/base/README.md" >"$work/readme"
mv "$work/readme" "$work/base/README.md"
(cd "$work/base" && git apply -R ../ref.patch)
cp -r src tests "$work/new/"
upstream_readme README.md >"$work/new/README.md"
python3 - "$work" "$mode" <<'PY'
import datetime, difflib, hashlib, json, pathlib, sys
work, mode = pathlib.Path(sys.argv[1]), sys.argv[2]
base, new = work / "base", work / "new"
fork = {".cargo_vcs_info.json", "Cargo.toml", "Cargo.lock", "README.md"}
def entry(data): return {"sha256": hashlib.sha256(data).hexdigest(), "bytes": len(data)}
def files(root): return {str(p.relative_to(root)) for p in root.rglob("*") if p.is_file()}
src = json.load(open("rightkit-fs.source.json"))
prov = json.load(open("rightkit-fs.provenance.json"))
names = sorted(files(base))
if bad := [n for n in names if entry((base / n).read_bytes()) != src["reference"].get(n)]:
    sys.exit("base differs from the upstream reference: " + " ".join(bad))
if extra := sorted(files(new) - set(names)):
    sys.exit("added forked files need a hand-made record and review: " + " ".join(extra))
patch = "".join(line for n in names for line in difflib.unified_diff(
    (base / n).read_text().splitlines(True), (new / n).read_text().splitlines(True), "a/" + n, "b/" + n))
proposed = dict(src["proposed"])
proposed.update({n: entry((new / n).read_bytes()) for n in names})
proposed.update({n: entry(pathlib.Path(n).read_bytes()) for n in proposed if n not in fork and n not in names})
if mode == "write":
    pathlib.Path("upstream.patch").write_text(patch)
    if proposed != src["proposed"]:
        src["utc"] = datetime.datetime.now(datetime.timezone.utc).isoformat()
    src["proposed"] = proposed
    pathlib.Path("rightkit-fs.source.json").write_text(json.dumps(src, indent=2) + "\n")
    prov["patch"]["sha256"] = entry(patch.encode())["sha256"]
    prov["source_manifest"]["sha256"] = entry(pathlib.Path("rightkit-fs.source.json").read_bytes())["sha256"]
    prov["source_bytes"]["patched"] = sum(e["bytes"] for e in proposed.values())
    pathlib.Path("rightkit-fs.provenance.json").write_text(json.dumps(prov, indent=2) + "\n")
problems = [what for what, ok in (
    ("upstream.patch is not the diff from upstream to the tree", pathlib.Path("upstream.patch").read_text() == patch),
    ("rightkit-fs.source.json proposed entries differ from the tree", src["proposed"] == proposed),
    ("provenance patch.sha256", prov["patch"]["sha256"] == entry(pathlib.Path("upstream.patch").read_bytes())["sha256"]),
    ("provenance source_manifest.sha256", prov["source_manifest"]["sha256"] == entry(pathlib.Path("rightkit-fs.source.json").read_bytes())["sha256"]),
    ("provenance source_bytes.patched", prov["source_bytes"]["patched"] == sum(e["bytes"] for e in proposed.values())),
) if not ok]
print("\n".join(problems) or "records match the tree")
sys.exit(bool(problems))
PY
