#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=./check-error-architecture.sh
source "$script_dir/check-error-architecture.sh"

fail=0
cases_run=0

assert_case() {
    local expectation=$1 name=$2 content=$3
    local fixture actual=pass
    cases_run=$((cases_run + 1))
    fixture=$(mktemp -d)
    mkdir -p "$fixture/src/output"
    printf '%s\n' "$content" >"$fixture/src/sample.rs"
    (cd "$fixture" && run_error_architecture_checks) >/dev/null 2>&1 || actual=fail
    rm -rf "$fixture"
    if [ "$actual" = "$expectation" ]; then
        echo "  PASS $name"
    else
        echo "  FAIL $name: expected $expectation, got $actual"
        fail=1
    fi
}

echo "Running check-error-architecture.sh self-tests..."
assert_case fail "raw error output fails" \
    'fn run(error: std::io::Error) { eprintln!("failed: {error}"); }'
assert_case fail "print_error helper fails" \
    'fn print_error(error: &dyn std::error::Error) { println!("{error}"); }'
assert_case pass "typed output path passes" \
    'fn run(line: UserLine) { println!("{line}"); }'

if [ "$fail" -ne 0 ]; then
    exit 1
fi
echo "check-error-architecture.sh self-tests passed ($cases_run cases)."
