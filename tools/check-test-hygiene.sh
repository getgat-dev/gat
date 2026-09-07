#!/usr/bin/env bash
# Lightweight test-isolation checks that standard Rust tooling cannot express:
# unexplained sleeps, process-global environment mutation, public network
# URLs, fixed ports, and hardcoded shared filesystem paths.
#
# Run via `task lint:test-hygiene` or directly as
# `./tools/check-test-hygiene.sh` from the repository root. Pass
# `--self-test` to instead run this script's own regression fixtures
# (see tools/check-test-hygiene-selftest.sh) and exit.
#
# Scanned roots and how each is classified as "test code":
#   - `tests/**/*.rs` and each extracted crate's `tests/**/*.rs` --
#     entirely test code; scanned in full.
#   - `test-support-*/**/*.rs` -- shared fixture crates are dev-only
#     dependency of `gat` itself (see Cargo.toml's [dev-dependencies]);
#     every line in it is test-support code even though none of it sits
#     inside a `#[cfg(test)]` block, so it's scanned in full, the same
#     as `tests/`.
#   - every root/extracted-crate `src/**/*.rs` -- ordinary production code; only the
#     region(s) from an unindented `#[cfg(test)]` immediately followed
#     by any `mod <name> { ... }` (e.g. `mod tests`, `mod test_support`,
#     `mod fixture`, `pub(crate) mod tests`) onward through that
#     module's matching closing brace are scanned (see
#     `extract_cfg_test_mod` below), so scanning one test-only module
#     never depends on it being the last module in the file, nor
#     accidentally consumes unrelated production code that follows it.
#     A file can have more than one such module (e.g. a
#     `#[cfg(test)] mod test_support` fixture helper module followed
#     later by `#[cfg(test)] mod tests`); every one is extracted. A bare
#     `#[cfg(test)]` on something other than a `mod` item (e.g. an
#     inline attribute on individual production call sites/
#     instrumentation, see src/commands/fault.rs's rationale comment) is
#     deliberately NOT treated as test code by this heuristic.
set -euo pipefail

# --- shared helpers ---------------------------------------------------

# Extracts every `#[cfg(test)] mod <name> { ... }` region of a src/*.rs
# file (see the module doc comment above for exactly what is/isn't
# matched), stopping each region at its module's own matching closing
# brace via a simple brace-depth counter -- not by running to the end
# of the file or the next `#[cfg(test)]` -- so a test-only module that
# isn't the last item in the file, or a file with more than one
# `#[cfg(test)] mod ...` block, is still scanned correctly and doesn't
# pull in unrelated production code after it. tests/*.rs and
# test-support-*/**/*.rs files don't need this: every line in them
# already counts as test code.
extract_cfg_test_mod() {
    awk '
        /^#\[cfg\(test\)\]\r?$/ { pending = NR; next }
        !in_mod && pending && NR == pending + 1 && \
            /^(pub(\(crate\))? )?mod [A-Za-z_][A-Za-z0-9_]* *\{/ {
            in_mod = 1
            depth = 0
        }
        { pending = 0 }
        in_mod {
            print
            line = $0
            len = length(line)
            for (i = 1; i <= len; i++) {
                ch = substr(line, i, 1)
                if (ch == "{") depth++
                else if (ch == "}") {
                    depth--
                    if (depth == 0) { in_mod = 0 }
                }
            }
        }
    ' "$1"
}

# Prints the full content of `$1` if it's entirely test code (tests/ or
# test-support/), or just its `#[cfg(test)] mod tests` region if it's a
# src/*.rs file.
test_code_of() {
    local file=$1
    case "$file" in
        tests/*|*/tests/*|test-support-*) cat "$file" ;;
        *) extract_cfg_test_mod "$file" ;;
    esac
}

# All source roots this script scans. Kept as one array so every check
# below (and the "extend a heuristic" doc comment above) stays in sync
# with what's actually covered.
configured_scan_roots=(
    src
    tests
    test-support-git
    test-support-gat
    gat-core/src
    gat-core/tests
    gat-io/src
    gat-io/tests
    gat-engine/src
    gat-engine/tests
    gat-command/src
    gat-command/tests
    tools
)

run_hygiene_checks() {
    local fail=0
    local scan_roots=()
    local root
    for root in "${configured_scan_roots[@]}"; do
        [ -d "$root" ] && scan_roots+=("$root")
    done

    # --- Unexplained sleeps: no new sleep in test code without an allow-comment -
    #
    # Every `std::thread::sleep`/`thread::sleep` call inside a
    # `tests/*.rs` or `test-support-*/**/*.rs` file, or inside a
    # `#[cfg(test)]` module in `src/`, must have a `sleep-ok:` comment
    # within the two lines immediately preceding it, explaining why
    # elapsed time is the actual property under test, or (for
    # `test-support`'s bounded polling helpers) why a fixed interval is
    # a polling backoff rather than a synchronization delay standing in
    # for a deterministic handshake -- see CONTRIBUTING.md's
    # testing-architecture section. There is no blanket exemption for
    # `test-support` as a whole; each sleep site must justify itself.
    echo "Checking for unexplained sleeps in test code..."
    while IFS= read -r -d '' file; do
        content=$(test_code_of "$file")
        [ -z "$content" ] && continue
        lines=$(printf '%s\n' "$content" | grep -n 'thread::sleep(' || true)
        [ -z "$lines" ] && continue
        while IFS=: read -r lineno _; do
            local window_start=$((lineno - 4))
            [ "$window_start" -lt 1 ] && window_start=1
            window=$(printf '%s\n' "$content" | sed -n "${window_start},${lineno}p")
            if ! printf '%s\n' "$window" | grep -q 'sleep-ok:'; then
                echo "  ✗ $file: thread::sleep at test-relative line $lineno has no 'sleep-ok:' comment"
                fail=1
            fi
        done <<< "$lines"
    done < <(find "${scan_roots[@]}" -name '*.rs' -print0)

    # --- Environment mutation: no new std::env::set_var/remove_var in test code ---
    #
    # Test-side environment mutation is unsound to do concurrently
    # across test threads (see CONTRIBUTING.md); real environment-
    # variable wiring is exercised through a child process instead
    # (`gat_with_env`). No file is currently allow-listed -- add one
    # below only with a comment explaining why a child process
    # genuinely cannot cover the case.
    local allow_listed_files=()
    echo "Checking for test-side std::env::set_var/remove_var..."
    while IFS= read -r -d '' file; do
        local is_allowed=0
        for allowed in "${allow_listed_files[@]+"${allow_listed_files[@]}"}"; do
            [ "$file" = "$allowed" ] && is_allowed=1
        done
        [ "$is_allowed" -eq 1 ] && continue
        content=$(test_code_of "$file")
        [ -z "$content" ] && continue
        if printf '%s\n' "$content" | grep -qE 'std::env::(set_var|remove_var)\('; then
            echo "  ✗ $file: test-side std::env::set_var/remove_var (use gat_with_env/a child process instead)"
            fail=1
        fi
    done < <(find "${scan_roots[@]}" -name '*.rs' -print0)

    # --- Network/filesystem isolation: no public URLs / fixed ports / shared paths -
    #
    # `file://` remotes are the only integration backend; no test
    # should reach a real network service, a fixed localhost port, or
    # a hardcoded shared filesystem path (every fixture must live under
    # its own owned TempDir). Checked through the same `test_code_of()`
    # extraction the sleep and env-mutation checks use, across the same
    # `scan_roots`, so a `src/**/*.rs` `#[cfg(test)] mod <name>` region
    # is covered exactly like `tests/` and `test-support/`, and an
    # ordinary production URL/path outside any such module is never
    # scanned at all.
    #
    # Extending the scan into `src/**/*.rs` surfaces many unit tests
    # that use `http(s)://`/`/tmp/...`/`C:\...`-shaped string *literals*
    # purely as input data for pure string/URL-parsing functions (URL
    # classification, secret redaction,
    # path normalization, override-resolution) -- these never reach a
    # network service or touch a shared filesystem path, they only
    # exercise string handling. There is no whole-file or whole-module
    # allowlist for this: each such occurrence must instead carry its
    # own `hygiene-ok:` comment on the single line immediately
    # preceding the match -- never on the same line as the match
    # itself, and never merely appearing inside a string literal or
    # other non-comment code on that preceding line -- explaining why
    # that exact literal can't perform network or shared-filesystem
    # I/O. Only a genuine `//` line comment (optionally indented) whose
    # text contains `hygiene-ok:` counts as a marker; code that happens
    # to contain the substring `hygiene-ok:` inside a string literal,
    # identifier, or elsewhere never authorizes anything. A marker is
    # unambiguous per match by construction: it can only ever sit directly above the one line it
    # exempts, so it can never be misread as covering some other match
    # nearby. To keep that one-marker-per-match correspondence
    # unambiguous, a line carrying more than one forbidden occurrence
    # (whether two URLs, a URL and a `/tmp/...` path, or any other
    # combination, across any of the three categories below) always
    # fails regardless of markers: each occurrence must be split onto
    # its own line with its own preceding `hygiene-ok:` comment before
    # any of them can be exempted. A genuinely new hardcoded
    # network/filesystem fixture next to an existing `hygiene-ok:`-
    # marked line is still caught, because the exemption only covers
    # the specific matched line, not the rest of the
    # function/module/file.
    echo "Checking tests/, test-support/, and src/ test modules for network URLs, fixed ports, or shared paths..."
    local url_regex='https?://'
    local port_regex='(127\.0\.0\.1|0\.0\.0\.0|localhost):[0-9]+'
    local path_regex='"(/tmp/[^"]+|C:\\\\[^"]+|file:///[[:alpha:]]:/[^"]+)"'
    # Reports every line in `$2` (the extracted test code of `$1`)
    # matching any of the three forbidden-pattern categories (URL,
    # fixed port, hardcoded shared path) above. A line with exactly one
    # such occurrence is exempt only if the single line immediately
    # before it is itself a real `//` line comment containing
    # `hygiene-ok:` (matched via `marker_regex` below, anchored to
    # optional leading whitespace then `//`) -- a string literal, a
    # doc-comment example, or any other code that merely *contains* the
    # text `hygiene-ok:` without being a genuine `//` comment on its own
    # line never authorizes the match below it. A line with more than
    # one occurrence (in any combination of categories) always fails,
    # forcing the literals to be split across separate lines each with
    # their own marker, so one `hygiene-ok:` comment can never be
    # stretched to cover more than the single match it precedes.
    local marker_regex='^[[:space:]]*//[[:space:]]*hygiene-ok:'
    check_forbidden_pattern() {
        local file=$1 content=$2
        local matched_lines
        matched_lines=$(printf '%s\n' "$content" | grep -nE "${url_regex}|${port_regex}|${path_regex}" || true)
        [ -z "$matched_lines" ] && return 0
        local local_fail=0
        local entry lineno this_line url_count port_count path_count total prev_line desc
        while IFS= read -r entry; do
            lineno=${entry%%:*}
            this_line=$(printf '%s\n' "$content" | sed -n "${lineno}p")
            url_count=$(printf '%s\n' "$this_line" | grep -oE "$url_regex" | wc -l)
            port_count=$(printf '%s\n' "$this_line" | grep -oE "$port_regex" | wc -l)
            path_count=$(printf '%s\n' "$this_line" | grep -oE "$path_regex" | wc -l)
            total=$((url_count + port_count + path_count))
            if [ "$total" -gt 1 ]; then
                echo "  ✗ $file: test-relative line $lineno packs $total forbidden network/port/path occurrences onto one line -- split each occurrence onto its own line with its own preceding 'hygiene-ok:' comment:"
                echo "      $this_line"
                local_fail=1
                continue
            fi
            if [ "$url_count" -eq 1 ]; then
                desc="found an http(s):// URL in test code (only file:// remotes are permitted)"
            elif [ "$port_count" -eq 1 ]; then
                desc="found a fixed localhost port in test code (tests must not depend on a fixed port)"
            else
                desc="found a hardcoded shared filesystem path in test code (use an owned TempDir instead)"
            fi
            if [ "$lineno" -gt 1 ]; then
                prev_line=$(printf '%s\n' "$content" | sed -n "$((lineno - 1))p")
                if printf '%s\n' "$prev_line" | grep -qE "$marker_regex"; then
                    continue
                fi
            fi
            echo "  ✗ $file: $desc (test-relative line $lineno, no preceding 'hygiene-ok:' comment):"
            echo "      $this_line"
            local_fail=1
        done <<< "$matched_lines"
        return "$local_fail"
    }
    while IFS= read -r -d '' file; do
        content=$(test_code_of "$file")
        [ -z "$content" ] && continue
        check_forbidden_pattern "$file" "$content" || fail=1
    done < <(find "${scan_roots[@]}" -name '*.rs' -print0)

    return "$fail"
}

if [ "${1:-}" = "--self-test" ]; then
    exec "$(dirname "${BASH_SOURCE[0]}")/check-test-hygiene-selftest.sh"
fi

# Only run the checks (and `exit`) when executed directly; when sourced
# (by check-test-hygiene-selftest.sh, to reuse `run_hygiene_checks`
# against fixture directories) just define the functions above.
if [ "${BASH_SOURCE[0]}" = "${0}" ]; then
    if ! run_hygiene_checks; then
        echo
        echo "Test-hygiene check failed -- see above."
        exit 1
    fi
    echo "Test-hygiene checks passed."
fi
