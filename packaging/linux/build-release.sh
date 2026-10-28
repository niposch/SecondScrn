#!/usr/bin/env bash
# Builds the Linux release: a .deb (Debian, Ubuntu, KDE neon, …) and a
# tar.gz with an install script (Fedora, Arch, openSUSE, …).
#
#   packaging/linux/build-release.sh            # -> releases/linux/
#
# Build on the oldest distribution you support: the binaries need its glibc
# or newer, and GTK >= 4.14 / libadwaita >= 1.5 at runtime (Ubuntu 24.04 is
# the reference). The version comes from the workspace Cargo.toml.
set -euo pipefail

root="$(cd "$(dirname "$0")/../.." && pwd)"
here="$root/packaging/linux"
out="$root/releases/linux"
target="${CARGO_TARGET_DIR:-$root/target}"
version="$(tr -d '\r' < "$root/Cargo.toml" | sed -n 's/^version = "\(.*\)"/\1/p' | head -1)"
arch="$(dpkg --print-architecture 2>/dev/null || echo amd64)"
app_id="dev.secondscrn.SecondScrn"

echo "SecondScrn $version for Linux ($arch)"
(cd "$root" && cargo build --release --locked -p secondscrn-linux-app -p secondscrn-linux-host)

stage="$(mktemp -d)"
trap 'rm -rf "$stage"' EXIT
tree="$stage/tree"
install -Dm755 "$target/release/secondscrn" "$tree/usr/bin/secondscrn"
install -Dm755 "$target/release/secondscrn-linux-cli" "$tree/usr/bin/secondscrn-cli"
install -Dm644 "$here/$app_id.desktop" "$tree/usr/share/applications/$app_id.desktop"
install -Dm644 "$here/$app_id.metainfo.xml" "$tree/usr/share/metainfo/$app_id.metainfo.xml"
install -Dm644 "$root/apps/windows/SecondScrn.App/Assets/SecondScrn.png" \
    "$tree/usr/share/icons/hicolor/256x256/apps/$app_id.png"
install -Dm644 "$here/70-secondscrn-uinput.rules" "$tree/usr/lib/udev/rules.d/70-secondscrn-uinput.rules"
install -Dm644 "$root/LICENSE" "$tree/usr/share/doc/secondscrn/copyright"
install -Dm644 "$root/THIRD_PARTY_NOTICES.md" "$tree/usr/share/doc/secondscrn/THIRD_PARTY_NOTICES.md"
for f in RUST-NOTICES.txt UPSTREAM-NOTICES.txt; do
    install -Dm644 "$root/licenses/$f" "$tree/usr/share/doc/secondscrn/licenses/$f"
done
# Line tables stay in target/ for symbolising crash reports; not shipped.
strip --strip-debug "$tree/usr/bin/secondscrn" "$tree/usr/bin/secondscrn-cli"

mkdir -p "$out"

# ---- .deb ---------------------------------------------------------------------
deb="$stage/deb"
cp -a "$tree" "$deb"
mkdir -p "$deb/DEBIAN"
size="$(du -sk "$deb/usr" | cut -f1)"
sed -e "s/@VERSION@/$version/" -e "s/@ARCH@/$arch/" -e "s/@SIZE@/$size/" "$here/debian/control" > "$deb/DEBIAN/control"
install -m755 "$here/debian/postinst" "$deb/DEBIAN/postinst"
install -m755 "$here/debian/postrm" "$deb/DEBIAN/postrm"
fakeroot dpkg-deb --build -Zxz "$deb" "$out/secondscrn_${version}_${arch}.deb" >/dev/null
echo "  $out/secondscrn_${version}_${arch}.deb"

# ---- tar.gz -------------------------------------------------------------------
name="SecondScrn-${version}-linux-$(uname -m)"
tgz="$stage/$name"
mkdir -p "$tgz"
cp -a "$tree/usr" "$tgz/"
install -m755 "$here/install.sh" "$tgz/install.sh"
cp "$here/README-tarball.txt" "$tgz/README.txt"
tar -C "$stage" -czf "$out/$name.tar.gz" "$name"
echo "  $out/$name.tar.gz"

(cd "$out" && sha256sum "secondscrn_${version}_${arch}.deb" "$name.tar.gz" > "SHA256SUMS-linux-$version.txt")
