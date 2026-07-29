#!/usr/bin/env bash
set -euo pipefail

repository_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
temporary_directory="$(
  mktemp -d "${TMPDIR:-/tmp}/viewr-release-publication-test.XXXXXX"
)"
trap 'rm -rf "$temporary_directory"' EXIT

fake_bin="$temporary_directory/bin"
operation_log="$temporary_directory/operations"
get_count_file="$temporary_directory/get-count"
mkdir -p "$fake_bin"

cat >"$fake_bin/gh" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail

draft='{"id":123,"tag_name":"v1.2.3","draft":true,"prerelease":false}'
published='{"id":123,"tag_name":"v1.2.3","draft":false,"prerelease":false}'
expected_sha=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa

case "$*" in
  "api repos/example/viewr/git/ref/tags/v1.2.3")
    printf 'tag\n' >>"$FAKE_OPERATION_LOG"
    if [[ "${FAKE_RELEASE_MODE:-success}" == "moved-tag" ]]; then
      printf '%s\n' \
        '{"object":{"type":"commit","sha":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"}}'
    elif [[ "${FAKE_RELEASE_MODE:-success}" == "annotated-tag" ]]; then
      printf '%s\n' \
        '{"object":{"type":"tag","sha":"cccccccccccccccccccccccccccccccccccccccc"}}'
    else
      jq -cn --arg sha "$expected_sha" \
        '{object: {type: "commit", sha: $sha}}'
    fi
    ;;
  "api repos/example/viewr/git/tags/cccccccccccccccccccccccccccccccccccccccc")
    printf 'peel\n' >>"$FAKE_OPERATION_LOG"
    jq -cn --arg sha "$expected_sha" \
      '{object: {type: "commit", sha: $sha}}'
    ;;
  "api repos/example/viewr/releases/123")
    count="$(<"$FAKE_GET_COUNT_FILE")"
    count=$((count + 1))
    printf '%s\n' "$count" >"$FAKE_GET_COUNT_FILE"
    printf 'get:%s\n' "$count" >>"$FAKE_OPERATION_LOG"
    case "${FAKE_RELEASE_MODE:-success}:$count" in
      wrong-id:1)
        printf '%s\n' \
          '{"id":124,"tag_name":"v1.2.3","draft":true,"prerelease":false}'
        ;;
      wrong-tag:1)
        printf '%s\n' \
          '{"id":123,"tag_name":"v9.9.9","draft":true,"prerelease":false}'
        ;;
      published:1)
        printf '%s\n' "$published"
        ;;
      prerelease:1)
        printf '%s\n' \
          '{"id":123,"tag_name":"v1.2.3","draft":true,"prerelease":true}'
        ;;
      delayed:2)
        printf '%s\n' "$draft"
        ;;
      *)
        if ((count == 1)); then
          printf '%s\n' "$draft"
        else
          printf '%s\n' "$published"
        fi
        ;;
    esac
    ;;
  "api --method PATCH repos/example/viewr/releases/123 -F draft=false")
    printf 'patch\n' >>"$FAKE_OPERATION_LOG"
    if [[ "${FAKE_RELEASE_MODE:-success}" == "bad-patch" ]]; then
      printf '%s\n' "$draft"
    else
      printf '%s\n' "$published"
    fi
    ;;
  *)
    echo "unexpected gh arguments: $*" >&2
    exit 64
    ;;
esac
EOF
chmod +x "$fake_bin/gh"

run_publisher() {
  printf '0\n' >"$get_count_file"
  : >"$operation_log"
  GITHUB_REPOSITORY=example/viewr \
    GH_TOKEN=test-token \
    VIEWR_RELEASE_QUERY_ATTEMPTS=3 \
    VIEWR_RELEASE_QUERY_DELAY_SECONDS=0 \
    FAKE_GET_COUNT_FILE="$get_count_file" \
    FAKE_OPERATION_LOG="$operation_log" \
    PATH="$fake_bin:$PATH" \
    "$repository_root/scripts/publish-draft-release.sh" \
    --release-id 123 \
    --release-sha aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa \
    v1.2.3
}

unset FAKE_RELEASE_MODE
run_publisher >/dev/null
expected_operations=$'tag\nget:1\npatch\nget:2'
if [[ "$(<"$operation_log")" != "$expected_operations" ]]; then
  echo "publisher did not use the exact release ID and verify publication" >&2
  diff \
    <(printf '%s\n' "$expected_operations") \
    "$operation_log" >&2 || true
  exit 1
fi

export FAKE_RELEASE_MODE=delayed
run_publisher >/dev/null
expected_operations=$'tag\nget:1\npatch\nget:2\nget:3'
if [[ "$(<"$operation_log")" != "$expected_operations" ]]; then
  echo "publisher did not retry a stale post-publication read" >&2
  exit 1
fi

assert_rejected_before_patch() {
  export FAKE_RELEASE_MODE="$1"
  if run_publisher >/dev/null 2>&1; then
    echo "publisher accepted invalid initial state: $1" >&2
    exit 1
  fi
  if grep -Fxq patch "$operation_log"; then
    echo "publisher mutated invalid initial state: $1" >&2
    exit 1
  fi
}

assert_rejected_before_patch wrong-id
assert_rejected_before_patch wrong-tag
assert_rejected_before_patch published
assert_rejected_before_patch prerelease
assert_rejected_before_patch moved-tag

export FAKE_RELEASE_MODE=annotated-tag
run_publisher >/dev/null
expected_operations=$'tag\npeel\nget:1\npatch\nget:2'
if [[ "$(<"$operation_log")" != "$expected_operations" ]]; then
  echo "publisher did not resolve an annotated release tag" >&2
  exit 1
fi

export FAKE_RELEASE_MODE=bad-patch
if run_publisher >/dev/null 2>&1; then
  echo "publisher accepted an invalid PATCH response" >&2
  exit 1
fi
grep -Fxq patch "$operation_log" || {
  echo "publisher did not attempt the expected exact-ID PATCH" >&2
  exit 1
}

echo "Exact-ID draft publication tests passed."
