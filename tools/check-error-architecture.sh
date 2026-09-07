#!/usr/bin/env bash
# Defense-in-depth for raw error output that Rust's type system cannot forbid.
set -euo pipefail

run_error_architecture_checks() {
    local fail=0 file
    while IFS= read -r -d '' file; do
        if grep -nE 'fn[[:space:]]+print_error' "$file" >/dev/null; then
            echo "error-architecture: '$file' defines print_error; render Diagnostic instead." >&2
            fail=1
        fi
        [ "$file" = "src/output/error.rs" ] && continue
        if grep -nE 'eprintln!\([^)]*\b(err|error)\b' "$file" |
            grep -vE '^[0-9]+:[[:space:]]*//' >/dev/null
        then
            echo "error-architecture: '$file' prints a raw error directly; render Diagnostic instead." >&2
            fail=1
        fi
    done < <(find src -type f -name '*.rs' -print0 2>/dev/null)
    return "$fail"
}

if [ "${1:-}" = "--self-test" ]; then
    exec "$(dirname "${BASH_SOURCE[0]}")/check-error-architecture-selftest.sh"
fi

if [ "${BASH_SOURCE[0]}" = "$0" ]; then
    cd "$(dirname "${BASH_SOURCE[0]}")/.."
    run_error_architecture_checks
    echo "check-error-architecture: OK"
fi
