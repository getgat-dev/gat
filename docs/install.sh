#!/usr/bin/env sh
# Download, verify, and install a prebuilt gat release.
# Served from main via the redirect in docs/docs.json.
# Pin a version with the option below or GAT_VERSION; otherwise use latest.
#
# Usage:
#   curl -fsSL https://getgat.dev/install.sh | sh
#
#   curl -fsSL https://getgat.dev/install.sh | sh -s -- --version 0.1.0
set -eu

REPO="getgat-dev/gat"
VERSION="${GAT_VERSION:-}"
INSTALL_DIR="${GAT_INSTALL_DIR:-}"
LIBC=auto
# Keep aligned with the GNU release build and artifact checks.
MIN_GLIBC=2.28

# Official SemVer 2.0.0 grammar (https://semver.org), ERE form, without the
# leading "v" (which is stripped and re-added separately).
SEMVER_RE='^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(-((0|[1-9][0-9]*|[0-9]*[A-Za-z-][0-9A-Za-z-]*)(\.(0|[1-9][0-9]*|[0-9]*[A-Za-z-][0-9A-Za-z-]*))*))?(\+([0-9A-Za-z-]+(\.[0-9A-Za-z-]+)*))?$'

err() {
  echo "error: $*" >&2
  exit 1
}

need_cmd() {
  command -v "$1" > /dev/null 2>&1
}

# Keep every request bounded, retry curl's transient failures, and prevent
# redirects from weakening the transport used for release assets.
fetch() {
  curl -fsSL --proto '=https' --proto-redir '=https' \
    --connect-timeout 15 --max-time 120 --retry 2 --retry-max-time 360 "$@"
}

assert_destination() {
  if [ -L "$INSTALL_DIR/gat" ] || {
    [ -e "$INSTALL_DIR/gat" ] && [ ! -f "$INSTALL_DIR/gat" ]
  }; then
    err "destination must be a regular file or absent: $INSTALL_DIR/gat"
  fi
}

usage() {
  printf '%s\n' 'usage: install.sh [--version|-v <version>] [--libc auto|gnu|musl]

  --version, -v <version>   Install this exact gat release (e.g. 0.1.0 or
                             v0.1.0) instead of the latest release. Can also
                             be supplied via the GAT_VERSION environment
                             variable.
  --libc auto|gnu|musl      Linux binary variant (default: auto). Auto uses
                             GNU on glibc '"$MIN_GLIBC"'+, static musl otherwise.' >&2
  exit "${1:-1}"
}

while [ $# -gt 0 ]; do
  case "$1" in
    --version | -v)
      [ $# -ge 2 ] || err "$1 requires an argument"
      [ -n "$2" ] || err "invalid version: $1 requires a non-empty argument"
      VERSION="$2"
      shift 2
      ;;
    --version=*)
      VERSION="${1#*=}"
      [ -n "$VERSION" ] || err "invalid version: --version requires a non-empty argument"
      shift
      ;;
    --libc)
      [ $# -ge 2 ] || err "--libc requires auto, gnu, or musl"
      LIBC="$2"
      shift 2
      ;;
    --libc=*)
      LIBC="${1#*=}"
      shift
      ;;
    -h | --help)
      usage 0
      ;;
    *)
      echo "error: unknown argument: $1" >&2
      usage 1
      ;;
  esac
done
case "$LIBC" in
  auto|gnu|musl) ;;
  *) err "invalid --libc: expected auto, gnu, or musl" ;;
esac

if [ -z "$INSTALL_DIR" ]; then
  [ -n "${HOME:-}" ] || err "set HOME or GAT_INSTALL_DIR to choose an install directory"
  INSTALL_DIR="$HOME/.local/bin"
fi

for utility in bash curl tar awk grep uname mktemp rm mkdir install mv cat sleep; do
  need_cmd "$utility" || err "'$utility' is required to install gat"
done
# Make relative overrides safe for utility option parsing and staged moves.
case "$INSTALL_DIR" in
  /*) ;;
  *) INSTALL_DIR="$PWD/$INSTALL_DIR" ;;
esac
assert_destination
if ! need_cmd sha256sum && ! need_cmd shasum; then
  err "'sha256sum' or 'shasum' is required to install gat"
fi

os="$(uname -s)"
arch="$(uname -m)"

case "$os" in
  Linux)
    if [ "$LIBC" = auto ]; then
      libc="$(getconf GNU_LIBC_VERSION 2>/dev/null)" || libc=""
      # Treat absent, failed, or unrecognized detection as unknown. Never
      # compare versions lexically (2.9 is older than 2.28).
      glibc_version="$(printf '%s\n' "$libc" | awk '
        NR == 1 && /^glibc [0-9]+\.[0-9]+$/ { version = $2; valid = 1; next }
        { valid = 0 }
        END { if (valid) print version }
      ')"
      LIBC=musl
      if [ -n "$glibc_version" ]; then
        if awk -v actual="$glibc_version" -v minimum="$MIN_GLIBC" 'BEGIN {
          split(actual, a, "."); split(minimum, m, ".")
          exit !(a[1] > m[1] || (a[1] == m[1] && a[2] >= m[2]))
        }'; then
          LIBC=gnu
        fi
        echo "Detected glibc $glibc_version; selecting $LIBC build."
      else
        echo "Could not detect glibc; selecting static musl build."
      fi
    else
      echo "Selecting $LIBC build (--libc override)."
    fi
    platform="unknown-linux-$LIBC"
    ;;
  Darwin)
    [ "$LIBC" = auto ] || err "--libc gnu/musl is only supported on Linux"
    platform="apple-darwin"
    ;;
  *) err "unsupported operating system: $os (see https://getgat.dev/installation for other options)" ;;
esac

case "$arch" in
  x86_64 | amd64) cpu="x86_64" ;;
  arm64 | aarch64) cpu="aarch64" ;;
  *) err "unsupported architecture: $arch" ;;
esac

target="${cpu}-${platform}"

if [ -z "$VERSION" ]; then
  # Resolve GitHub's canonical release redirect instead of parsing JSON with
  # line-oriented tools. Validate the resulting URL before using its tag.
  latest_url="$(fetch -o /dev/null -w '%{url_effective}' \
    "https://github.com/$REPO/releases/latest")" \
    || err "could not resolve the latest gat release"
  case "$latest_url" in
    "https://github.com/$REPO/releases/tag/"*)
      VERSION="${latest_url#https://github.com/"$REPO"/releases/tag/}"
      ;;
    *) err "could not resolve the latest gat release" ;;
  esac
fi

# Normalize both "0.1.0" and "v0.1.0" to a bare SemVer core, then strictly
# validate it before it is ever used to build a URL or path. Fail closed on
# anything that isn't a well-formed SemVer version.
case "$VERSION" in
  *'
'*|*"$(printf '\r')"*) err "invalid version: expected a single-line SemVer" ;;
esac
core="$VERSION"
case "$core" in [vV]*) core="${core#?}" ;; esac
printf '%s' "$core" | LC_ALL=C grep -Eq "$SEMVER_RE" \
  || err "invalid version: '$VERSION' (expected SemVer, e.g. 0.1.0 or v0.1.0)"
tag="v$core"

archive="gat-$tag-$target.tar.gz"
base_url="https://github.com/$REPO/releases/download/$tag"

workdir="$(mktemp -d)"
stagedir=""
probe_pid=""
cleanup() {
  if [ -n "$probe_pid" ]; then
    kill -TERM "$probe_pid" 2>/dev/null || :
    wait "$probe_pid" 2>/dev/null || :
  fi
  rm -rf "$workdir"
  [ -z "$stagedir" ] || rm -rf "$stagedir"
}
trap cleanup EXIT
trap 'exit 129' HUP
trap 'exit 130' INT
trap 'exit 143' TERM

# Inspect the release's own asset inventory first. Older pinned releases may
# not offer musl; do not substitute a different version or an incompatible GNU asset.
fetch "$base_url/SHA256SUMS" -o "$workdir/SHA256SUMS" \
  || err "failed to download checksum for $archive"

# Select one exact filename before checking: the other release archives are
# intentionally not downloaded. Reject missing, duplicate, or malformed entries.
awk -v archive="$archive" '
  { sub(/\r$/, ""); separator = index($0, "  ") }
  separator && substr($0, separator + 2) == archive {
    count++
    hash = substr($0, 1, separator - 1)
    if (length(hash) != 64 || hash ~ /[^0-9a-fA-F]/) invalid = 1
    print
  }
  END { if (count > 1 || invalid) exit 1 }
' "$workdir/SHA256SUMS" > "$workdir/$archive.sha256" \
  || err "failed to read exactly one valid checksum for $archive from SHA256SUMS"
# An empty successful selection means an absent asset. Keep parser/I/O errors
# separate: awk exit codes are not an asset-availability protocol.
[ -s "$workdir/$archive.sha256" ] \
  || err "release $tag does not provide $target; choose a release with this build or build from source (https://getgat.dev/installation); existing installation preserved"

echo "Downloading gat $tag for $target..."
fetch "$base_url/$archive" -o "$workdir/$archive" \
  || err "failed to download $archive from release $tag"

(
  cd "$workdir" || exit 1
  if need_cmd sha256sum; then
    sha256sum -c "$archive.sha256"
  else
    shasum -a 256 -c "$archive.sha256"
  fi
) || err "checksum verification failed for $archive"

tar -xzf "$workdir/$archive" -C "$workdir" \
  || err "failed to extract $archive"
mkdir -p "$INSTALL_DIR"
# Stage on the destination filesystem so committing the upgrade is a rename.
# A failed copy or incompatible executable must leave the old binary intact.
stagedir="$(mktemp -d "$INSTALL_DIR/.gat-install.XXXXXXXX")"
payload="$workdir/gat-$tag-$target/gat"
if [ ! -f "$payload" ] || [ -L "$payload" ]; then
  err "archive does not contain a regular gat executable"
fi
install -m 755 "$payload" "$stagedir/gat" \
  || err "failed to stage gat; existing installation preserved"
# Poll a child process so startup cannot hang installation indefinitely. Keep
# its PID owned until wait completes, including when a signal triggers cleanup.
# Bash job control gives the probe its own process group on both Linux and
# macOS. The supervisor owns that group until all descendants are terminated.
# Privileged mode suppresses BASH_ENV, exported functions, and inherited
# shell options that could alter the supervisor's waits or cleanup traps.
bash -p -c '
  set -m
  "$1" --version </dev/null &
  child=$!
  trap '\''kill -KILL -- -"$child" 2>/dev/null || :'\'' EXIT
  trap '\''exit 129'\'' HUP
  trap '\''exit 130'\'' INT
  trap '\''exit 143'\'' TERM
  wait "$child"
' bash "$stagedir/gat" > "$stagedir/version" 2> "$stagedir/stderr" &
probe_pid=$!
probe_timed_out=false
remaining=15
while kill -0 "$probe_pid" 2>/dev/null; do
  if [ "$remaining" -eq 0 ]; then
    probe_timed_out=true
    kill -TERM "$probe_pid" 2>/dev/null || :
    break
  fi
  sleep 1
  remaining=$((remaining - 1))
done
probe_status=0
wait "$probe_pid" || probe_status=$?
probe_pid=""
[ "$probe_timed_out" = false ] || err "gat startup timed out; existing installation preserved"
if [ "$probe_status" -ne 0 ]; then
  cat "$stagedir/stderr" >&2
  err "downloaded gat cannot run on this system; existing installation preserved"
fi
[ "$(cat "$stagedir/version")" = "gat $core" ] \
  || err "downloaded gat reported an unexpected version; existing installation preserved"
assert_destination
mv -f "$stagedir/gat" "$INSTALL_DIR/" \
  || err "failed to replace gat; existing installation preserved"

echo "Installed gat $tag to $INSTALL_DIR/gat"

case ":${PATH:-}:" in
  *":$INSTALL_DIR:"*) ;;
  *)
    echo "Note: $INSTALL_DIR is not on your PATH."
    echo "  Add it, e.g.: export PATH=\"$INSTALL_DIR:\$PATH\""
    ;;
esac
