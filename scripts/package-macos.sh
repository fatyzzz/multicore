#!/usr/bin/env bash
# Builds MultiCore.app for Apple silicon from packaging/macos/versions.json.
#
# usage: scripts/package-macos.sh --output <new-dir> [--cache <download-dir>]
# env:   MULTICORE_MIHOMO_BINARIES_DIR  an earlier scripts/build-mihomo.sh output to reuse
#
# The bundle keeps the package layout the desktop validates (bootstrap.rs):
#   MultiCore.app/Contents/MacOS/{multicore, runtime/, cores/}
# The dashboard is in Contents/Resources/dashboard: a signed bundle allows only code in MacOS.
# Every download is SHA-256 pinned; the bundle is signed ad hoc (not notarized).
set -euo pipefail

usage() {
    echo "usage: $0 --output <new-dir> [--cache <download-dir>]" >&2
    exit 2
}

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OUTPUT=""
CACHE="$ROOT/target/macos-downloads"
while [[ $# -gt 0 ]]; do
    case "$1" in
        --output) [[ $# -ge 2 ]] || usage; OUTPUT="$2"; shift 2 ;;
        --cache) [[ $# -ge 2 ]] || usage; CACHE="$2"; shift 2 ;;
        *) usage ;;
    esac
done
[[ -n "$OUTPUT" ]] || usage
[[ ! -e "$OUTPUT" ]] || { echo "refusing to overwrite existing $OUTPUT" >&2; exit 1; }
[[ "$(uname -s)" == Darwin && "$(uname -m)" == arm64 ]] || {
    echo "MultiCore.app is built on an Apple silicon Mac" >&2
    exit 1
}

MANIFEST="$ROOT/packaging/macos/versions.json"
# patch_dashboard_csp.py needs Python 3.10+; the system python3 on macOS is 3.9.
PY=""
for candidate in python3 python3.14 python3.13 python3.12 python3.11 python3.10; do
    if command -v "$candidate" >/dev/null 2>&1 &&
        "$candidate" -c 'import sys; sys.exit(0 if sys.version_info >= (3, 10) else 1)' 2>/dev/null; then
        PY="$candidate"
        break
    fi
done
[[ -n "$PY" ]] || { echo "Python 3.10+ is required (brew install python)" >&2; exit 1; }
# field <python-expr over m>: one value from the manifest.
field() {
    "$PY" -c 'import json,sys; m=json.load(open(sys.argv[1])); print(eval(sys.argv[2]))' "$MANIFEST" "$1"
}
check_sha() { echo "$1  $2" | shasum -a 256 -c --status; }
# fetch <url> <sha256> <file>: download once into the cache; the bytes must match the pin.
fetch() {
    local url="$1" sha="$2" archive="$CACHE/$3"
    if [[ ! -f "$archive" ]] || ! check_sha "$sha" "$archive"; then
        curl -fsSL --proto '=https' --tlsv1.2 --retry 3 -o "$archive.part" "$url"
        mv "$archive.part" "$archive"
    fi
    check_sha "$sha" "$archive" || { echo "$3 does not match its pinned SHA-256" >&2; exit 1; }
    printf '%s' "$archive"
}

TOOLCHAIN="$(field 'm["build"]["rustToolchain"]')"
rustc --version | grep -q "^rustc $TOOLCHAIN " || {
    echo "rustc $TOOLCHAIN is required" >&2
    exit 1
}
mkdir -p "$CACHE"
CACHE="$(cd "$CACHE" && pwd)"
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$ROOT/target/macos-release}"

# Skia (Metal): skia-bindings links the pinned prebuilt archive from the cache.
SKIA_KEY="$(field 'm["build"]["skiaBinaries"]["key"]')"
fetch "$(field 'm["build"]["skiaBinaries"]["url"]')" "$(field 'm["build"]["skiaBinaries"]["sha256"]')" \
    "skia-binaries-$SKIA_KEY.tar.gz" >/dev/null
export SKIA_BINARIES_URL="file://$CACHE/skia-binaries-{key}.tar.gz"
unset FORCE_SKIA_BUILD FORCE_SKIA_BINARIES_DOWNLOAD SKIA_SOURCE_DIR SKIA_LIBRARY_SEARCH_PATH SKIA_DEBUG
export MACOSX_DEPLOYMENT_TARGET="$(field 'm["build"]["minimumSystemVersion"]')"
# Encoded (0x1f-separated) so a checkout path with spaces stays one flag.
unset RUSTFLAGS
CARGO_ENCODED_RUSTFLAGS="$(printf '%s\x1f%s\x1f%s' \
    "--remap-path-prefix=$ROOT=/source/multicore" \
    "--remap-path-prefix=${CARGO_HOME:-$HOME/.cargo}=/cargo" \
    "--remap-path-prefix=${RUSTUP_HOME:-$HOME/.rustup}=/rustup")"
export CARGO_ENCODED_RUSTFLAGS
cargo build --release --locked \
    --manifest-path "$ROOT/Cargo.toml" \
    -p multicore-desktop -p multicore-daemon -p multicore-core-host
linked_skia="$(ls -t "$CARGO_TARGET_DIR"/release/build/skia-bindings-*/out/skia/key.txt 2>/dev/null | head -n 1)"
[[ -n "$linked_skia" && "$(cat "$linked_skia")" == "$SKIA_KEY" ]] || {
    echo "the build did not link the pinned Skia binaries $SKIA_KEY" >&2
    exit 1
}

APP="$OUTPUT/MultiCore.app"
CONTENTS="$APP/Contents"
PACKAGE_ROOT="$CONTENTS/MacOS"
install -d -m 0755 "$PACKAGE_ROOT/runtime" "$PACKAGE_ROOT/cores" "$CONTENTS/Resources/licenses"

install -m 0755 "$CARGO_TARGET_DIR/release/multicore-desktop" "$PACKAGE_ROOT/multicore"
install -m 0755 "$CARGO_TARGET_DIR/release/multicore-daemon" "$PACKAGE_ROOT/runtime/multicore-daemon"
install -m 0755 "$CARGO_TARGET_DIR/release/multicore-core-host" "$PACKAGE_ROOT/runtime/multicore-core-host"
strip -x "$PACKAGE_ROOT/multicore" "$PACKAGE_ROOT/runtime/multicore-daemon" \
    "$PACKAGE_ROOT/runtime/multicore-core-host"

# Xray: the pinned XTLS release asset.
xray_archive="$(fetch "$(field 'm["components"]["xray"]["asset"]["url"]')" \
    "$(field 'm["components"]["xray"]["asset"]["sha256"]')" \
    "$(field 'm["components"]["xray"]["asset"]["name"]')")"
unzip -p "$xray_archive" "$(field 'm["components"]["xray"]["asset"]["entry"]')" > "$PACKAGE_ROOT/cores/xray"
check_sha "$(field 'm["components"]["xray"]["asset"]["executableSha256"]')" "$PACKAGE_ROOT/cores/xray" || {
    echo "xray does not match its pinned SHA-256" >&2
    exit 1
}
chmod 0755 "$PACKAGE_ROOT/cores/xray"

# Mihomo: never the stock download, always scripts/build-mihomo.sh (pinned source + patches).
mihomo_target="$(field 'm["components"]["mihomo"]["build"]["target"]')"
mihomo_file="$(field 'm["components"]["mihomo"]["build"]["file"]')"
mihomo_sha="$(field 'm["components"]["mihomo"]["build"]["executableSha256"]')"
mihomo_dir="${MULTICORE_MIHOMO_BINARIES_DIR:-}"
mihomo_scratch=""
if [[ -z "$mihomo_dir" ]]; then
    mihomo_scratch="$(mktemp -d "$CACHE/mihomo-build.XXXXXX")"
    mihomo_dir="$mihomo_scratch/out"
    "$ROOT/scripts/build-mihomo.sh" --cache "$CACHE/mihomo" --output "$mihomo_dir" "$mihomo_target"
fi
check_sha "$mihomo_sha" "$mihomo_dir/$mihomo_file" || {
    echo "the Mihomo binary in $mihomo_dir does not match its pinned SHA-256" >&2
    exit 1
}
install -m 0755 "$mihomo_dir/$mihomo_file" "$PACKAGE_ROOT/cores/mihomo"
[[ -z "$mihomo_scratch" ]] || rm -rf "$mihomo_scratch"

# Dashboard: the pinned zashboard build, CSP-patched like on Linux.
dashboard_archive="$(fetch "$(field 'm["components"]["dashboard"]["asset"]["url"]')" \
    "$(field 'm["components"]["dashboard"]["asset"]["sha256"]')" \
    "$(field 'm["components"]["dashboard"]["asset"]["name"]')")"
dashboard_root="$(field 'm["components"]["dashboard"]["asset"]["rootPrefix"]')"
extract="$(mktemp -d "$CACHE/dashboard-extract.XXXXXX")"
unzip -q "$dashboard_archive" -d "$extract"
[[ -f "$extract/${dashboard_root%/}/index.html" ]] || {
    echo "dashboard archive does not contain $dashboard_root/index.html" >&2
    exit 1
}
if find "$extract" -type l | grep -q .; then
    echo "dashboard archive contains a symlink" >&2
    exit 1
fi
mv "$extract/${dashboard_root%/}" "$CONTENTS/Resources/dashboard"
rm -rf "$extract"
"$PY" "$ROOT/scripts/patch_dashboard_csp.py" "$MANIFEST" "$CONTENTS/Resources/dashboard/index.html"
find "$CONTENTS/Resources/dashboard" -type d -exec chmod 0755 {} \;
find "$CONTENTS/Resources/dashboard" -type f -exec chmod 0644 {} \;
license="$(fetch "$(field 'm["components"]["dashboard"]["licenseFile"]["url"]')" \
    "$(field 'm["components"]["dashboard"]["licenseFile"]["sha256"]')" \
    "$(field 'm["components"]["dashboard"]["licenseFile"]["name"]')")"
install -m 0644 "$license" "$ROOT/LICENSE" "$ROOT/packaging/linux/THIRD_PARTY_NOTICES.md" \
    "$ROOT/packaging/windows-x64/licenses/Xray-core-MPL-2.0.txt" \
    "$ROOT/packaging/windows-x64/licenses/mihomo-GPL-3.0.txt" \
    "$CONTENTS/Resources/licenses/"

# App icon from the canonical SVG (Quick Look renders it; sips scales it).
iconset="$(mktemp -d "$CACHE/icon.XXXXXX")/MultiCore.iconset"
mkdir -p "$iconset"
qlmanage -t -s 1024 -o "$(dirname "$iconset")" "$ROOT/apps/multicore-desktop/assets/app-icon.svg" >/dev/null 2>&1
master="$(dirname "$iconset")/app-icon.svg.png"
if [[ -f "$master" ]]; then
    for size in 16 32 128 256 512; do
        sips -z "$size" "$size" "$master" --out "$iconset/icon_${size}x${size}.png" >/dev/null
        sips -z $((size * 2)) $((size * 2)) "$master" --out "$iconset/icon_${size}x${size}@2x.png" >/dev/null
    done
    iconutil -c icns "$iconset" -o "$CONTENTS/Resources/MultiCore.icns"
fi
rm -rf "$(dirname "$iconset")"

VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' "$ROOT/apps/multicore-desktop/Cargo.toml" | head -1)"
cat > "$CONTENTS/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleDevelopmentRegion</key><string>en</string>
    <key>CFBundleDisplayName</key><string>MultiCore</string>
    <key>CFBundleExecutable</key><string>multicore</string>
    <key>CFBundleIconFile</key><string>MultiCore</string>
    <key>CFBundleIdentifier</key><string>$(field 'm["build"]["bundleIdentifier"]')</string>
    <key>CFBundleInfoDictionaryVersion</key><string>6.0</string>
    <key>CFBundleName</key><string>MultiCore</string>
    <key>CFBundlePackageType</key><string>APPL</string>
    <key>CFBundleShortVersionString</key><string>$VERSION</string>
    <key>CFBundleVersion</key><string>$VERSION</string>
    <key>LSApplicationCategoryType</key><string>public.app-category.utilities</string>
    <key>LSMinimumSystemVersion</key><string>$MACOSX_DEPLOYMENT_TARGET</string>
    <key>NSHighResolutionCapable</key><true/>
</dict>
</plist>
PLIST

for binary in "$PACKAGE_ROOT/multicore" "$PACKAGE_ROOT/runtime/multicore-daemon" "$PACKAGE_ROOT/runtime/multicore-core-host"; do
    if grep -a -q -F -e "$ROOT/" -e "$HOME/" "$binary"; then
        echo "private build path found in $binary" >&2
        exit 1
    fi
done

# Ad-hoc signature: inner executables first, then the bundle.
for binary in "$PACKAGE_ROOT"/cores/* "$PACKAGE_ROOT"/runtime/*; do
    codesign --force --sign - --options runtime --timestamp=none "$binary"
done
codesign --force --sign - --options runtime --timestamp=none "$APP"
codesign --verify --strict "$APP"

echo "staged $APP"
