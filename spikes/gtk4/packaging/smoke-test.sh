#!/bin/sh
set -eu

fail() {
    printf 'smoke-test: %s\n' "$1" >&2
    exit 1
}

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
dist_dir=$script_dir/dist
if [ "$#" -gt 1 ]; then
    fail 'usage: smoke-test.sh [package.deb]'
fi
if [ "$#" -eq 0 ]; then
    set -- "$dist_dir"/xwindowlog-gtk4-spike_0.1.0-1+local_*.deb
    [ "$#" -eq 1 ] && [ -f "$1" ] || fail 'expected one built package in packaging/dist'
fi
package=$1
case "$package" in
    /*) ;;
    *) package=$PWD/$package ;;
esac
[ -f "$package" ] || fail "package not found: $package"

package_name=$(dpkg-deb --field "$package" Package)
version=$(dpkg-deb --field "$package" Version)
architecture=$(dpkg-deb --field "$package" Architecture)
depends=$(dpkg-deb --field "$package" Depends)
[ "$package_name" = xwindowlog-gtk4-spike ] || fail "unexpected package name: $package_name"
[ "$version" = 0.1.0-1+local ] || fail "unexpected version: $version"
[ "$architecture" = "$(dpkg --print-architecture)" ] || fail "non-native package architecture: $architecture"
[ -n "$depends" ] || fail 'shlibs-derived Depends is empty'
case "$depends" in
    *xwindowlog*) fail 'the GTK UI must not require the xwindowlog CLI package' ;;
esac

test_dir=$dist_dir/.smoke-test-$$
mkdir "$test_dir"
root=$test_dir/root
dpkg-deb --extract "$package" "$root"
contents=$(dpkg-deb --contents "$package")
printf '%s\n' "$contents" | awk '$2 != "root/root" { exit 1 }' || fail 'archive entries are not root:root'
files=$(find "$root" -type f -printf '%P\n' | LC_ALL=C sort)
expected_files=$(printf '%s\n' \
    usr/bin/xwindowlog-gtk4-spike \
    usr/lib/xwindowlog-gtk4-spike/xwindowlog-gtk4-spike \
    usr/share/applications/xwindowlog-gtk4-spike.desktop)
[ "$files" = "$expected_files" ] || fail "unexpected package payload:\n$files"
for entry in \
    ./usr/bin/xwindowlog-gtk4-spike \
    ./usr/lib/xwindowlog-gtk4-spike/xwindowlog-gtk4-spike \
    ./usr/share/applications/xwindowlog-gtk4-spike.desktop; do
    printf '%s\n' "$contents" | grep -Fq "$entry" || fail "archive is missing $entry"
done
[ -x "$root/usr/bin/xwindowlog-gtk4-spike" ] || fail 'launcher is not executable'
[ -x "$root/usr/lib/xwindowlog-gtk4-spike/xwindowlog-gtk4-spike" ] || fail 'GTK binary is not executable'
if [ -n "${HOME:-}" ] && grep -aFq "$HOME" "$root/usr/lib/xwindowlog-gtk4-spike/xwindowlog-gtk4-spike"; then fail 'binary contains the build user HOME path'; fi
[ ! -x "$root/usr/share/applications/xwindowlog-gtk4-spike.desktop" ] || fail 'desktop entry is executable'
desktop=$root/usr/share/applications/xwindowlog-gtk4-spike.desktop
desktop-file-validate "$desktop"
grep -Fxq 'Type=Application' "$desktop" || fail 'desktop entry is not an application'
grep -Fxq 'Terminal=false' "$desktop" || fail 'desktop entry requests a terminal'
grep -Fxq 'Name=xwindowlog GTK4 Prototype' "$desktop" || fail 'desktop name does not identify the prototype'
grep -Fxq 'Name[es]=Prototipo de xwindowlog GTK4' "$desktop" || fail 'Spanish desktop name is missing'
grep -Fxq 'Exec=/usr/bin/xwindowlog-gtk4-spike' "$desktop" || fail 'desktop entry has the wrong launcher'

control=$test_dir/control
dpkg-deb --control "$package" "$control"
control_files=$(find "$control" -type f -printf '%P\n' | LC_ALL=C sort)
[ "$control_files" = control ] || fail "unexpected maintainer script or control payload: $control_files"

mkdir -p "$test_dir/home with spaces/.cargo/bin" "$test_dir/path only"
stub_ui=$test_dir/ui-stub
cat > "$stub_ui" <<'STUB'
#!/bin/sh
{
    printf '%s\n' "$#"
    for arg do printf 'ARG:%s\n' "$arg"; done
} > "$CAPTURE_FILE"
STUB
chmod 0755 "$stub_ui"
system_cli=$test_dir/system-cli
user_cli=$test_dir/home\ with\ spaces/.cargo/bin/xwindowlog
printf '#!/bin/sh\nexit 0\n' > "$system_cli"
printf '#!/bin/sh\nexit 0\n' > "$user_cli"
printf '#!/bin/sh\nexit 0\n' > "$test_dir/path only/xwindowlog"
chmod 0755 "$system_cli" "$user_cli" "$test_dir/path only/xwindowlog"

escape_sed_replacement() {
    printf '%s' "$1" | sed 's/[\\&|]/\\&/g'
}
make_launcher() {
    output=$1
    system_path=$2
    escaped_ui=$(escape_sed_replacement "$stub_ui")
    escaped_system=$(escape_sed_replacement "$system_path")
    sed -e "s|/usr/lib/xwindowlog-gtk4-spike/xwindowlog-gtk4-spike|$escaped_ui|g" \
        -e "s|/usr/bin/xwindowlog|$escaped_system|g" \
        "$root/usr/bin/xwindowlog-gtk4-spike" > "$output"
    chmod 0755 "$output"
}
make_launcher "$test_dir/system launcher" "$system_cli"
make_launcher "$test_dir/user launcher" "$test_dir/missing-system-bin/xwindowlog"
make_launcher "$test_dir/no-cli launcher" "$test_dir/missing-system-bin/xwindowlog"

capture=$test_dir/capture
home=$test_dir/home\ with\ spaces
CAPTURE_FILE=$capture HOME="$home" PATH="$test_dir/path only" "$test_dir/system launcher"
actual=$(sed -n '1,$p' "$capture")
expected=$(printf '%s\n' 2 ARG:--cli "ARG:$system_cli")
[ "$actual" = "$expected" ] || fail "system CLI was not preferred with exact --cli argv:\n$actual"
CAPTURE_FILE=$capture HOME="$home" PATH="$test_dir/path only" "$test_dir/user launcher"
actual=$(sed -n '1,$p' "$capture")
expected=$(printf '%s\n' 2 ARG:--cli "ARG:$user_cli")
[ "$actual" = "$expected" ] || fail "user CLI path with spaces was not passed exactly:\n$actual"

sentinel=$test_dir/evaluated-argument
payload=$(printf '%s' '$(' "touch \"$sentinel\"" ')')
CAPTURE_FILE=$capture HOME="$test_dir/empty home" PATH="$test_dir/path only" \
    "$test_dir/no-cli launcher" 'forwarded argument with spaces' "$payload"
actual=$(sed -n '1,$p' "$capture")
expected=$(printf '%s\n' 2 'ARG:forwarded argument with spaces' "ARG:$payload")
[ "$actual" = "$expected" ] || fail "no-CLI launch did not omit --cli and forward arguments literally:\n$actual"
[ ! -e "$sentinel" ] || fail 'launcher evaluated a forwarded argument'

printf 'smoke-test: PASS (%s; Depends: %s)\n' "$package_name $version $architecture" "$depends"
