#!/usr/bin/env bash
# Reuse release_artifact_ with the packaged CLI running in its baseline libc.
# The Rust test harness runs on the host; every gat child runs in the container.
set -euo pipefail
if [ "$#" -ne 3 ]; then
  echo 'usage: test-linux-release.sh BINARY TARGET GLIBC_BASELINE' >&2
  exit 1
fi
binary="$1"
target="$2"
glibc_baseline="$3"
repo_dir="$(cd "$(dirname "$0")/.." && pwd)"
# Validate direct local invocations with the same ABI gate used by the workflow.
bash "$repo_dir/tools/check-linux-release.sh" "$binary" "$target" "$glibc_baseline"
libc="${target##*-}"
# Put all shared state in one owned directory: candidate, wrapper, and fixtures.
# The container only needs this directory, not write access to the checkout.
# Keeping it under target also works with Docker hosts that isolate /tmp.
mkdir -p "$repo_dir/target"
workdir="$(mktemp -d "$repo_dir/target/linux-release.XXXXXXXX")"
trap 'rm -rf "$workdir"' EXIT
mkdir -p "$workdir/bin" "$workdir/tmp"
cp "$binary" "$workdir/candidate"
# The runtime image needs no build context or candidate bytes.
docker build --target "$libc" --iidfile "$workdir/image" - < "$repo_dir/tools/release-runtime.Dockerfile"
export GAT_RELEASE_IMAGE GAT_RELEASE_WORKDIR
GAT_RELEASE_IMAGE="$(cat "$workdir/image")"
GAT_RELEASE_WORKDIR="$workdir"
if [ "$libc" = gnu ]; then
  runtime_glibc="$(docker run --rm "$GAT_RELEASE_IMAGE" getconf GNU_LIBC_VERSION)"
  if [ "$runtime_glibc" != "glibc $glibc_baseline" ]; then
    echo "runtime image has $runtime_glibc; expected glibc $glibc_baseline" >&2
    exit 1
  fi
fi
cat > "$workdir/bin/gat" <<'WRAPPER'
#!/usr/bin/env bash
set -euo pipefail
exec docker run --rm --user "$(id -u):$(id -g)" \
  --volume "$GAT_RELEASE_WORKDIR:$GAT_RELEASE_WORKDIR" \
  --volume "$GAT_RELEASE_WORKDIR/candidate:/usr/local/bin/gat:ro" \
  --workdir "$PWD" \
  --env HOME --env GIT_CONFIG_GLOBAL --env GIT_CONFIG_SYSTEM --env GAT_CACHE_DIR \
  --env TMPDIR --env NO_COLOR \
  "$GAT_RELEASE_IMAGE" gat "$@"
WRAPPER
chmod +x "$workdir/bin/gat"
cd "$repo_dir"
TMPDIR="$workdir/tmp" GAT_TEST_BIN="$workdir/bin/gat" \
  cargo test --locked --test cli_integration release_artifact_
