#!/usr/bin/env bash
# Inspect the exact Linux release executable before it can be published.
set -euo pipefail
export LC_ALL=C

if [ "${1:-}" = --self-test ]; then
  fixture="$(mktemp -d)"
  trap 'rm -rf "$fixture"' EXIT
  mkdir "$fixture/bin"
  cat > "$fixture/bin/readelf" <<'MOCK'
#!/bin/sh
cat "$ELF_FIXTURE/$1"
MOCK
  chmod +x "$fixture/bin/readelf"
  export ELF_FIXTURE="$fixture"
  check() {
    local expected="$1" target="$2" diagnostic="${3:-}" maximum="${4:-2.28}"
    local status=0
    PATH="$fixture/bin:$PATH" bash "$0" fixture "$target" "$maximum" > "$fixture/output" 2>&1 || status=$?
    if { [ "$expected" = pass ] && [ "$status" -ne 0 ]; } ||
       { [ "$expected" = fail ] && [ "$status" -eq 0 ]; } ||
       ! grep -Fq "$diagnostic" "$fixture/output"; then
      echo "FAIL: expected $expected for $target ($diagnostic), got exit $status" >&2
      cat "$fixture/output" >&2
      exit 1
    fi
  }
  header_fixture() {
    printf '  Class: %s\n  Data: %s\n  Type: %s\n  Machine: %s\n' \
      "$1" "$2" "$3" "$4" > "$fixture/--file-header"
  }
  header_fixture ELF64 "2's complement, little endian" 'DYN (Position-Independent Executable file)' 'Advanced Micro Devices X86-64'
  printf '  INTERP\n' > "$fixture/--program-headers"
  printf ' (NEEDED) Shared library: [libc.so.6]\n' > "$fixture/--dynamic"
  printf ' Name: GLIBC_2.9 Flags: none\n Name: GLIBC_2.28 Flags: none\n' > "$fixture/--version-info"
  check pass x86_64-unknown-linux-gnu
  check fail x86_64-unknown-linux-gnu 'invalid glibc baseline' 2.28invalid
  check fail x86_64-unknown-linux-gnu 'invalid glibc baseline' 2
  check fail aarch64-unknown-linux-gnu 'wrong architecture'
  for version in 2.28.1 2.29 2.100 3.0 PRIVATE ABI_DT_RELR; do
    printf ' Name: GLIBC_%s Flags: none\n' "$version" > "$fixture/--version-info"
    check fail x86_64-unknown-linux-gnu "unsupported glibc symbol: GLIBC_$version"
  done
  : > "$fixture/--version-info"
  check fail x86_64-unknown-linux-gnu 'no glibc symbol requirements found'
  check fail x86_64-unknown-linux-musl 'no ELF interpreter or shared-library dependencies'
  : > "$fixture/--dynamic"
  check fail x86_64-unknown-linux-musl 'no ELF interpreter or shared-library dependencies'
  : > "$fixture/--program-headers"
  printf ' (NEEDED) Shared library: [libc.so]\n' > "$fixture/--dynamic"
  check fail x86_64-unknown-linux-musl 'no ELF interpreter or shared-library dependencies'
  : > "$fixture/--dynamic"
  check pass x86_64-unknown-linux-musl
  header_fixture ELF32 "2's complement, little endian" EXEC 'Advanced Micro Devices X86-64'
  check fail x86_64-unknown-linux-musl '64-bit little-endian executable'
  header_fixture ELF64 "2's complement, big endian" EXEC AArch64
  check fail aarch64-unknown-linux-musl '64-bit little-endian executable'
  header_fixture ELF64 "2's complement, little endian" REL AArch64
  check fail aarch64-unknown-linux-musl '64-bit little-endian executable'
  header_fixture ELF64 "2's complement, little endian" EXEC AArch64
  check pass aarch64-unknown-linux-musl
  echo 'PASS: Linux release ABI checks'
  exit 0
fi

if [ "$#" -ne 3 ]; then
  echo 'usage: check-linux-release.sh BINARY TARGET MAX_GLIBC' >&2
  exit 1
fi
binary="$1"
target="$2"
maximum="$3"
if [[ ! "$maximum" =~ ^[0-9]+\.[0-9]+(\.[0-9]+)?$ ]]; then
  echo "invalid glibc baseline: $maximum" >&2
  exit 1
fi
case "$target" in
  x86_64-unknown-linux-gnu|x86_64-unknown-linux-musl) machine='Advanced Micro Devices X86-64' ;;
  aarch64-unknown-linux-gnu|aarch64-unknown-linux-musl) machine='AArch64' ;;
  *) echo "unsupported Linux target: $target" >&2; exit 1 ;;
esac
header="$(readelf --file-header --wide "$binary")"
if ! grep -Eq 'Class: +ELF64$' <<< "$header" ||
   ! grep -Eq 'Data: +.*little endian$' <<< "$header" ||
   ! grep -Eq 'Type: +(EXEC|DYN)([[:space:]]|$)' <<< "$header"; then
  echo 'release must be a 64-bit little-endian executable' >&2
  exit 1
fi
if ! grep -Eq "Machine: +$machine$" <<< "$header"; then
  echo "release executable has the wrong architecture for $target" >&2
  exit 1
fi
case "$target" in
  *-musl)
    headers="$(readelf --program-headers --wide "$binary")"
    dynamic="$(readelf --dynamic --wide "$binary")"
    if grep -Eq '(^|[[:space:]])INTERP([[:space:]]|$)' <<< "$headers" || grep -q '(NEEDED)' <<< "$dynamic"; then
      echo 'musl release must have no ELF interpreter or shared-library dependencies' >&2
      exit 1
    fi
    ;;
  *-gnu)
    versions="$(readelf --version-info --wide "$binary")"
    awk -v maximum="$maximum" '
      function exceeds_maximum(version, parts, i) {
        split(version, parts, ".")
        for (i = 1; i <= 3; i++) {
          if (parts[i]+0 > limit[i]+0) return 1
          if (parts[i]+0 < limit[i]+0) return 0
        }
        return 0
      }
      BEGIN { split(maximum, limit, ".") }
      /Name: GLIBC_/ {
        for (i = 1; i < NF; i++) if ($i == "Name:" && $(i+1) ~ /^GLIBC_/) {
          symbol = $(i+1); version = substr(symbol, 7); count++
          if (version !~ /^[0-9]+\.[0-9]+(\.[0-9]+)?$/ || exceeds_maximum(version)) {
            print "unsupported glibc symbol: " symbol " (maximum " maximum ")"
            invalid = 1
          }
        }
      }
      END {
        if (count == 0) print "no glibc symbol requirements found in GNU release"
        exit (count == 0 || invalid)
      }
    ' <<< "$versions" >&2
    ;;
esac
echo "PASS: $target release ABI"
