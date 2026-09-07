#!/usr/bin/env bash
# Exercise the Unix installer with local release fixtures and no network access.
set -euo pipefail
installer_shell="$(command -v "${1:-/bin/sh}")"
echo "Testing installer with $installer_shell"
repo_dir="$(cd "$(dirname "$0")/.." && pwd)"
fixture="$(mktemp -d)"
trap 'rm -rf "$fixture"' EXIT
export FIXTURE_DIR="$fixture"
export GAT_VERSION=''
mkdir -p "$fixture/bin" "$fixture/tmp"
export TMPDIR="$fixture/tmp"
cat > "$fixture/gat" <<'PAYLOAD'
#!/bin/sh
[ "$1" = --version ] || exit 2
if [ "$FIXTURE_SCENARIO" = descendant ]; then
  "$REAL_SLEEP" 60 >/dev/null 2>&1 &
  printf '%s\n' "$!" > "$FIXTURE_DIR/child-pid"
fi
[ "$FIXTURE_SCENARIO" != incompatible ] || exit 1
if [ "$FIXTURE_SCENARIO" = interrupted_probe ]; then
  printf '%s\n' "$$" > "$FIXTURE_DIR/probe-pid"
  kill -TERM "$(ps -o ppid= -p "$PPID" | tr -d " ")"
  exec "$REAL_SLEEP" 60
fi
if [ "$FIXTURE_SCENARIO" = hung_payload ]; then exec "$REAL_SLEEP" 60; fi
if [ "$FIXTURE_SCENARIO" = wrong_version ]; then printf 'gat 9.9.9\n'; exit 0; fi
printf 'gat 0.1.0+build.1\n'
PAYLOAD

cat > "$fixture/bin/curl" <<'MOCK'
#!/bin/sh
set -eu
# Parse options independently of their order while enforcing the download policy.
url='' output='' write_format='' secure=false secure_redirects=false
connect_timeout='' request_timeout='' retries='' retry_budget='' fail_http=false
while [ "$#" -gt 0 ]; do
  case "$1" in
    -fsSL) fail_http=true; shift ;;
    --proto) [ "$2" = '=https' ]; secure=true; shift 2 ;;
    --proto-redir) [ "$2" = '=https' ]; secure_redirects=true; shift 2 ;;
    --connect-timeout) connect_timeout="$2"; shift 2 ;;
    --max-time) request_timeout="$2"; shift 2 ;;
    --retry) retries="$2"; shift 2 ;;
    --retry-max-time) retry_budget="$2"; shift 2 ;;
    -o) output="$2"; shift 2 ;;
    -w) write_format="$2"; shift 2 ;;
    https://*) [ -z "$url" ]; url="$1"; shift ;;
    *) echo "unexpected curl argument: $1" >&2; exit 1 ;;
  esac
done
[ "$fail_http" = true ]
[ "$secure" = true ]
[ "$secure_redirects" = true ]
[ "$connect_timeout" = 15 ]
[ "$request_timeout" = 120 ]
[ "$retries" = 2 ]
[ "$retry_budget" = 360 ]
[ -n "$url" ]
[ -n "$output" ]
printf 'request\n' >> "$FIXTURE_DIR/requests"
if [ "$url" = https://github.com/getgat-dev/gat/releases/latest ]; then
  [ "$output" = /dev/null ]
  [ "$write_format" = '%{url_effective}' ]
  case "$FIXTURE_SCENARIO" in
    latest_failure) printf 'https://github.com/getgat-dev/gat/releases/tag/v0.1.0+build.1'; exit 18 ;;
    latest_bad_url) printf 'https://example.invalid/v0.1.0+build.1'; exit 0 ;;
    latest_bad_tag) printf 'https://github.com/getgat-dev/gat/releases/tag/vV0.1.0'; exit 0 ;;
    *) printf 'https://github.com/getgat-dev/gat/releases/tag/v0.1.0+build.1'; exit 0 ;;
  esac
fi
[ -z "$write_format" ]
case "$url" in
  */SHA256SUMS)
    [ "$FIXTURE_SCENARIO" != checksum_download_failure ] || exit 22
    cp "$FIXTURE_DIR/SHA256SUMS" "$output" ;;
  */gat-*.tar.gz)
    if [ "$FIXTURE_SCENARIO" = download_failure ]; then
      printf 'partial download' > "$output"
      exit 18
    fi
    cp "$FIXTURE_DIR/${url##*/}" "$output" ;;
  *) echo "unexpected download: $url" >&2; exit 1 ;;
esac
MOCK
cat > "$fixture/bin/uname" <<'MOCK'
#!/bin/sh
case "$1" in
  -s)
    case "$FIXTURE_SCENARIO" in
      macos_*) echo Darwin ;;
      unsupported_os) echo FreeBSD ;;
      *) echo Linux ;;
    esac ;;
  -m)
    case "$FIXTURE_SCENARIO" in
      *_arm64) echo arm64 ;;
      unsupported_arch) echo mips64 ;;
      *) echo x86_64 ;;
    esac ;;
  *) exit 1 ;;
esac
MOCK
cat > "$fixture/bin/getconf" <<'MOCK'
#!/bin/sh
[ "$FIXTURE_SCENARIO" != non_glibc ] || exit 1
echo 'glibc 2.39'
MOCK
REAL_INSTALL="$(command -v install)"
REAL_MV="$(command -v mv)"
REAL_SLEEP="$(command -v sleep)"
export REAL_INSTALL REAL_MV REAL_SLEEP
cat > "$fixture/bin/install" <<'MOCK'
#!/bin/sh
if [ "$FIXTURE_SCENARIO" = copy_failure ]; then
  printf 'partial executable' > "$4"
  exit 1
fi
if [ "$FIXTURE_SCENARIO" = hangup ]; then
  kill -HUP "$PPID"
  exit 0
fi
if [ "$FIXTURE_SCENARIO" = interrupted ]; then
  kill -TERM "$PPID"
  exit 0
fi
exec "$REAL_INSTALL" "$@"
MOCK
cat > "$fixture/bin/mv" <<'MOCK'
#!/bin/sh
[ "$FIXTURE_SCENARIO" != rename_failure ] || exit 1
exec "$REAL_MV" "$@"
MOCK
cat > "$fixture/bin/sleep" <<'MOCK'
#!/bin/sh
# Advance the timeout budget immediately only for the deliberately hung probe.
[ "$FIXTURE_SCENARIO" != hung_payload ] || { "$REAL_SLEEP" 0.05; exit 0; }
exec "$REAL_SLEEP" "$@"
MOCK
chmod +x "$fixture/bin/"*
# Limit PATH so the fallback test cannot accidentally discover sha256sum.
for utility in bash ps tr cp grep awk mktemp rm tar gzip mkdir cat; do
  ln -s "$(command -v "$utility")" "$fixture/bin/$utility"
done

for checker in sha256sum shasum; do
  if ! command -v "$checker" >/dev/null 2>&1; then
    echo "SKIP: $checker is not installed"
    continue
  fi
  ln -s "$(command -v "$checker")" "$fixture/bin/$checker"
  for scenario in valid startup_env descendant hangup missing duplicate malformed short_hash malformed_duplicate mismatch similar_name crlf uppercase_hash \
    upgrade latest latest_failure latest_bad_url latest_bad_tag download_failure checksum_download_failure \
    invalid_archive missing_payload incompatible hung_payload wrong_version copy_failure rename_failure interrupted interrupted_probe directory symlink dangling_symlink fifo non_glibc \
    multiline_prefix multiline_suffix trailing_newline carriage_return double_prefix empty_version empty_version_equals env_version relative_path macos_arm64 macos_x86_64 linux_arm64 unsupported_os unsupported_arch; do
    export FIXTURE_SCENARIO="$scenario"
    : > "$fixture/requests"
    target='x86_64-unknown-linux-gnu'
    case "$scenario" in
      macos_arm64) target='aarch64-apple-darwin' ;;
      macos_x86_64) target='x86_64-apple-darwin' ;;
      linux_arm64) target='aarch64-unknown-linux-gnu' ;;
    esac
    staging="gat-v0.1.0+build.1-$target"
    archive="$staging.tar.gz"
    mkdir -p "$fixture/$staging"
    cp "$fixture/gat" "$fixture/$staging/gat"
    if [ "$scenario" = invalid_archive ]; then
      printf 'not an archive' > "$fixture/$archive"
    elif [ "$scenario" = missing_payload ]; then
      tar -czf "$fixture/$archive" -C "$fixture" bin/uname
    else
      tar -czf "$fixture/$archive" -C "$fixture" "$staging"
    fi
    if [ "$checker" = sha256sum ]; then
      hash="$(sha256sum "$fixture/$archive")"
    else
      hash="$(shasum -a 256 "$fixture/$archive")"
    fi
    hash="${hash%% *}"
    printf '%s  unrelated.tar.gz\n' "$hash" > "$fixture/SHA256SUMS"
    case "$scenario" in
      missing) : ;;
      duplicate) printf '%s  %s\n%s  %s\n' "$hash" "$archive" "$hash" "$archive" ;;
      malformed) printf '%064d  %s\n' 0 "$archive" | sed 's/^0/z/' ;;
      short_hash) printf 'abc  %s\n' "$archive" ;;
      malformed_duplicate) printf '%s  %s\nabc  %s\n' "$hash" "$archive" "$archive" ;;
      mismatch) printf '%064d  %s\n' 0 "$archive" ;;
      similar_name) printf '%s  %s.extra\n' "$hash" "$archive" ;;
      uppercase_hash) printf '%s  %s\n' "$(printf '%s' "$hash" | tr 'a-f' 'A-F')" "$archive" ;;
      crlf) printf '%s  %s\r\n' "$hash" "$archive" ;;
      *) printf '%s  %s\n' "$hash" "$archive" ;;
    esac >> "$fixture/SHA256SUMS"
    destination="$fixture/install $checker $scenario"
    mkdir -p "$destination"
    # Failures must preserve an existing installation, not merely avoid a new one.
    case "$scenario" in
      directory) mkdir "$destination/gat" ;;
      symlink) ln -s "$fixture/$staging/gat" "$destination/gat" ;;
      dangling_symlink) ln -s "$fixture/absent" "$destination/gat" ;;
      fifo) mkfifo "$destination/gat" ;;
      valid|crlf|uppercase_hash) ;;
      *) printf 'old binary\n' > "$destination/gat" ;;
    esac
    expect_success=false
    case "$scenario" in
      valid|startup_env|descendant|crlf|uppercase_hash|upgrade|latest|env_version|relative_path|macos_arm64|macos_x86_64|linux_arm64) expect_success=true ;;
    esac
    set -- --version '0.1.0+build.1'
    export GAT_VERSION=''
    case "$scenario" in
      latest*) set -- ;;
      multiline_prefix) set -- --version $'bad\n0.1.0' ;;
      multiline_suffix) set -- --version $'0.1.0\n../../bad' ;;
      trailing_newline) set -- --version $'0.1.0\n' ;;
      carriage_return) set -- --version $'0.1.0\r' ;;
      double_prefix) set -- --version vV0.1.0 ;;
      empty_version) set -- --version '' ;;
      empty_version_equals) set -- --version= ;;
      env_version) set --; export GAT_VERSION='V0.1.0+build.1' ;;
    esac
    install_override="$destination"
    if [ "$scenario" = relative_path ]; then
      install_override="./${destination##*/}"
    fi
    startup_file=''
    if [ "$scenario" = startup_env ]; then
      startup_file="$fixture/startup.sh"
      printf '%s\n' 'printf "startup hook\n"' > "$startup_file"
    fi
    if (cd "$fixture" && BASH_ENV="$startup_file" PATH="$fixture/bin" GAT_INSTALL_DIR="$install_override" "$installer_shell" \
      "$repo_dir/docs/install.sh" "$@") > "$fixture/output" 2>&1; then
      if [ "$expect_success" != true ]; then
        cat "$fixture/output"
        echo "FAIL: $checker accepted $scenario"
        exit 1
      fi
      cmp "$fixture/$staging/gat" "$destination/gat"
      test -x "$destination/gat"
    else
      installer_status=$?
      if [ "$expect_success" = true ]; then
        cat "$fixture/output"
        echo "FAIL: $checker rejected $scenario"
        exit 1
      fi
      case "$scenario" in
        directory) test -d "$destination/gat"; test ! -e "$destination/gat/gat" ;;
        symlink|dangling_symlink) test -L "$destination/gat" ;;
        fifo) test -p "$destination/gat" ;;
        *) test "$(cat "$destination/gat")" = 'old binary' ;;
      esac
    fi
    case "$scenario" in
      multiline_*|trailing_newline|carriage_return|double_prefix|empty_version|empty_version_equals)
        grep -q 'invalid version' "$fixture/output"
        test ! -s "$fixture/requests" ;;
      directory|symlink|dangling_symlink|fifo)
        grep -q 'destination must be a regular file or absent' "$fixture/output"
        test ! -s "$fixture/requests" ;;
      non_glibc) grep -q 'require glibc' "$fixture/output"; test ! -s "$fixture/requests" ;;
      missing|duplicate|malformed|short_hash|malformed_duplicate|mismatch|similar_name)
        grep -Eq 'expected exactly one valid checksum|checksum verification failed' "$fixture/output" ;;
      latest_failure|latest_bad_url) grep -q 'could not resolve' "$fixture/output" ;;
      download_failure|checksum_download_failure) grep -q 'failed to download' "$fixture/output" ;;
      latest_bad_tag) grep -q 'invalid version' "$fixture/output" ;;
      invalid_archive) grep -q 'failed to extract' "$fixture/output" ;;
      missing_payload) grep -q 'archive does not contain a regular gat executable' "$fixture/output" ;;
      hung_payload) grep -q 'gat startup timed out' "$fixture/output" ;;
      wrong_version) grep -q 'reported an unexpected version' "$fixture/output" ;;
      incompatible) grep -q 'downloaded gat cannot run' "$fixture/output" ;;
      copy_failure) grep -q 'failed to stage gat' "$fixture/output" ;;
      rename_failure) grep -q 'failed to replace gat' "$fixture/output" ;;
      hangup) test "$installer_status" -eq 129 ;;
      interrupted|interrupted_probe)
        test "$installer_status" -eq 143
        if [ "$scenario" = interrupted_probe ]; then
          if kill -0 "$(cat "$fixture/probe-pid")" 2>/dev/null; then
            echo 'FAIL: interrupted probe was left running'
            exit 1
          fi
        fi ;;
      unsupported_os) grep -q 'unsupported operating system' "$fixture/output"; test ! -s "$fixture/requests" ;;
      unsupported_arch) grep -q 'unsupported architecture' "$fixture/output"; test ! -s "$fixture/requests" ;;
    esac
    if [ "$scenario" = descendant ]; then
      child_pid="$(cat "$fixture/child-pid")"
      # A killed orphan may remain a zombie until the host's init reaps it.
      child_state="$(ps -o stat= -p "$child_pid" || :)"
      case "$child_state" in
        ''|Z*) ;;
        *) kill -KILL "$child_pid"; echo 'FAIL: probe descendant survived'; exit 1 ;;
      esac
    fi
    test -z "$(find "$destination" -name '.gat-install.*' -print)"
    test -z "$(find "$fixture/tmp" -mindepth 1 -print)"
    echo "PASS: $checker $scenario"
  done
  rm "$fixture/bin/$checker"
done

# Help must not depend on a configured home directory or installed utilities.
# Run in a subshell so the caller's environment stays intact.
(
  unset HOME GAT_INSTALL_DIR
  PATH="$fixture/no-tools" "$installer_shell" "$repo_dir/docs/install.sh" --help > "$fixture/output" 2>&1
)
grep -q '^usage: install.sh' "$fixture/output"
echo 'PASS: help without HOME'

if (
  unset HOME GAT_INSTALL_DIR
  PATH="$fixture/bin" "$installer_shell" "$repo_dir/docs/install.sh" --version 0.1.0 > "$fixture/output" 2>&1
); then
  echo 'FAIL: installed without a destination or HOME'
  exit 1
fi
grep -q 'set HOME or GAT_INSTALL_DIR' "$fixture/output"
echo 'PASS: missing install directory diagnostic'
