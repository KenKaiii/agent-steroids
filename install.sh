#!/bin/sh
# Agent Steroids installer.
#
#   curl -fsSL https://raw.githubusercontent.com/KenKaiii/agent-steroids/main/install.sh | sh
#
# Downloads the release binary for this machine, checks it against the
# SHA256SUMS published with the release, and puts it on your PATH. No Rust
# toolchain needed. From then on `steroids upgrade` keeps it current.
#
# Environment:
#   STEROIDS_VERSION  pin a release, e.g. v0.3.1 (default: latest)
#   INSTALL_DIR       where to put the binary (default: ~/.local/bin, or
#                     ~/.cargo/bin when that already holds a steroids)
#   GITHUB_TOKEN      optional, only to avoid GitHub API rate limits

set -eu

REPO="KenKaiii/agent-steroids"
API="https://api.github.com/repos/${REPO}/releases"
DL="https://github.com/${REPO}/releases/download"
BIN="steroids"
TMP=""

say() { printf '%s\n' "$*"; }
step() { printf '  %-12s %s\n' "$1" "$2"; }
die() {
	printf '\nsteroids install: %s\n' "$1" >&2
	[ $# -gt 1 ] && printf '%s\n' "$2" >&2
	exit 1
}
have() { command -v "$1" >/dev/null 2>&1; }

cleanup() { [ -n "$TMP" ] && rm -rf "$TMP"; }
trap cleanup EXIT INT TERM HUP

fetch() {
	# fetch URL DEST. Fails rather than exits so callers can word the error.
	if have curl; then
		if [ -n "${GITHUB_TOKEN:-}" ] && [ "${1#https://api.github.com/}" != "$1" ]; then
			curl -fsSL --retry 3 --retry-delay 1 -H "Authorization: Bearer ${GITHUB_TOKEN}" -o "$2" "$1"
		else
			curl -fsSL --retry 3 --retry-delay 1 -o "$2" "$1"
		fi
	elif have wget; then
		wget -q --tries=3 -O "$2" "$1"
	else
		die "neither curl nor wget is installed" \
			"Install one, or download by hand from https://github.com/${REPO}/releases"
	fi
}

# --- platform: must spell the target exactly as release.yml names assets ----

detect_target() {
	os="$(uname -s 2>/dev/null || echo unknown)"
	arch="$(uname -m 2>/dev/null || echo unknown)"
	case "$arch" in
		x86_64 | amd64) arch="x86_64" ;;
		aarch64 | arm64) arch="aarch64" ;;
		*) die "unsupported CPU architecture: ${arch}" \
			"Releases cover x86_64 and aarch64. Build from source: cargo install --git https://github.com/${REPO}" ;;
	esac
	case "$os" in
		Linux) TARGET="${arch}-unknown-linux-musl" ;;
		Darwin) TARGET="${arch}-apple-darwin" ;;
		MINGW* | MSYS* | CYGWIN*) die "this is the Unix installer" \
			"On Windows run in PowerShell: irm https://raw.githubusercontent.com/${REPO}/main/install.ps1 | iex" ;;
		*) die "unsupported operating system: ${os}" \
			"Build from source: cargo install --git https://github.com/${REPO}" ;;
	esac
}

# --- version -----------------------------------------------------------------

resolve_version() {
	if [ -n "${STEROIDS_VERSION:-}" ]; then
		VERSION="$STEROIDS_VERSION"
		case "$VERSION" in v*) ;; *) VERSION="v${VERSION}" ;; esac
	else
		fetch "${API}/latest" "${TMP}/latest.json" 2>/dev/null || true
		VERSION="$(sed -n 's/.*"tag_name"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' "${TMP}/latest.json" 2>/dev/null | head -n 1)"
		[ -n "$VERSION" ] || die "could not work out the latest release" \
			"GitHub may be rate limiting you. Pin one: STEROIDS_VERSION=v0.3.1 sh install.sh"
	fi
	# A tag is vX.Y.Z and nothing else, whether it came from the API or the
	# environment; anything odd stops here rather than becoming part of a URL.
	case "$VERSION" in
		v[0-9]*.[0-9]*.[0-9]*) ;;
		*) die "unexpected release tag '${VERSION}'" "Tags look like v0.3.1." ;;
	esac
	# The glob above accepts trailing junk; everything after the v must be
	# digits and dots.
	case "${VERSION#v}" in
		*[!0-9.]*) die "unexpected release tag '${VERSION}'" "Tags look like v0.3.1." ;;
	esac
}

# --- checksum ----------------------------------------------------------------

sha256_of() {
	if have sha256sum; then sha256sum "$1" | awk '{print $1}'
	elif have shasum; then shasum -a 256 "$1" | awk '{print $1}'
	elif have openssl; then openssl dgst -sha256 "$1" | awk '{print $NF}'
	else return 1
	fi
}

# --- install dir -------------------------------------------------------------

choose_dir() {
	if [ -n "${INSTALL_DIR:-}" ]; then
		DIR="$INSTALL_DIR"
	elif [ -x "${HOME}/.cargo/bin/${BIN}" ]; then
		# A cargo install is already on PATH; replace it rather than shadow it.
		DIR="${HOME}/.cargo/bin"
	else
		DIR="${HOME}/.local/bin"
	fi
	mkdir -p "$DIR" 2>/dev/null || die "cannot create ${DIR}" \
		"Pick somewhere writable: INSTALL_DIR=\$HOME/bin sh install.sh"
	[ -w "$DIR" ] || die "cannot write to ${DIR}" \
		"Pick somewhere writable: INSTALL_DIR=\$HOME/bin sh install.sh"
}

on_path() {
	case ":${PATH}:" in *":$1:"*) return 0 ;; *) return 1 ;; esac
}

# --- main --------------------------------------------------------------------

say "Agent Steroids installer"
have tar || die "tar is required"
TMP="$(mktemp -d 2>/dev/null || mktemp -d -t steroids)" || die "could not create a temporary directory"

detect_target
step "platform" "$TARGET"
resolve_version
step "release" "$VERSION"
choose_dir
step "install to" "$DIR"

ASSET="${BIN}-${TARGET}.tar.gz"

# Every release publishes SHA256SUMS; a missing or mismatched entry is a
# reason to stop, not to skip, exactly as `steroids upgrade` treats it.
fetch "${DL}/${VERSION}/SHA256SUMS" "${TMP}/SHA256SUMS" ||
	die "could not download SHA256SUMS for ${VERSION}" \
		"Check the release: https://github.com/${REPO}/releases/tag/${VERSION}"
EXPECTED="$(awk -v f="$ASSET" '$2 == f || $2 == "*" f {print tolower($1)}' "${TMP}/SHA256SUMS" | head -n 1)"
[ -n "$EXPECTED" ] || die "SHA256SUMS has no entry for ${ASSET}" \
	"That release may not ship this platform yet: https://github.com/${REPO}/releases/tag/${VERSION}"

if [ -x "${DIR}/${BIN}" ] && current="$("${DIR}/${BIN}" --version 2>/dev/null)" &&
	[ "$current" = "${BIN} ${VERSION#v}" ]; then
	step "already at" "${VERSION}, nothing to do"
	exit 0
fi

step "downloading" "$ASSET"
fetch "${DL}/${VERSION}/${ASSET}" "${TMP}/${ASSET}" ||
	die "could not download ${DL}/${VERSION}/${ASSET}"
ACTUAL="$(sha256_of "${TMP}/${ASSET}")" ||
	die "no sha256sum, shasum or openssl on this machine" \
		"Install one; the download is not trusted without a checksum."
[ "$ACTUAL" = "$EXPECTED" ] ||
	die "checksum mismatch for ${ASSET}" \
		"expected ${EXPECTED}, got ${ACTUAL}. The download was corrupted or tampered with; nothing was installed."
step "checksum" "ok"

# Extract only the one file the archive is supposed to hold, into the temp
# dir, so an unexpected archive layout cannot write anywhere else.
tar -xzf "${TMP}/${ASSET}" -C "$TMP" "$BIN" 2>/dev/null ||
	die "the archive does not contain a ${BIN} binary"
chmod 0755 "${TMP}/${BIN}"

# Sibling name then rename: a running steroids is never overwritten in place.
cp "${TMP}/${BIN}" "${DIR}/${BIN}.new" && mv "${DIR}/${BIN}.new" "${DIR}/${BIN}" ||
	die "could not write to ${DIR}"
step "installed" "${DIR}/${BIN}"

say ""
if ! on_path "$DIR"; then
	say "  ${DIR} is not on your PATH yet. Add it:"
	say "    export PATH=\"${DIR}:\$PATH\""
	say ""
fi
say "  Next: steroids add BurntSushi/ripgrep    (or hand the README to your agent)"
