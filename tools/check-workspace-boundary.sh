#!/usr/bin/env bash
# Guard only dependency directions and source ownership Cargo cannot express.
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

production_dependencies() {
    awk '
        function trim(value) {
            sub(/^[[:space:]]+/, "", value)
            sub(/[[:space:]]+$/, "", value)
            return value
        }
        function emit_table() {
            if (table_dependency != "") {
                print package_name != "" ? package_name : table_dependency
                table_dependency = ""
                package_name = ""
            }
        }
        /^\[/ {
            emit_table()
            section = $0
            list = section == "[dependencies]" ||
                section == "[build-dependencies]" ||
                section ~ /^\[target\..*\.(build-)?dependencies\]$/
            if (section ~ /^\[(build-)?dependencies\.[^]]+\]$/) {
                table_dependency = section
                sub(/^\[(build-)?dependencies\./, "", table_dependency)
                sub(/\]$/, "", table_dependency)
            } else if (section ~ /^\[target\..*\.(build-)?dependencies\.[^]]+\]$/) {
                table_dependency = section
                sub(/^\[target\..*\.(build-)?dependencies\./, "", table_dependency)
                sub(/\]$/, "", table_dependency)
            }
            next
        }
        table_dependency != "" &&
            /^[[:space:]]*package[[:space:]]*=/ {
            package_name = $0
            sub(/^[^=]*=[[:space:]]*"/, "", package_name)
            sub(/".*$/, "", package_name)
            next
        }
        list && /^[[:space:]]*[^#[:space:]][^=]*=/ {
            dependency = $0
            value = dependency
            sub(/=.*/, "", dependency)
            dependency = trim(dependency)
            sub(/^[^=]*=/, "", value)
            if (value ~ /package[[:space:]]*=[[:space:]]*"/) {
                sub(/^.*package[[:space:]]*=[[:space:]]*"/, "", value)
                sub(/".*$/, "", value)
                dependency = value
            }
            gsub(/^"|"$/, "", dependency)
            print dependency
        }
        END { emit_table() }
    ' "$1"
}

has_optional_normal_dependency() {
    awk '
        /^\[/ {
            section = $0
            active = section == "[dependencies]" ||
                section ~ /^\[dependencies\.[^]]+\]$/ ||
                section ~ /^\[target\..*\.dependencies(\.[^]]+)?\]$/
            next
        }
        active && /optional[[:space:]]*=[[:space:]]*true/ { found = 1 }
        END { exit !found }
    ' "$1"
}

tools_is_default_member() {
    awk '
        /^\[workspace\]$/ { workspace = 1; next }
        /^\[/ { workspace = 0; collecting = 0 }
        workspace && /^[[:space:]]*default-members[[:space:]]*=/ {
            collecting = 1
        }
        collecting {
            text = text " " $0
            if ($0 ~ /\]/) collecting = 0
        }
        END { exit !(text ~ /"(\.\/)?tools"/) }
    ' "$1"
}

check_forbidden_dependencies() {
    local owner=$1 manifest=$2
    shift 2
    local dependencies dependency forbidden
    dependencies=$(production_dependencies "$manifest")
    for forbidden in "$@"; do
        while IFS= read -r dependency; do
            if [ "$dependency" = "$forbidden" ]; then
                echo "workspace-boundary: $owner has forbidden production dependency on $forbidden" >&2
                return 1
            fi
        done <<<"$dependencies"
    done
}

source_has() {
    local pattern=$1
    shift
    grep -nEH "$pattern" "$@" 2>/dev/null |
        grep -vE '^[^:]+:[0-9]+:[[:space:]]*//' |
        grep -q .
}

run_workspace_boundary_checks() {
    local fail=0

    check_forbidden_dependencies gat-core gat-core/Cargo.toml \
        gat-io gat-engine gat-command gat test-support-git test-support-gat ||
        fail=1
    check_forbidden_dependencies gat-io gat-io/Cargo.toml \
        gat-engine gat-command gat test-support-git test-support-gat ||
        fail=1
    check_forbidden_dependencies gat-engine gat-engine/Cargo.toml \
        gat-command gat test-support-git test-support-gat ||
        fail=1
    check_forbidden_dependencies gat-command gat-command/Cargo.toml \
        gat-io gat test-support-git test-support-gat ||
        fail=1
    check_forbidden_dependencies gat Cargo.toml \
        gat-io test-support-git test-support-gat ||
        fail=1

    if has_optional_normal_dependency Cargo.toml; then
        echo "workspace-boundary: root normal dependencies must not be optional" >&2
        fail=1
    fi
    if tools_is_default_member Cargo.toml; then
        echo "workspace-boundary: tools must not be a default workspace member" >&2
        fail=1
    fi

    local engine_files=() command_files=() remote_files=() file
    while IFS= read -r -d '' file; do
        engine_files+=("$file")
        [ "$file" = "gat-engine/src/remote_session.rs" ] ||
            remote_files+=("$file")
    done < <(find gat-engine/src -type f -name '*.rs' -print0)
    while IFS= read -r -d '' file; do
        command_files+=("$file")
    done < <(find gat-command/src -type f -name '*.rs' -print0)

    if source_has '(^|[^[:alnum:]_])(BufReader|BufWriter)([^[:alnum:]_]|$)|std[[:space:]]*::[[:space:]]*fs[[:space:]]*::[[:space:]]*File' \
        "${engine_files[@]}"
    then
        echo "workspace-boundary: gat-engine owns buffered filesystem persistence" >&2
        fail=1
    fi
    if source_has 'std[[:space:]]*::[[:space:]]*env[[:space:]]*::[[:space:]]*(current_dir|var|var_os)([^[:alnum:]_]|$)' \
        "${engine_files[@]}"
    then
        echo "workspace-boundary: gat-engine reads ambient paths or environment" >&2
        fail=1
    fi
    if source_has 'gat_io[[:space:]]*::[[:space:]]*RemoteClient[[:space:]]*::[[:space:]]*open' \
        "${remote_files[@]}"
    then
        echo "workspace-boundary: gat-engine opens a remote outside remote_session" >&2
        fail=1
    fi
    if source_has 'std[[:space:]]*::[[:space:]]*fs([^[:alnum:]_]|$)' \
        "${command_files[@]}"
    then
        echo "workspace-boundary: gat-command performs physical filesystem work" >&2
        fail=1
    fi

    return "$fail"
}

if [ "${1:-}" = "--self-test" ]; then
    exec "$script_dir/check-workspace-boundary-selftest.sh"
fi

if [ "${BASH_SOURCE[0]}" = "$0" ]; then
    cd "$script_dir/.."
    run_workspace_boundary_checks
    echo "check-workspace-boundary: OK"
fi
