#!/usr/bin/env bash
# Builds the Mihomo core MULTICORE ships, as pinned by packaging/mihomo-patches/manifest.json:
# the SHA-256-pinned MetaCubeX source archive + the patch series in packaging/mihomo-patches,
# compiled with the pinned stock Go release and the MetaCubeX release flags.
#
# usage: scripts/build-mihomo.sh --output <new-dir> [--cache <dir>] [--update-pins] [target ...]
#   targets: keys of manifest "targets" (default: all of them)
#   --update-pins  bump mode: write the built binaries' SHA-256 into the manifest instead of
#                  verifying them (review the diff; never used by a release)
# env:  GOMAXPROCS              passed through (keep builds polite on a shared machine)
#       MIHOMO_SKIP_TESTS=1     skip go vet/test of the patched packages (local iteration only)
#
# Output: one binary per target, SHA256SUMS, BUILDINFO.txt and mihomo-source.zip (the patched
# source tree plus the patches, the manifest and this script: the GPL corresponding source).
#
# Fails, naming the cause, when a patch is missing, extra, altered or no longer applies, when the
# Go toolchain or the source archive does not match its pin, or when a built binary's SHA-256
# differs from the manifest. It never produces an unpatched or unpinned core.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PATCH_DIR="$ROOT/packaging/mihomo-patches"
MANIFEST="$PATCH_DIR/manifest.json"

fail() { echo "build-mihomo: ERROR: $*" >&2; exit 1; }
usage() { echo "usage: $0 --output <new-dir> [--cache <dir>] [--update-pins] [target ...]" >&2; exit 2; }

OUTPUT=""
CACHE="$ROOT/target/mihomo-cache"
UPDATE_PINS=0
TARGETS=()
while [[ $# -gt 0 ]]; do
    case "$1" in
        --output) [[ $# -ge 2 ]] || usage; OUTPUT="$2"; shift 2 ;;
        --cache) [[ $# -ge 2 ]] || usage; CACHE="$2"; shift 2 ;;
        --update-pins) UPDATE_PINS=1; shift ;;
        -*) usage ;;
        *) TARGETS+=("$1"); shift ;;
    esac
done
[[ -n "$OUTPUT" ]] || usage
[[ ! -e "$OUTPUT" ]] || fail "refusing to overwrite existing $OUTPUT"
# Anchor a relative --output to the caller's directory: the build cds into the source tree.
mkdir -p "$(dirname "$OUTPUT")"
OUTPUT="$(cd "$(dirname "$OUTPUT")" && pwd)/$(basename "$OUTPUT")"

PY=""
for candidate in python3 python; do
    if command -v "$candidate" >/dev/null 2>&1 && "$candidate" -c 'import sys; sys.exit(0 if sys.version_info >= (3, 8) else 1)' >/dev/null 2>&1; then
        PY="$candidate"
        break
    fi
done
[[ -n "$PY" ]] || fail "python 3.8+ is required"
command -v git >/dev/null 2>&1 || fail "git is required (git apply)"
command -v curl >/dev/null 2>&1 || fail "curl is required"

case "$(uname -s)" in
    Linux) HOST_PLATFORM="linux-amd64"; IS_WINDOWS=0 ;;
    Darwin) HOST_PLATFORM="darwin-arm64"; IS_WINDOWS=0 ;;
    MINGW*|MSYS*|CYGWIN*) HOST_PLATFORM="windows-amd64"; IS_WINDOWS=1 ;;
    *) fail "unsupported build host $(uname -s)" ;;
esac
case "$HOST_PLATFORM" in
    darwin-arm64) [[ "$(uname -m)" == "arm64" ]] || fail "the pinned macOS Go archive is for Apple silicon build hosts" ;;
    *) [[ "$(uname -m)" == "x86_64" ]] || fail "the pinned Go archives are for x86_64 build hosts" ;;
esac

# native <path>: the path as the (native) Go toolchain and Python expect it.
native() { if [[ "$IS_WINDOWS" == 1 ]]; then cygpath -m "$1"; else printf '%s' "$1"; fi; }
sha() { "$PY" -c 'import hashlib,sys; h=hashlib.sha256(); f=open(sys.argv[1],"rb"); [h.update(b) for b in iter(lambda: f.read(1<<20), b"")]; print(h.hexdigest())' "$(native "$1")"; }
# m <python-expr over m>: read the manifest; lists print one item per line.
m() {
    "$PY" -c 'import json,sys
m=json.load(open(sys.argv[1], encoding="utf-8"))
v=eval(sys.argv[2])
print("\n".join(map(str, v)) if isinstance(v, list) else v)' "$(native "$MANIFEST")" "$1"
}

mkdir -p "$CACHE"
CACHE="$(cd "$CACHE" && pwd)"

# fetch <url> <sha256> <file>: download once into the cache; the bytes must match the pin.
fetch() {
    local url="$1" want="$2" file="$CACHE/downloads/$3"
    mkdir -p "$CACHE/downloads"
    if [[ ! -f "$file" ]] || [[ "$(sha "$file")" != "$want" ]]; then
        rm -f "$file" "$file.part"
        curl -fsSL --proto '=https' --tlsv1.2 --retry 3 -o "$file.part" "$url"
        mv "$file.part" "$file"
    fi
    local got
    got="$(sha "$file")"
    [[ "$got" == "$want" ]] || fail "$3: SHA-256 $got does not match the pinned $want"
    printf '%s' "$file"
}

# --- patches: exactly the manifest's series, byte for byte -------------------------------------
mapfile -t LISTED < <(m '[p["file"] for p in m["patches"]]' | tr -d '\r')
mapfile -t ONDISK < <(cd "$PATCH_DIR" && ls -1 -- *.patch 2>/dev/null | LC_ALL=C sort)
[[ "$(printf '%s\n' "${LISTED[@]}")" == "$(printf '%s\n' "${ONDISK[@]}")" ]] ||
    fail "packaging/mihomo-patches does not match the manifest patch list (manifest: ${LISTED[*]}; on disk: ${ONDISK[*]})"
for i in "${!LISTED[@]}"; do
    want="$(m "m['patches'][$i]['sha256']" | tr -d '\r')"
    got="$(sha "$PATCH_DIR/${LISTED[$i]}")"
    [[ "$got" == "$want" ]] || fail "patch ${LISTED[$i]}: SHA-256 $got does not match the manifest pin $want"
done

mapfile -t ALL_TARGETS < <(m 'list(m["targets"])' | tr -d '\r')
[[ ${#TARGETS[@]} -gt 0 ]] || TARGETS=("${ALL_TARGETS[@]}")
for t in "${TARGETS[@]}"; do
    printf '%s\n' "${ALL_TARGETS[@]}" | grep -qx -- "$t" || fail "target $t is not in the manifest"
done
if [[ "$UPDATE_PINS" == 1 && ${#TARGETS[@]} -ne ${#ALL_TARGETS[@]} ]]; then
    fail "--update-pins builds every target"
fi

VERSION="$(m 'm["upstream"]["version"]' | tr -d '\r')"
COMMIT="$(m 'm["upstream"]["commit"]' | tr -d '\r')"
SERIES="$(m 'm["series"]' | tr -d '\r')"
[[ "$COMMIT" =~ ^[0-9a-f]{40}$ ]] || fail "upstream commit must be a full lowercase commit"
[[ "$(m 'm["build"]["version"]' | tr -d '\r')" == "$VERSION" ]] || fail "build.version must equal upstream.version"

# --- toolchain ----------------------------------------------------------------------------------
GO_VERSION="$(m 'm["go"]["version"]' | tr -d '\r')"
GO_URL="$(m "m['go']['archives']['$HOST_PLATFORM']['url']" | tr -d '\r')"
GO_SHA="$(m "m['go']['archives']['$HOST_PLATFORM']['sha256']" | tr -d '\r')"
[[ "$GO_URL" == "https://go.dev/dl/$GO_VERSION."* ]] || fail "Go archive URL does not name $GO_VERSION"
GO_ARCHIVE="$(fetch "$GO_URL" "$GO_SHA" "${GO_URL##*/}")"
GO_HOME="$CACHE/toolchain/$GO_VERSION-$HOST_PLATFORM-${GO_SHA:0:16}"
if [[ ! -f "$GO_HOME/.complete" ]]; then
    rm -rf "$GO_HOME"
    mkdir -p "$GO_HOME"
    if [[ "$IS_WINDOWS" == 1 ]]; then
        "$PY" -c 'import sys,zipfile; zipfile.ZipFile(sys.argv[1]).extractall(sys.argv[2])' "$(native "$GO_ARCHIVE")" "$(native "$GO_HOME")"
    else
        tar -xzf "$GO_ARCHIVE" -C "$GO_HOME"
    fi
    touch "$GO_HOME/.complete"
fi
GO="$GO_HOME/go/bin/go"
[[ "$IS_WINDOWS" == 1 ]] && GO="$GO.exe"

# A clean, explicit Go environment: nothing from the caller's go env or GOFLAGS leaks in.
while IFS= read -r name; do unset "$name"; done < <(env | sed -n 's/^\(GO[A-Z0-9_]*\)=.*/\1/p' | grep -v '^GOMAXPROCS$' || true)
unset CGO_CFLAGS CGO_LDFLAGS CC CXX
export GOENV=off GOTOOLCHAIN=local CGO_ENABLED=0 GOFLAGS=-mod=readonly
export GOPROXY="https://proxy.golang.org,direct" GOSUMDB="sum.golang.org"
export GOPATH="$(native "$CACHE/gopath")" GOMODCACHE="$(native "$CACHE/gomod")" GOCACHE="$(native "$CACHE/gocache")"
GO_SEEN="$("$GO" version | tr -d '\r')"
[[ "$GO_SEEN" == "go version $GO_VERSION ${HOST_PLATFORM%-*}/${HOST_PLATFORM#*-}" ]] || fail "unexpected toolchain: $GO_SEEN"

# --- source + patches ---------------------------------------------------------------------------
SOURCE_URL="$(m 'm["upstream"]["source"]["url"]' | tr -d '\r')"
SOURCE_SHA="$(m 'm["upstream"]["source"]["sha256"]' | tr -d '\r')"
SOURCE_PREFIX="$(m 'm["upstream"]["source"]["rootPrefix"]' | tr -d '\r')"
[[ "$SOURCE_URL" == *"$COMMIT"* && "$SOURCE_PREFIX" == *"$COMMIT/" ]] || fail "source archive is not pinned to the upstream commit"
SOURCE_ARCHIVE="$(fetch "$SOURCE_URL" "$SOURCE_SHA" "mihomo-$COMMIT.zip")"

WORK="$(mktemp -d "${TMPDIR:-/tmp}/multicore-mihomo.XXXXXX")"
trap 'rm -rf "$WORK"' EXIT
SOURCE_NAME="mihomo-$VERSION-$SERIES"
SRC="$WORK/$SOURCE_NAME"
"$PY" - "$(native "$SOURCE_ARCHIVE")" "$SOURCE_PREFIX" "$(native "$SRC")" <<'PY'
import os, stat, sys, zipfile
archive, prefix, dest = sys.argv[1:]
with zipfile.ZipFile(archive) as z:
    for info in z.infolist():
        name = info.filename
        if not name.startswith(prefix):
            sys.exit(f"source archive entry outside {prefix}: {name}")
        rel = name[len(prefix):]
        if not rel or rel.endswith("/"):
            continue
        parts = rel.split("/")
        if any(p in ("", ".", "..") for p in parts) or ":" in rel or "\\" in rel:
            sys.exit(f"unsafe source archive entry: {name}")
        if stat.S_ISLNK(info.external_attr >> 16):
            sys.exit(f"source archive contains a symlink: {name}")
        target = os.path.join(dest, *parts)
        os.makedirs(os.path.dirname(target), exist_ok=True)
        with z.open(info) as src, open(target, "xb") as out:
            out.write(src.read())
PY
# Its own repository, so `git apply` resolves paths against the source root and never against a
# repository the work directory happens to sit in.
git -C "$SRC" init -q
for p in "${LISTED[@]}"; do
    if ! git -C "$SRC" apply --check --whitespace=nowarn "$PATCH_DIR/$p" 2>"$WORK/apply.err"; then
        cat "$WORK/apply.err" >&2
        fail "patch $p does not apply to mihomo $VERSION ($COMMIT): rebase it before bumping the core"
    fi
    git -C "$SRC" apply --whitespace=nowarn "$PATCH_DIR/$p"
    echo "==> applied $p"
done
rm -rf "$SRC/.git"

cd "$SRC"
if [[ "${MIHOMO_SKIP_TESTS:-0}" != 1 ]]; then
    echo "==> go vet + go test of the patched packages"
    "$GO" vet ./component/tls/ ./config/ ./hub/executor/
    "$GO" test -count=1 ./component/tls/ ./config/
fi

BUILD_TIME="$(m 'm["build"]["buildTime"]' | tr -d '\r')"
TAGS="$(m 'm["build"]["tags"]' | tr -d '\r')"
LDFLAGS="-extldflags --static -X 'github.com/metacubex/mihomo/constant.Version=$VERSION' -X 'github.com/metacubex/mihomo/constant.BuildTime=$BUILD_TIME' -w -s -buildid="

mkdir -p "$OUTPUT"
OUTPUT="$(cd "$OUTPUT" && pwd)"
: > "$OUTPUT/SHA256SUMS"
for t in "${TARGETS[@]}"; do
    goos="$(m "m['targets']['$t']['goos']" | tr -d '\r')"
    goarch="$(m "m['targets']['$t']['goarch']" | tr -d '\r')"
    level="$(m "m['targets']['$t']['level']" | tr -d '\r')"
    file="$(m "m['targets']['$t']['file']" | tr -d '\r')"
    [[ "$file" =~ ^[A-Za-z0-9._-]+$ ]] || fail "target $t has an unsafe file name"
    echo "==> build $t ($goos/$goarch $level) -> $file"
    (
        export GOOS="$goos" GOARCH="$goarch"
        case "$goarch" in
            amd64) export GOAMD64="$level" ;;
            386) export GO386="$level" ;;
            arm64) export GOARM64="$level" ;;
            *) fail "no CPU level variable for $goarch" ;;
        esac
        "$GO" build -tags "$TAGS" -trimpath -ldflags "$LDFLAGS" -o "$(native "$OUTPUT/$file")" .
    )
    got="$(sha "$OUTPUT/$file")"
    echo "$got  $file" >> "$OUTPUT/SHA256SUMS"
    want="$(m "m['targets']['$t']['sha256']" | tr -d '\r')"
    if [[ "$UPDATE_PINS" != 1 && "$got" != "$want" ]]; then
        rm -f "$OUTPUT/$file"
        fail "$file: SHA-256 $got does not match the manifest pin '$want' (an unreproducible build or an unpinned change; never ship it)"
    fi
done
cd "$ROOT"

if [[ "$UPDATE_PINS" == 1 ]]; then
    "$PY" - "$(native "$MANIFEST")" "$(native "$OUTPUT/SHA256SUMS")" <<'PY'
import json, sys
path, sums = sys.argv[1:]
manifest = json.load(open(path, encoding="utf-8"))
by_file = {line.split()[1]: line.split()[0] for line in open(sums, encoding="utf-8") if line.strip()}
for target in manifest["targets"].values():
    target["sha256"] = by_file[target["file"]]
with open(path, "w", encoding="utf-8", newline="\n") as out:
    json.dump(manifest, out, indent=2, ensure_ascii=False)
    out.write("\n")
PY
    echo "==> wrote the built SHA-256 pins into packaging/mihomo-patches/manifest.json"
fi

# GPL corresponding source: the exact tree that was compiled, plus how it was made.
mkdir -p "$SRC/MULTICORE-BUILD/patches"
cp "$PATCH_DIR"/*.patch "$SRC/MULTICORE-BUILD/patches/"
cp "$PATCH_DIR/README.md" "$MANIFEST" "$SRC/MULTICORE-BUILD/"
cp "$ROOT/scripts/build-mihomo.sh" "$SRC/MULTICORE-BUILD/"
"$PY" - "$(native "$SRC")" "$SOURCE_NAME" "$(native "$OUTPUT/mihomo-source.zip")" <<'PY'
import os, sys, zipfile
root, prefix, out = sys.argv[1:]
files = []
for directory, dirs, names in os.walk(root):
    dirs.sort()
    for name in sorted(names):
        full = os.path.join(directory, name)
        files.append((os.path.relpath(full, root).replace(os.sep, "/"), full))
files.sort()
with zipfile.ZipFile(out, "x", zipfile.ZIP_DEFLATED, compresslevel=9) as z:
    for rel, full in files:
        info = zipfile.ZipInfo(f"{prefix}/{rel}", date_time=(1980, 1, 1, 0, 0, 0))
        info.external_attr = 0o644 << 16
        info.compress_type = zipfile.ZIP_DEFLATED
        with open(full, "rb") as src:
            z.writestr(info, src.read())
PY

{
    echo "series:    $SERIES"
    echo "upstream:  $(m 'm["upstream"]["project"]' | tr -d '\r') $VERSION $COMMIT"
    echo "source:    $SOURCE_URL"
    echo "           sha256 $SOURCE_SHA"
    echo "go:        $GO_SEEN ($GO_URL sha256 $GO_SHA)"
    echo "flags:     CGO_ENABLED=0 -tags $TAGS -trimpath"
    echo "ldflags:   $LDFLAGS"
    echo "patches:"
    for p in "${LISTED[@]}"; do echo "  $(sha "$PATCH_DIR/$p")  $p"; done
    echo "targets:"
    for t in "${TARGETS[@]}"; do
        echo "  $t: GOOS=$(m "m['targets']['$t']['goos']" | tr -d '\r') GOARCH=$(m "m['targets']['$t']['goarch']" | tr -d '\r') level=$(m "m['targets']['$t']['level']" | tr -d '\r')"
    done
} > "$OUTPUT/BUILDINFO.txt"

echo "==> mihomo $VERSION + ${#LISTED[@]} patches ($SERIES) built into $OUTPUT"
cat "$OUTPUT/SHA256SUMS"
