#!/usr/bin/env bash
set -euo pipefail

repository_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
temporary_directory="$(mktemp -d "${TMPDIR:-/tmp}/viewr-release-upload-test.XXXXXX")"
trap 'rm -rf "$temporary_directory"' EXIT

asset_directory="$temporary_directory/assets"
fake_bin="$temporary_directory/bin"
operation_log="$temporary_directory/operations"
mkdir -p "$asset_directory" "$fake_bin"
printf 'asset a\n' >"$asset_directory/a.bin"
printf 'asset b contents\n' >"$asset_directory/b.bin"

cat >"$fake_bin/gh" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail

case "$*" in
  "api repos/example/viewr/releases/123")
    printf '%s\n' "$FAKE_RELEASE_JSON"
    ;;
  "api --method DELETE repos/example/viewr/releases/assets/"*" --silent")
    asset_id="${*: -2:1}"
    asset_id="${asset_id##*/}"
    printf 'delete:%s\n' "$asset_id" >>"$FAKE_OPERATION_LOG"
    ;;
  *)
    echo "unexpected gh arguments: $*" >&2
    exit 64
    ;;
esac
EOF
chmod +x "$fake_bin/gh"

cat >"$fake_bin/curl" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail

output_path=""
data_argument=""
upload_url=""
authorization_seen=0
previous=""
for argument in "$@"; do
  case "$previous" in
    --output)
      output_path="$argument"
      ;;
    --data-binary)
      data_argument="$argument"
      ;;
    --header)
      if [[ "$argument" == "Authorization: Bearer test-token" ]]; then
        authorization_seen=1
      fi
      ;;
  esac
  if [[ "$argument" == https://uploads.github.com/* ]]; then
    upload_url="$argument"
  fi
  previous="$argument"
done

[[ -n "$output_path" && "$data_argument" == @* && -n "$upload_url" ]] || {
  echo "missing upload arguments" >&2
  exit 64
}
[[ "$authorization_seen" == "1" ]] || {
  echo "missing exact authorization header" >&2
  exit 64
}
[[ "$upload_url" =~ ^https://uploads\.github\.com/repos/example/viewr/releases/123/assets\?name=([A-Za-z0-9._-]+)$ ]] || {
  echo "unexpected upload URL: $upload_url" >&2
  exit 64
}

asset_name="${BASH_REMATCH[1]}"
asset_path="${data_argument#@}"
asset_size="$(wc -c <"$asset_path" | tr -d '[:space:]')"
printf 'upload:%s\n' "$asset_name" >>"$FAKE_OPERATION_LOG"

if [[ "${FAKE_CURL_MODE:-success}" == "failure" ]]; then
  echo "simulated upload failure" >&2
  exit 22
fi

response_name="$asset_name"
if [[ "${FAKE_CURL_MODE:-success}" == "wrong-name" ]]; then
  response_name="wrong.bin"
fi
jq -cn \
  --arg name "$response_name" \
  --argjson size "$asset_size" \
  '{id: 900, name: $name, state: "uploaded", size: $size}' >"$output_path"
EOF
chmod +x "$fake_bin/curl"

run_uploader() {
  GITHUB_REPOSITORY=example/viewr \
    GH_TOKEN=test-token \
    FAKE_OPERATION_LOG="$operation_log" \
    PATH="$fake_bin:$PATH" \
    "$repository_root/scripts/upload-release-assets.sh" \
    --release-id 123 \
    "$asset_directory" \
    v1.2.3
}

assert_no_operations() {
  if [[ -s "$operation_log" ]]; then
    echo "uploader mutated remote state before rejecting invalid input" >&2
    cat "$operation_log" >&2
    exit 1
  fi
}

export FAKE_RELEASE_JSON='{"id":123,"tag_name":"v1.2.3","draft":true,"prerelease":false,"assets":[{"id":11,"name":"a.bin"}]}'
: >"$operation_log"
run_uploader >/dev/null
expected_operations=$'delete:11\nupload:a.bin\nupload:b.bin'
if [[ "$(<"$operation_log")" != "$expected_operations" ]]; then
  echo "unexpected upload operations" >&2
  diff \
    <(printf '%s\n' "$expected_operations") \
    "$operation_log" >&2 || true
  exit 1
fi

export FAKE_RELEASE_JSON='{"id":123,"tag_name":"v1.2.3","draft":true,"prerelease":false,"assets":[{"id":12,"name":"unexpected.bin"}]}'
: >"$operation_log"
if run_uploader >/dev/null 2>&1; then
  echo "uploader accepted an unexpected existing asset" >&2
  exit 1
fi
assert_no_operations

export FAKE_RELEASE_JSON='{"id":124,"tag_name":"v1.2.3","draft":true,"prerelease":false,"assets":[]}'
: >"$operation_log"
if run_uploader >/dev/null 2>&1; then
  echo "uploader accepted a different release ID" >&2
  exit 1
fi
assert_no_operations

export FAKE_RELEASE_JSON='{"id":123,"tag_name":"v1.2.3","draft":false,"prerelease":false,"assets":[]}'
: >"$operation_log"
if run_uploader >/dev/null 2>&1; then
  echo "uploader accepted a published release" >&2
  exit 1
fi
assert_no_operations

export FAKE_RELEASE_JSON='{"id":123,"tag_name":"v1.2.3","draft":true,"prerelease":true,"assets":[]}'
: >"$operation_log"
if run_uploader >/dev/null 2>&1; then
  echo "uploader accepted a prerelease" >&2
  exit 1
fi
assert_no_operations

export FAKE_RELEASE_JSON='{"id":123,"tag_name":"v1.2.3","draft":true,"prerelease":false,"assets":[]}'
export FAKE_CURL_MODE=wrong-name
: >"$operation_log"
if run_uploader >/dev/null 2>&1; then
  echo "uploader accepted a mismatched upload response" >&2
  exit 1
fi
unset FAKE_CURL_MODE

echo "Exact-ID release asset upload tests passed."
