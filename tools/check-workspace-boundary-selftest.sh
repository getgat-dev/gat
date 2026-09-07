#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=./check-workspace-boundary.sh
source "$script_dir/check-workspace-boundary.sh"

fail=0
cases_run=0

write_valid_fixture() {
    local root=$1
    mkdir -p "$root"/{gat-core,gat-io,gat-engine,gat-command}/src
    cat >"$root/Cargo.toml" <<'EOF'
[workspace]
members = [".", "gat-core", "gat-io", "gat-engine", "gat-command", "tools"]
default-members = [".", "gat-core", "gat-io", "gat-engine", "gat-command"]

[dependencies]
gat-core = { path = "gat-core" }
gat-engine = { path = "gat-engine" }
gat-command = { path = "gat-command" }
EOF
    cat >"$root/gat-core/Cargo.toml" <<'EOF'
[dependencies]
serde = "1"
EOF
    cat >"$root/gat-io/Cargo.toml" <<'EOF'
[dependencies]
gat-core = { path = "../gat-core" }
EOF
    cat >"$root/gat-engine/Cargo.toml" <<'EOF'
[dependencies]
gat-core = { path = "../gat-core" }
gat-io = { path = "../gat-io" }
EOF
    cat >"$root/gat-command/Cargo.toml" <<'EOF'
[dependencies]
gat-core = { path = "../gat-core" }
gat-engine = { path = "../gat-engine" }
EOF
    : >"$root/gat-core/src/lib.rs"
    : >"$root/gat-io/src/lib.rs"
    : >"$root/gat-engine/src/lib.rs"
    : >"$root/gat-command/src/lib.rs"
}

assert_case() {
    local expectation=$1 name=$2 mutation=$3
    local fixture actual=pass
    cases_run=$((cases_run + 1))
    fixture=$(mktemp -d)
    write_valid_fixture "$fixture"
    case "$mutation" in
        valid) ;;
        core-upward)
            echo 'gat-io = { path = "../gat-io" }' >>"$fixture/gat-core/Cargo.toml"
            ;;
        io-upward)
            echo 'gat-engine = { path = "../gat-engine" }' >>"$fixture/gat-io/Cargo.toml"
            ;;
        io-upward-build)
            printf '\n[build-dependencies]\ngat-engine = { path = "../gat-engine" }\n' >>"$fixture/gat-io/Cargo.toml"
            ;;
        io-upward-target)
            printf "\n[target.'cfg(unix)'.dependencies]\ngat-engine = { path = \"../gat-engine\" }\n" >>"$fixture/gat-io/Cargo.toml"
            ;;
        command-io)
            echo 'storage = { package = "gat-io", path = "../gat-io" }' >>"$fixture/gat-command/Cargo.toml"
            ;;
        root-io)
            echo 'gat-io = { path = "gat-io" }' >>"$fixture/Cargo.toml"
            ;;
        optional-root)
            echo 'extra = { version = "1", optional = true }' >>"$fixture/Cargo.toml"
            ;;
        default-tools)
            sed -i.bak 's/"gat-command"\]/"gat-command", "tools"\]/' "$fixture/Cargo.toml"
            rm -f "$fixture/Cargo.toml.bak"
            ;;
        engine-file)
            echo 'fn load(file: std::fs::File) {}' >"$fixture/gat-engine/src/lib.rs"
            ;;
        engine-env)
            echo 'fn load() { std::env::var("HOME"); }' >"$fixture/gat-engine/src/lib.rs"
            ;;
        engine-remote)
            echo 'fn open() { gat_io::RemoteClient::open("file:///fixture"); }' >"$fixture/gat-engine/src/lib.rs"
            ;;
        allowed-remote)
            echo 'fn open() { gat_io::RemoteClient::open("file:///fixture"); }' >"$fixture/gat-engine/src/remote_session.rs"
            ;;
        command-file)
            echo 'fn load() { std::fs::read("x"); }' >"$fixture/gat-command/src/lib.rs"
            ;;
    esac
    (cd "$fixture" && run_workspace_boundary_checks) >/dev/null 2>&1 || actual=fail
    rm -rf "$fixture"
    if [ "$actual" = "$expectation" ]; then
        echo "  PASS $name"
    else
        echo "  FAIL $name: expected $expectation, got $actual"
        fail=1
    fi
}

echo "Running check-workspace-boundary.sh self-tests..."
assert_case pass "minimal valid workspace passes" valid
assert_case fail "core upward dependency fails" core-upward
assert_case fail "I/O upward dependency fails" io-upward
assert_case fail "I/O upward build dependency fails" io-upward-build
assert_case fail "I/O upward target dependency fails" io-upward-target
assert_case fail "renamed command-to-I/O dependency fails" command-io
assert_case fail "root production I/O dependency fails" root-io
assert_case fail "optional root dependency fails" optional-root
assert_case fail "tools as default member fails" default-tools
assert_case fail "engine buffered file access fails" engine-file
assert_case fail "engine ambient environment access fails" engine-env
assert_case fail "engine remote opening outside session fails" engine-remote
assert_case pass "engine remote session opening passes" allowed-remote
assert_case fail "command filesystem access fails" command-file

if [ "$fail" -ne 0 ]; then
    exit 1
fi
echo "check-workspace-boundary.sh self-tests passed ($cases_run cases)."
