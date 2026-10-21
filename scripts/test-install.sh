#!/bin/sh
# Tests install.sh against fake releases served from a temp dir by python3 -m http.server.
set -eu

root=$(CDPATH='' cd -- "$(dirname "$0")/.." && pwd)
work=$(mktemp -d)
server=
cleanup() {
	if [ -n "$server" ]; then kill "$server" 2>/dev/null || true; fi
	rm -rf "$work"
}
trap cleanup EXIT
trap 'exit 1' HUP INT TERM
unset PI_VERSION PI_INSTALL_DIR PI_RELEASES_URL

fail() {
	printf 'FAIL: %s\n' "$*" >&2
	if [ -f "$work/out" ]; then sed 's/^/  | /' "$work/out" >&2; fi
	exit 1
}
pass() { printf 'ok: %s\n' "$*"; }
sha256() { if command -v sha256sum >/dev/null 2>&1; then sha256sum "$@"; else shasum -a 256 "$@"; fi; }

# A fake release <tag> whose pi prints "pi <version>"; with "corrupt", the archives no longer match SHA256SUMS.
release() {
	rel="$work/site/download/$1"
	mkdir -p "$rel"
	for target in x86_64-unknown-linux-musl aarch64-unknown-linux-musl armv7-unknown-linux-musleabihf \
		x86_64-apple-darwin aarch64-apple-darwin; do
		mkdir -p "$work/pkg/pi-$1-$target"
		printf '#!/bin/sh\necho "pi %s"\n' "${1#v}" >"$work/pkg/pi-$1-$target/pi"
		chmod 755 "$work/pkg/pi-$1-$target/pi"
		tar -C "$work/pkg" -czf "$rel/pi-$1-$target.tar.gz" "pi-$1-$target"
	done
	printf 'daemons\n' >"$rel/pi-env-daemons-$1.tar.gz"
	(cd "$rel" && sha256 pi-*.tar.gz >SHA256SUMS)
	if [ "${2:-}" = corrupt ]; then for file in "$rel"/pi-v*.tar.gz; do printf 'x' >>"$file"; done; fi
}
release v9.9.7 corrupt
release v9.9.8
release v9.9.9
mkdir -p "$work/site/latest/download"
cp "$work/site/download/v9.9.9/SHA256SUMS" "$work/site/latest/download/"

python3 -u -m http.server 0 --bind 127.0.0.1 --directory "$work/site" >"$work/server.log" 2>&1 &
server=$!
port=
tries=0
while [ -z "$port" ]; do
	tries=$((tries + 1))
	[ "$tries" -le 100 ] || fail "http.server did not start: $(cat "$work/server.log")"
	sleep 0.1
	port=$(sed -n 's/.* port \([0-9][0-9]*\).*/\1/p' "$work/server.log" | head -n 1)
done
url="http://127.0.0.1:$port"
mkdir -p "$work/tmp"

# run_install <shell> <dir> [NAME=value...]: run install.sh, output in $work/out.
run_install() {
	shell=$1 dir=$2
	shift 2
	env TMPDIR="$work/tmp" PI_RELEASES_URL="$url" PI_INSTALL_DIR="$dir" "$@" "$shell" "$root/install.sh" >"$work/out" 2>&1
}
expect_version() {
	got=$("$1/pi") || fail "$1/pi did not run"
	[ "$got" = "pi $2" ] || fail "$1/pi printed '$got', expected 'pi $2'"
}
expect_output() { grep -q "$1" "$work/out" || fail "output does not contain '$1'"; }

for shell in sh dash bash zsh; do
	command -v "$shell" >/dev/null 2>&1 || continue
	run_install "$shell" "$work/bin-$shell" || fail "$shell: install of the latest release failed"
	expect_version "$work/bin-$shell" 9.9.9
	[ -z "$(ls -A "$work/tmp")" ] || fail "$shell: temporary files were left in TMPDIR"
	pass "$shell installs the latest release"
done

run_install sh "$work/bin" PI_VERSION=9.9.8 PI_RELEASES_URL="$url/" || fail "install of PI_VERSION=9.9.8 failed"
expect_version "$work/bin" 9.9.8
run_install sh "$work/bin" PI_VERSION=v9.9.9 || fail "upgrade to PI_VERSION=v9.9.9 failed"
expect_version "$work/bin" 9.9.9
pass "PI_VERSION selects a release and replaces an existing pi"

if run_install sh "$work/bad" PI_VERSION=9.9.7; then fail "an archive with a bad checksum was installed"; fi
expect_output "checksum mismatch"
[ ! -e "$work/bad/pi" ] || fail "pi was installed despite the bad checksum"
pass "a bad checksum fails"

if run_install sh "$work/missing" PI_VERSION=1.0.0; then fail "a missing release did not fail"; fi
expect_output "could not download"
pass "a missing release fails"

mkdir -p "$work/other"
cp "$work/bin/pi" "$work/other/pi"
run_install sh "$work/bin" PATH="$work/other:$work/bin:$PATH" || fail "install with another pi on PATH failed"
expect_output "another pi comes first on PATH"
run_install sh "$work/bin" PATH="/usr/bin:/bin" || fail "install with the directory off PATH failed"
expect_output "is not on your PATH"
pass "PATH hints"
