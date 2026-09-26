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

step() {
  printf '=> %s\n' "$1"
}

err() {
  printf 'error: %s\n' "$*" >&2
  exit 1
}

# Print one literal shell argument, including apostrophes and trailing newlines.
quote_shell() {
  printf "'"
  while :; do
    case "$1" in
      *"'"*)
        printf '%s' "${1%%\'*}" "'\\''"
        set -- "${1#*\'}"
        ;;
      *) printf "%s'" "$1"; break ;;
    esac
  done
}

show_next_steps() {
  resolved_gat="$(command -v gat 2>/dev/null)" || resolved_gat=""
  printf '\nTry gat now:\n  '
  # Compare file identity through Bash: POSIX test has no portable -ef operator.
  if [ -n "$resolved_gat" ] && bash -p -c '[[ "$1" -ef "$2" ]]' bash "$resolved_gat" "$INSTALL_DIR/gat"; then
    printf 'gat --help\n'
  else
    quote_shell "$INSTALL_DIR/gat"
    printf ' --help\n'
    if [ -n "$resolved_gat" ]; then
      printf '\nYour PATH currently finds another gat first:\n  %s\n' "$resolved_gat"
    fi
    case "$INSTALL_DIR" in
      *:*)
        printf '\nThis directory contains a colon, so it cannot be added to PATH.\nUse the full command above, or set GAT_INSTALL_DIR to a directory without a colon and reinstall.\n'
        ;;
      *)
        printf '\nTo use this installation by name, put its directory first in PATH.\n'
        printf 'For sh, bash, or zsh, run:\n  export PATH='
        quote_shell "$INSTALL_DIR"
        # shellcheck disable=SC2016 # The suggested command expands PATH when run.
        printf ':"$PATH"\n'
        printf '\nFor future terminals, add that line to your shell startup file\n(e.g. ~/.bashrc for bash or ~/.zshrc for zsh).\n'
        ;;
    esac
  fi
  printf '\nInstallation guide: https://getgat.dev/installation\n'
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
  # Reject files in the directory chain before requesting release metadata.
  parent="$INSTALL_DIR"
  while [ ! -d "$parent" ]; do
    if [ -e "$parent" ] || [ -L "$parent" ]; then
      err "installation directory is blocked by a non-directory: $parent. Set GAT_INSTALL_DIR to a directory you can write to."
    fi
    parent="${parent%/*}"
    [ -n "$parent" ] || parent=/
  done
  if [ -L "$INSTALL_DIR/gat" ] || {
    [ -e "$INSTALL_DIR/gat" ] && [ ! -f "$INSTALL_DIR/gat" ]
  }; then
    err "destination must be a regular file or absent: $INSTALL_DIR/gat"
  fi
}

usage() {
  if [ "${1:-0}" -ne 0 ]; then exec 1>&2; fi
  printf '%s\n' 'usage: install.sh [--version|-v <version>] [--libc auto|gnu|musl]

  --version, -v <version>   Install this exact gat release (e.g. 0.1.0 or
                             v0.1.0) instead of the latest release. Can also
                             be supplied via the GAT_VERSION environment
                             variable.
  --libc auto|gnu|musl      Linux binary variant (default: auto). Auto uses
                             GNU on glibc '"$MIN_GLIBC"'+, static musl otherwise.
  --help, -h              Show this help.

Environment:
  GAT_VERSION             Release to install (overridden by --version).
  GAT_INSTALL_DIR         Installation directory (default: ~/.local/bin).

The installer does not modify your shell startup files.'
  exit "${1:-0}"
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
      printf 'error: unknown argument: %s\n' "$1" >&2
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
  need_cmd "$utility" || err "'$utility' is required to install gat. Install it with your system package manager, then try again."
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

printf '\n'
step 'Installing gat'
printf '\n'

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
        step "Using the $LIBC Linux build (glibc $glibc_version detected)."
      else
        step 'Using the portable Linux build (glibc not detected).'
      fi
    else
      step "Using the $LIBC Linux build you requested."
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
  step 'Finding the latest release...'
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

workdir="$(mktemp -d)" || err "could not create a temporary directory. Check TMPDIR and available disk space."
stagedir=""
probe_pid=""
cleanup() {
  status=$?
  trap - EXIT
  if [ -n "$probe_pid" ]; then
    kill -TERM "$probe_pid" 2>/dev/null || :
    wait "$probe_pid" 2>/dev/null || :
  fi
  if ! rm -rf "$workdir"; then
    printf 'warning: could not remove temporary directory %s; remove it manually.\n' "$workdir" >&2
  fi
  if [ -n "$stagedir" ] && ! rm -rf "$stagedir"; then
    printf 'warning: could not remove staging directory %s; remove it manually.\n' "$stagedir" >&2
  fi
  exit "$status"
}
trap cleanup EXIT
trap 'exit 129' HUP
trap 'exit 130' INT
trap 'exit 143' TERM

# Inspect the release's own asset inventory first. Older pinned releases may
# not offer musl; do not substitute a different version or an incompatible GNU asset.
step 'Checking release files...'
fetch "$base_url/SHA256SUMS" -o "$workdir/SHA256SUMS" \
  || err "failed to download checksum for $archive. Check your connection and that release $tag exists at https://github.com/$REPO/releases."

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

step "Downloading gat $tag for $target..."
fetch "$base_url/$archive" -o "$workdir/$archive" \
  || err "failed to download $archive from release $tag. Check your connection and try again."

step 'Verifying the download...'
(
  cd "$workdir" || exit 1
  if need_cmd sha256sum; then
    sha256sum -c "$archive.sha256" > /dev/null
  else
    shasum -a 256 -c "$archive.sha256" > /dev/null
  fi
) || err "checksum verification failed for $archive. Please run the installer again; the download may be incomplete."

step 'Extracting gat...'
tar -xzf "$workdir/$archive" -C "$workdir" \
  || err "failed to extract $archive. Check available disk space and try again; the archive may be damaged."
mkdir -p "$INSTALL_DIR" \
  || err "could not create $INSTALL_DIR. Set GAT_INSTALL_DIR to a directory you can write to."
# Stage on the destination filesystem so committing the upgrade is a rename.
# A failed copy or incompatible executable must leave the old binary intact.
stagedir="$(mktemp -d "$INSTALL_DIR/.gat-install.XXXXXXXX")" \
  || err "could not stage gat in $INSTALL_DIR. Check directory permissions and available disk space; existing installation preserved."
payload="$workdir/gat-$tag-$target/gat"
if [ ! -f "$payload" ] || [ -L "$payload" ]; then
  err "archive does not contain a regular gat executable"
fi
install -m 755 "$payload" "$stagedir/gat" \
  || err "failed to stage gat. Check directory permissions and available disk space; existing installation preserved."
step 'Checking that gat runs on your system...'
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
step "Installing to $INSTALL_DIR..."
assert_destination
mv -f "$stagedir/gat" "$INSTALL_DIR/" \
  || err "failed to replace gat. Check directory permissions and close any running gat processes, then try again; existing installation preserved."

printf '\n'
step "Successfully installed gat $tag!"
printf '  Location: %s/gat\n' "$INSTALL_DIR"

show_next_steps
