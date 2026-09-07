#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=./check-test-hygiene.sh
source "$script_dir/check-test-hygiene.sh"

fail=0
cases_run=0

assert_case() {
    local expectation=$1 name=$2
    shift 2
    local fixture actual=pass spec path content
    cases_run=$((cases_run + 1))
    fixture=$(mktemp -d)
    mkdir -p "$fixture/src" "$fixture/tests" "$fixture/test-support-git"
    for spec in "$@"; do
        path=${spec%%:::*}
        content=${spec#*:::}
        mkdir -p "$fixture/$(dirname "$path")"
        printf '%s\n' "$content" >"$fixture/$path"
    done
    (cd "$fixture" && run_hygiene_checks) >/dev/null 2>&1 || actual=fail
    rm -rf "$fixture"
    if [ "$actual" = "$expectation" ]; then
        echo "  PASS $name"
    else
        echo "  FAIL $name: expected $expectation, got $actual"
        fail=1
    fi
}

echo "Running check-test-hygiene.sh self-tests..."
assert_case fail "unexplained sleep fails" \
    'tests/sample.rs:::fn test() { std::thread::sleep(Duration::ZERO); }'
assert_case pass "documented sleep passes" \
    'tests/sample.rs:::// sleep-ok: elapsed time is the behavior under test.
fn test() { std::thread::sleep(Duration::ZERO); }'
assert_case fail "environment mutation fails" \
    'test-support-git/src/lib.rs:::fn test() { std::env::set_var("A", "B"); }'
assert_case fail "public URL fails" \
    'tests/sample.rs:::const URL: &str = "https://example.com";'
assert_case fail "fixed port fails" \
    'tests/sample.rs:::const ADDRESS: &str = "127.0.0.1:8080";'
assert_case fail "shared path fails" \
    'tests/sample.rs:::const PATH: &str = "/tmp/gat-test";'
assert_case pass "marked parser input passes" \
    'tests/sample.rs:::// hygiene-ok: parser input only; never opened.
const PATH: &str = "/tmp/gat-test";'
assert_case fail "marker covers only the next single occurrence" \
    'tests/sample.rs:::// hygiene-ok: parser input only; never opened.
const FIRST: &str = "/tmp/one";
const SECOND: &str = "/tmp/two";'
assert_case fail "multiple occurrences on one line fail" \
    'tests/sample.rs:::// hygiene-ok: parser inputs only; never opened.
const URLS: (&str, &str) = ("https://one.example", "https://two.example");'
assert_case pass "production code after a test module is ignored" \
    'src/sample.rs:::#[cfg(test)]
mod tests {
    #[test]
    fn ok() {}
}
fn retry() {
    std::thread::sleep(Duration::ZERO);
    let _ = "https://example.com";
}'

if [ "$fail" -ne 0 ]; then
    exit 1
fi
echo "check-test-hygiene.sh self-tests passed ($cases_run cases)."
