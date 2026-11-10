#!/bin/sh
# Builds the static Linux release (musl) and packs it as
# packages/GitHubTurbo-<version>-linux-<arch>.tar.gz with a .sha256 beside it.
# Run on Linux with Rust, the musl target of each architecture, perl, make and
# python3; musl-tools for a native build, cargo-zigbuild and zig for a cross build.
#   --arch x64|arm64   architecture to build (default: x64)
#   --skip-tests       build without running the test suite first
#   --overwrite        replace an existing package of this version (never a published one)
# The tests run on the build machine. For a cross build, also run the tests on the
# target: cargo zigbuild --tests --release --locked --target <target>, then copy
# target/<target>/release/deps/github_turbo-* there and run it.
set -eu
root=$(cd "$(dirname "$0")/.." && pwd)
cd "$root"
arch=x64
skip_tests=0
overwrite=0
while [ $# -gt 0 ]; do
    case "$1" in
        --arch) arch=${2:-}; shift ;;
        --skip-tests) skip_tests=1 ;;
        --overwrite) overwrite=1 ;;
        *) echo "unknown option: $1" >&2; exit 2 ;;
    esac
    shift
done
case "$arch" in
    x64) target=x86_64-unknown-linux-musl; machine=x86_64 ;;
    arm64) target=aarch64-unknown-linux-musl; machine=aarch64 ;;
    *) echo "--arch must be x64 or arm64" >&2; exit 2 ;;
esac
# A native build uses musl-gcc; another architecture needs zig as the C cross compiler.
if [ "$(uname -m)" = "$machine" ]; then build="cargo build"; else build="cargo zigbuild"; fi
version=$(sed -n 's/^version = "\([^"]*\)".*/\1/p' Cargo.toml | head -n 1 | tr -d '\r')
name="GitHubTurbo-$version-linux-$arch"
archive="$root/packages/$name.tar.gz"
# A published package must never change under the same name. Check before building.
if [ -e "$archive" ] && [ "$overwrite" = 0 ]; then
    echo "$archive already exists. Bump the version in Cargo.toml, or pass --overwrite if this version was never published." >&2
    exit 1
fi
if [ "$skip_tests" = 0 ]; then
    cargo test --locked
fi
$build --release --locked --target "$target"

# Start from an empty stage so files from an earlier run cannot slip in.
stage="$root/packages/staging/$name"
rm -rf "$stage"
mkdir -p "$stage/docs"
install -m 755 "target/$target/release/github-turbo" "$stage/github-turbo"
# The checked-in default configuration, never the build machine's settings.
cp config.json README.md README_zh.md LICENSE "$stage/"
cp -R third-party "$stage/"
cp docs/*.md "$stage/docs/"

# Notices for every crate built into the Linux binary, plus the C libraries
# linked statically: OpenSSL (through openssl-src) and musl.
cargo metadata --format-version 1 --locked --filter-platform "$target" > "$stage/metadata.tmp"
python3 - "$stage" "$root/assets/crate-licenses" <<'PY'
import json, os, re, shutil, sys
stage, supplied = sys.argv[1], sys.argv[2]
with open(os.path.join(stage, "metadata.tmp"), encoding="utf-8") as f:
    meta = json.load(f)
os.remove(os.path.join(stage, "metadata.tmp"))
licenses = os.path.join(stage, "licenses")
inventory, missing = [], []
for package in meta["packages"]:
    if package["name"] == "github-turbo":
        continue
    ident = f'{package["name"]}-{package["version"]}'
    out = os.path.join(licenses, ident)
    os.makedirs(out, exist_ok=True)
    src = os.path.dirname(package["manifest_path"])
    for entry in os.listdir(src):
        path = os.path.join(src, entry)
        if os.path.isfile(path) and re.match(r"^(LICENSE|COPYING|NOTICE|COPYRIGHT)", entry):
            shutil.copy(path, out)
    if package["name"] == "openssl-src":
        shutil.copy(os.path.join(src, "openssl", "LICENSE.txt"), os.path.join(out, "OPENSSL-LICENSE.txt"))
    # Some published crates omit their license files; assets/crate-licenses holds
    # the upstream texts for those.
    if not os.listdir(out) and os.path.isdir(os.path.join(supplied, ident)):
        for entry in os.listdir(os.path.join(supplied, ident)):
            shutil.copy(os.path.join(supplied, ident, entry), out)
    if not os.listdir(out):
        missing.append(ident)
    inventory.append({k: package[k] for k in ("name", "version", "license", "repository")})
musl = "/usr/share/doc/musl/copyright"
if os.path.isfile(musl):
    os.makedirs(os.path.join(licenses, "musl"), exist_ok=True)
    shutil.copy(musl, os.path.join(licenses, "musl", "COPYRIGHT"))
    inventory.append({"name": "musl", "version": "", "license": "MIT", "repository": "https://musl.libc.org/"})
else:
    missing.append("musl (install musl-tools)")
if missing:
    sys.exit("No license text for: " + ", ".join(missing) + ". Add the upstream files under assets/crate-licenses/<name>-<version>.")
with open(os.path.join(stage, "dependencies.json"), "w", encoding="utf-8") as f:
    json.dump(inventory, f, indent=2)
PY

# Normalised owner and permissions, so the archive does not carry build-machine accounts.
mkdir -p "$root/packages"
rm -f "$archive"
tar -C "$root/packages/staging" --owner=0 --group=0 --numeric-owner -czf "$archive" "$name"
listing=$(tar -tzf "$archive")
if printf '%s\n' "$listing" | grep -Eq '(^|/)(state|reports|\.git)/|ca-key\.pem|server-key\.pem|session\.json'; then
    echo "Private machine state was included in the archive" >&2
    exit 1
fi
for required in github-turbo config.json README.md README_zh.md LICENSE dependencies.json; do
    printf '%s\n' "$listing" | grep -qx "$name/$required" || { echo "Incomplete archive: $required is missing" >&2; exit 1; }
done
digest=$(sha256sum "$archive" | cut -d' ' -f1)
printf '%s  %s\n' "$digest" "$name.tar.gz" > "$archive.sha256"
ls -l "$archive"
echo "SHA256 $digest"
