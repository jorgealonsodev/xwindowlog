#!/bin/sh
set -eu
umask 022

fail() {
    printf 'build-deb: %s\n' "$1" >&2
    exit 1
}

[ "$#" -eq 0 ] || fail 'usage: build-deb.sh'
script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
gtk4_dir=$(CDPATH= cd -- "$script_dir/.." && pwd)
dist_dir=$script_dir/dist
mkdir -p "$dist_dir"
cargo_home=${CARGO_HOME:-${HOME:?HOME must be set to remap build paths}/.cargo}
CARGO_ENCODED_RUSTFLAGS="--remap-path-prefix=$cargo_home=/usr/src/cargo" timeout 180 cargo build --manifest-path "$gtk4_dir/Cargo.toml" --release --locked --offline
binary=$gtk4_dir/target/release/xwindowlog-gtk4-spike
[ -f "$binary" ] || fail "release binary not found: $binary"
architecture=$(dpkg --print-architecture)
[ -n "$architecture" ] || fail 'dpkg returned an empty native architecture'
version=0.1.0-1+local
output=$dist_dir/xwindowlog-gtk4-spike_${version}_${architecture}.deb
[ ! -e "$output" ] || fail "refusing to overwrite existing package: $output"

work=$(mktemp -d "$dist_dir/.package-build.XXXXXX")
package_root=$work/debian/xwindowlog-gtk4-spike
elf=$package_root/usr/lib/xwindowlog-gtk4-spike/xwindowlog-gtk4-spike
mkdir -p "$work/debian" "$package_root/DEBIAN" "$(dirname -- "$elf")" \
    "$package_root/usr/bin" "$package_root/usr/share/applications"
cp "$binary" "$elf"
chmod 0755 "$elf"
cat > "$work/debian/control" <<'CONTROL'
Source: xwindowlog-gtk4-spike
Section: utils
Priority: optional
Maintainer: Local Prototype <noreply@example.invalid>

Package: xwindowlog-gtk4-spike
Architecture: any
Description: GTK4 desktop feasibility prototype
 A local package of the existing GTK4 UI spike.
CONTROL

relative_elf=debian/xwindowlog-gtk4-spike/usr/lib/xwindowlog-gtk4-spike/xwindowlog-gtk4-spike
depends_output=$(cd "$work" && LC_ALL=C dpkg-shlibdeps -O -e"$relative_elf") || fail 'dpkg-shlibdeps could not derive runtime dependencies'
case "$depends_output" in
    shlibs:Depends=*) depends=${depends_output#shlibs:Depends=} ;;
    *) fail 'dpkg-shlibdeps returned no shlibs:Depends metadata' ;;
esac
case "$depends" in
    ''|*'
'*) fail 'dpkg-shlibdeps returned empty or multiline dependencies' ;;
esac

cp "$script_dir/launcher" "$package_root/usr/bin/xwindowlog-gtk4-spike"
cp "$script_dir/xwindowlog-gtk4-spike.desktop" "$package_root/usr/share/applications/xwindowlog-gtk4-spike.desktop"
chmod 0755 "$package_root/usr/bin/xwindowlog-gtk4-spike"
chmod 0644 "$package_root/usr/share/applications/xwindowlog-gtk4-spike.desktop"
cat > "$package_root/DEBIAN/control" <<CONTROL
Package: xwindowlog-gtk4-spike
Version: $version
Section: utils
Priority: optional
Architecture: $architecture
Depends: $depends
Maintainer: Local Prototype <noreply@example.invalid>
Description: GTK4 desktop feasibility prototype
 A local package of the existing GTK4 UI spike.
CONTROL

dpkg-deb --build --root-owner-group "$package_root" "$output"
printf 'Package: %s\nArchitecture: %s\nVersion: %s\nDepends: %s\n' \
    "$output" "$architecture" "$version" "$depends"
