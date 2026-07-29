#!/usr/bin/env bash
set -euo pipefail

repository_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
temporary_directory="$(mktemp -d "${TMPDIR:-/tmp}/viewr-draft-release-test.XXXXXX")"
trap 'rm -rf "$temporary_directory"' EXIT

fake_bin="$temporary_directory/bin"
counter_file="$temporary_directory/queries"
mkdir -p "$fake_bin"

cat >"$fake_bin/gh" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail

expected_arguments="api --paginate repos/example/viewr/releases?per_page=100"
if [[ "$*" != "$expected_arguments" ]]; then
  echo "unexpected gh arguments: $*" >&2
  exit 64
fi

query_count=0
if [[ -f "$FAKE_GH_COUNTER_FILE" ]]; then
  query_count="$(<"$FAKE_GH_COUNTER_FILE")"
fi
query_count=$((query_count + 1))
printf '%s\n' "$query_count" >"$FAKE_GH_COUNTER_FILE"

case "${FAKE_GH_MODE:-draft}" in
  draft)
    printf '%s\n' '[{"id":123,"draft":true,"prerelease":false,"tag_name":"v1.2.3"}]'
    ;;
  published)
    printf '%s\n' '[{"id":123,"draft":false,"prerelease":false,"tag_name":"v1.2.3"}]'
    ;;
  prerelease)
    printf '%s\n' '[{"id":123,"draft":true,"prerelease":true,"tag_name":"v1.2.3"}]'
    ;;
  wrong-tag)
    printf '%s\n' '[{"id":123,"draft":true,"prerelease":false,"tag_name":"v9.9.9"}]'
    ;;
  duplicate)
    printf '%s\n' \
      '[{"id":123,"draft":true,"prerelease":false,"tag_name":"v1.2.3"},{"id":124,"draft":true,"prerelease":false,"tag_name":"v1.2.3"}]'
    ;;
  second-page)
    printf '%s\n' \
      '[{"id":122,"draft":false,"prerelease":false,"tag_name":"v1.2.2"}]' \
      '[{"id":123,"draft":true,"prerelease":false,"tag_name":"v1.2.3"}]'
    ;;
  stderr-warning)
    echo "temporary API warning" >&2
    printf '%s\n' '[{"id":123,"draft":true,"prerelease":false,"tag_name":"v1.2.3"}]'
    ;;
  malformed)
    printf '%s\n' 'not-json'
    ;;
  delayed)
    if ((query_count <= FAKE_GH_FAILURES)); then
      echo "release not found" >&2
      exit 1
    fi
    printf '%s\n' '[{"id":123,"draft":true,"prerelease":false,"tag_name":"v1.2.3"}]'
    ;;
  *)
    echo "unexpected fake mode: $FAKE_GH_MODE" >&2
    exit 64
    ;;
esac
EOF
chmod +x "$fake_bin/gh"

run_gate() {
  GITHUB_REPOSITORY=example/viewr \
    FAKE_GH_COUNTER_FILE="$counter_file" \
    PATH="$fake_bin:$PATH" \
    VIEWR_RELEASE_QUERY_ATTEMPTS="${VIEWR_RELEASE_QUERY_ATTEMPTS:-3}" \
    VIEWR_RELEASE_QUERY_DELAY_SECONDS=0 \
    "$repository_root/scripts/require-draft-release.sh" \
    v1.2.3 \
    "$@"
}

assert_query_count() {
  local expected="$1"
  local actual
  actual="$(<"$counter_file")"
  if [[ "$actual" != "$expected" ]]; then
    echo "expected $expected release queries, got $actual" >&2
    exit 1
  fi
}

export FAKE_GH_MODE=draft
rm -f "$counter_file"
if [[ "$(run_gate)" != "123" ]]; then
  echo "gate did not return the exact draft release ID" >&2
  exit 1
fi
assert_query_count 1

export FAKE_GH_MODE=second-page
rm -f "$counter_file"
if [[ "$(run_gate)" != "123" ]]; then
  echo "gate did not find the draft on a later API page" >&2
  exit 1
fi
assert_query_count 1

export FAKE_GH_MODE=stderr-warning
rm -f "$counter_file"
if [[ "$(run_gate)" != "123" ]]; then
  echo "gate mixed successful JSON with gh stderr" >&2
  exit 1
fi
assert_query_count 1

export FAKE_GH_MODE=malformed
rm -f "$counter_file"
if run_gate >/dev/null 2>&1; then
  echo "gate accepted malformed release data" >&2
  exit 1
fi
assert_query_count 1

export FAKE_GH_MODE=delayed
export FAKE_GH_FAILURES=2
rm -f "$counter_file"
run_gate >/dev/null
assert_query_count 3

export FAKE_GH_FAILURES=3
rm -f "$counter_file"
if run_gate >/dev/null 2>&1; then
  echo "gate accepted a release that never became visible" >&2
  exit 1
fi
assert_query_count 3

export FAKE_GH_MODE=published
rm -f "$counter_file"
if run_gate >/dev/null 2>&1; then
  echo "gate accepted a published release" >&2
  exit 1
fi
assert_query_count 1

export FAKE_GH_MODE=prerelease
rm -f "$counter_file"
if run_gate >/dev/null 2>&1; then
  echo "gate accepted a prerelease" >&2
  exit 1
fi
assert_query_count 1

export FAKE_GH_MODE=wrong-tag
rm -f "$counter_file"
if run_gate >/dev/null 2>&1; then
  echo "gate accepted a different release tag" >&2
  exit 1
fi
assert_query_count 3

export FAKE_GH_MODE=duplicate
rm -f "$counter_file"
if run_gate >/dev/null 2>&1; then
  echo "gate accepted duplicate release tags" >&2
  exit 1
fi
assert_query_count 1

export FAKE_GH_MODE=draft
rm -f "$counter_file"
if run_gate 999 >/dev/null 2>&1; then
  echo "gate accepted a different release ID" >&2
  exit 1
fi
assert_query_count 1

rm -f "$counter_file"
if GITHUB_REPOSITORY=example/viewr \
  FAKE_GH_COUNTER_FILE="$counter_file" \
  PATH="$fake_bin:$PATH" \
  "$repository_root/scripts/require-draft-release.sh" \
  latest >/dev/null 2>&1; then
  echo "gate accepted a non-semantic release tag" >&2
  exit 1
fi
if [[ -e "$counter_file" ]]; then
  echo "gate queried GitHub for an invalid release tag" >&2
  exit 1
fi

echo "Draft release visibility tests passed."
