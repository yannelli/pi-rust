#!/bin/sh
# Install pi (pi-rust) on Linux or macOS: curl -fsSL https://raw.githubusercontent.com/yannelli/pi-rust/main/install.sh | sh
# Settings: PI_VERSION (default: latest), PI_INSTALL_DIR (default: ~/.local/bin), PI_RELEASES_URL.

set -eu

REPO_URL=https://github.com/yannelli/pi-rust

say() { printf '%s\n' "$*"; }
warn() { printf 'warning: %s\n' "$*" >&2; }
die() {
	printf 'error: %s\n' "$*" >&2
	exit 1
}

has() { command -v "$1" >/dev/null 2>&1; }

download() {
	if has curl; then
		case "$1" in
		https://*) curl --proto '=https' --tlsv1.2 -fsSL --retry 3 -o "$2" "$1" ;;
		*) curl -fsSL --retry 3 -o "$2" "$1" ;;
		esac
	else
		wget -q -O "$2" "$1"
	fi
}

sha256() {
	if has sha256sum; then
		sha256sum "$1" | cut -d ' ' -f 1
	else
		shasum -a 256 "$1" | cut -d ' ' -f 1
	fi
}

detect_target() {
	os=$(uname -s)
	arch=$(uname -m)
	case "$os" in
	Linux) os=unknown-linux-musl ;;
	Darwin)
		os=apple-darwin
		# Under Rosetta, uname reports x86_64 on Apple silicon.
		if [ "$arch" = x86_64 ] && [ "$(sysctl -n sysctl.proc_translated 2>/dev/null || true)" = 1 ]; then
			arch=arm64
		fi
		;;
	MINGW* | MSYS* | CYGWIN* | Windows_NT)
		die "on Windows, run this in PowerShell: irm https://raw.githubusercontent.com/yannelli/pi-rust/main/install.ps1 | iex"
		;;
	*) die "no pi release for $os. Build from source: cargo install --locked --git $REPO_URL pi-coding-agent" ;;
	esac
	case "$arch" in
	x86_64 | amd64) arch=x86_64 ;;
	aarch64 | arm64) arch=aarch64 ;;
	armv7* | armv8l) arch=armv7 ;;
	*) die "no pi release for $arch on $(uname -s). Build from source: cargo install --locked --git $REPO_URL pi-coding-agent" ;;
	esac
	case "$arch-$os" in
	armv7-unknown-linux-musl) target=armv7-unknown-linux-musleabihf ;;
	armv7-*) die "no pi release for $arch on $(uname -s)" ;;
	*) target="$arch-$os" ;;
	esac
}

# Prints the SHA256SUMS entry named $2 (or, with $2 empty, the pi archive for $target) in file $1.
sums_entry() {
	awk -v want="$2" -v suffix="-$target.tar.gz" '{
		name = $2; sub(/^\*/, "", name); sub(/\r$/, "", name)
		if (want != "" ? name == want : (index(name, "pi-v") == 1 && substr(name, length(name) - length(suffix) + 1) == suffix)) {
			print tolower($1) " " name; exit
		}
	}' "$1"
}

path_hint() {
	say ""
	say "$dir is not on your PATH. Add it, then open a new terminal:"
	case "${SHELL:-}" in
	*/zsh) say "  echo 'export PATH=\"$dir:\$PATH\"' >> ~/.zshrc" ;;
	*/bash) say "  echo 'export PATH=\"$dir:\$PATH\"' >> ~/.bashrc" ;;
	*/fish) say "  fish_add_path '$dir'" ;;
	*) say "  echo 'export PATH=\"$dir:\$PATH\"' >> ~/.profile" ;;
	esac
}

main() {
	has curl || has wget || die "curl or wget is required"
	has sha256sum || has shasum || die "sha256sum or shasum is required to verify the download"
	has tar || die "tar is required"
	detect_target

	base=${PI_RELEASES_URL:-$REPO_URL/releases}
	base=${base%/}
	version=${PI_VERSION:-latest}
	dir=${PI_INSTALL_DIR:-}
	if [ -z "$dir" ]; then
		[ -n "${HOME:-}" ] || die "HOME is not set; set PI_INSTALL_DIR"
		dir="$HOME/.local/bin"
	fi
	mkdir -p "$dir" 2>/dev/null || die "could not create $dir. Set PI_INSTALL_DIR to a directory you can write to"
	dir=$(CDPATH='' cd -- "$dir" && pwd)
	[ -w "$dir" ] || die "$dir is not writable. Set PI_INSTALL_DIR to a directory you can write to (no sudo needed)"

	tmp=$(mktemp -d 2>/dev/null || mktemp -d -t pi-install) || die "could not create a temporary directory"
	if [ "$version" = latest ]; then
		sums_url="$base/latest/download/SHA256SUMS"
		wanted=
	else
		tag="v${version#v}"
		sums_url="$base/download/$tag/SHA256SUMS"
		wanted="pi-$tag-$target.tar.gz"
	fi
	download "$sums_url" "$tmp/SHA256SUMS" ||
		die "could not download $sums_url. Check PI_VERSION and the releases at $REPO_URL/releases"
	entry=$(sums_entry "$tmp/SHA256SUMS" "$wanted")
	[ -n "$entry" ] || die "the release has no pi archive for $target (SHA256SUMS: $sums_url)"
	expected=${entry%% *}
	archive=${entry#* }
	tag=${archive#pi-}
	tag=${tag%"-$target.tar.gz"}

	say "Downloading pi $tag for $target"
	download "$base/download/$tag/$archive" "$tmp/$archive" || die "could not download $base/download/$tag/$archive"
	actual=$(sha256 "$tmp/$archive")
	[ "$actual" = "$expected" ] || die "checksum mismatch for $archive: expected $expected, got $actual"
	mkdir "$tmp/unpacked"
	tar -xzf "$tmp/$archive" -C "$tmp/unpacked" || die "could not extract $archive"
	bin="$tmp/unpacked/pi-$tag-$target/pi"
	[ -f "$bin" ] || die "$archive does not contain pi-$tag-$target/pi"

	staged="$dir/.pi.install-$$"
	if ! { cp "$bin" "$staged" && chmod 755 "$staged" && mv -f "$staged" "$dir/pi"; }; then
		rm -f "$staged"
		die "could not install pi to $dir"
	fi
	say "Installed pi $tag to $dir/pi"

	# pi update --self refuses to replace a binary in the cargo bin directory.
	cargo_bin="${CARGO_HOME:-${HOME:-}/.cargo}/bin"
	if [ -d "$cargo_bin" ] && [ "$(CDPATH='' cd -- "$cargo_bin" && pwd -P)" = "$(CDPATH='' cd -- "$dir" && pwd -P)" ]; then
		warn "$dir is the cargo bin directory, so 'pi update --self' will not update this pi"
	fi

	found=$(command -v pi 2>/dev/null || true)
	case ":${PATH:-}:" in
	*":$dir:"* | *":$dir/:"*)
		if [ -n "$found" ] && [ "$found" != "$dir/pi" ]; then
			warn "another pi comes first on PATH: $found (for example the npm package). Remove it, or put $dir before $(dirname "$found") on PATH"
		fi
		;;
	*)
		[ -z "$found" ] || warn "the pi at $found runs instead of this one until $dir comes first on PATH"
		path_hint
		;;
	esac
	say "Update later with: pi update --self"
}

tmp=
cleanup() {
	if [ -n "$tmp" ]; then rm -rf "$tmp"; fi
}
trap cleanup EXIT
trap 'exit 1' HUP INT TERM

main "$@"
