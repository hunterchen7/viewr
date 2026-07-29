#!/usr/bin/env bash
set -euo pipefail

repository_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
temporary_directory="$(
  mktemp -d "${TMPDIR:-/tmp}/viewr-release-publication-test.XXXXXX"
)"
trap 'rm -rf "$temporary_directory"' EXIT

asset_directory="$temporary_directory/assets"
fake_bin="$temporary_directory/bin"
operation_log="$temporary_directory/operations"
get_count_file="$temporary_directory/get-count"
tag_count_file="$temporary_directory/tag-count"
release_state_file="$temporary_directory/release-state"
mkdir -p "$asset_directory" "$fake_bin"

version=1.2.3
release_assets=(
  "viewr-$version-source.tar.gz"
  "viewr-linux-x64.deb"
  "viewr-linux-x64.tar.gz"
  "viewr-macos-arm64.pkg"
  "viewr-macos-arm64.tar.gz"
  "viewr-windows-x64.msi"
  "viewr-windows-x64.zip"
)
for asset_name in "${release_assets[@]}"; do
  printf 'validated bytes for %s\n' "$asset_name" \
    >"$asset_directory/$asset_name"
done

if command -v sha256sum >/dev/null 2>&1; then
  checksum_command=(sha256sum)
elif command -v shasum >/dev/null 2>&1; then
  checksum_command=(shasum -a 256)
else
  echo "sha256sum or shasum is required" >&2
  exit 1
fi
(
  cd "$asset_directory"
  "${checksum_command[@]}" "${release_assets[@]}" >SHA256SUMS
)

remote_assets='[]'
asset_id=100
while IFS= read -r asset_name; do
  asset_path="$asset_directory/$asset_name"
  asset_size="$(wc -c <"$asset_path" | tr -d '[:space:]')"
  asset_digest="$("${checksum_command[@]}" "$asset_path" | awk '{print $1}')"
  remote_assets="$(
    jq -c \
      --argjson id "$asset_id" \
      --arg name "$asset_name" \
      --argjson size "$asset_size" \
      --arg digest "sha256:$asset_digest" \
      '. + [{
        id: $id,
        name: $name,
        size: $size,
        digest: $digest,
        state: "uploaded"
      }]' \
      <<<"$remote_assets"
  )"
  asset_id=$((asset_id + 1))
done < <(
  find "$asset_directory" -mindepth 1 -maxdepth 1 -type f \
    -exec basename {} \; |
    LC_ALL=C sort
)
export FAKE_RELEASE_ASSETS="$remote_assets"
export FAKE_DRIFTED_ASSETS="$(
  jq -c '.[0].digest = "sha256:ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff"' \
    <<<"$remote_assets"
)"

cat >"$fake_bin/gh" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail

expected_sha=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
expected_workflow_sha=dddddddddddddddddddddddddddddddddddddddd

if [[ "${1:-}" == "attestation" && "${2:-}" == "verify" ]]; then
  if [[ "$#" -ne 13 ||
    "$4" != "--repo" ||
    "$5" != "example/viewr" ||
    "$6" != "--signer-workflow" ||
    "$7" != "example/viewr/.github/workflows/release-binaries.yml" ||
    "$8" != "--signer-digest" ||
    "$9" != "$expected_workflow_sha" ||
    "${10}" != "--predicate-type" ||
    "${11}" != "https://github.com/hunterchen7/viewr/attestations/release-recovery/v1" ||
    "${12}" != "--format" ||
    "${13}" != "json" ]]; then
    echo "unexpected attestation arguments: $*" >&2
    exit 64
  fi
  asset_name="$(basename "$3")"
  printf 'attest:%s\n' "$asset_name" >>"$FAKE_OPERATION_LOG"
  predicate_workflow_sha="$expected_workflow_sha"
  if [[ "${FAKE_RELEASE_MODE:-success}" == "invalid-attestation" ]]; then
    predicate_workflow_sha=eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee
  fi
  jq -cn \
    --argjson release_id 123 \
    --arg release_sha "$expected_sha" \
    --arg release_tag v1.2.3 \
    --arg workflow_sha "$predicate_workflow_sha" \
    '[{
      verificationResult: {
        statement: {
          predicate: {
            release: {
              id: $release_id,
              tag: $release_tag,
              sourceCommit: $release_sha
            },
            workflow: {commit: $workflow_sha}
          }
        }
      }
    }]'
  exit 0
fi

emit_release() {
  local id=123
  local tag=v1.2.3
  local draft=true
  local prerelease=false
  local assets="$FAKE_RELEASE_ASSETS"
  local state
  state="$(<"$FAKE_RELEASE_STATE_FILE")"

  if [[ "$state" == "published" ]]; then
    draft=false
  fi
  if [[ "${FAKE_RELEASE_MODE:-success}" == "asset-drift" ]]; then
    assets="$FAKE_DRIFTED_ASSETS"
  fi
  if [[ "${1:-}" == "initial" ]]; then
    case "${FAKE_RELEASE_MODE:-success}" in
      wrong-id) id=124 ;;
      wrong-tag) tag=v9.9.9 ;;
      published) draft=false ;;
      prerelease) prerelease=true ;;
    esac
  fi

  jq -cn \
    --argjson id "$id" \
    --arg tag "$tag" \
    --argjson draft "$draft" \
    --argjson prerelease "$prerelease" \
    --argjson assets "$assets" \
    '{
      id: $id,
      tag_name: $tag,
      draft: $draft,
      prerelease: $prerelease,
      assets: $assets
    }'
}

case "$*" in
  "api repos/example/viewr/git/ref/tags/v1.2.3")
    tag_count="$(<"$FAKE_TAG_COUNT_FILE")"
    tag_count=$((tag_count + 1))
    printf '%s\n' "$tag_count" >"$FAKE_TAG_COUNT_FILE"
    printf 'tag:%s\n' "$tag_count" >>"$FAKE_OPERATION_LOG"
    if [[ "${FAKE_RELEASE_MODE:-success}" == "moved-tag" ]] ||
      [[ "${FAKE_RELEASE_MODE:-success}" == "post-moved-tag" && "$tag_count" -gt 1 ]]; then
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
    if [[ "${FAKE_RELEASE_MODE:-success}" == "query-error" && "$count" == "4" ]]; then
      exit 1
    fi
    if [[ "${FAKE_RELEASE_MODE:-success}" == "delayed" && "$count" == "4" ]]; then
      printf 'draft\n' >"$FAKE_RELEASE_STATE_FILE"
      emit_release
      printf 'published\n' >"$FAKE_RELEASE_STATE_FILE"
    elif [[ "$count" == "1" ]]; then
      emit_release initial
    else
      emit_release
    fi
    ;;
  "api --method PATCH repos/example/viewr/releases/123 -F draft=false -f make_latest=legacy")
    printf 'patch\n' >>"$FAKE_OPERATION_LOG"
    if [[ "${FAKE_RELEASE_MODE:-success}" == "bad-patch" ]]; then
      emit_release
    else
      printf 'published\n' >"$FAKE_RELEASE_STATE_FILE"
      if [[ "${FAKE_RELEASE_MODE:-success}" == "patch-error-visible" ]]; then
        exit 1
      fi
      emit_release
    fi
    ;;
  "api --method PATCH repos/example/viewr/releases/123 -F draft=true")
    printf 'rollback\n' >>"$FAKE_OPERATION_LOG"
    printf 'draft\n' >"$FAKE_RELEASE_STATE_FILE"
    emit_release
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
  printf '0\n' >"$tag_count_file"
  printf 'draft\n' >"$release_state_file"
  : >"$operation_log"
  GITHUB_REPOSITORY=example/viewr \
    GH_TOKEN=test-token \
    VIEWR_RELEASE_QUERY_ATTEMPTS=3 \
    VIEWR_RELEASE_QUERY_DELAY_SECONDS=0 \
    FAKE_GET_COUNT_FILE="$get_count_file" \
    FAKE_OPERATION_LOG="$operation_log" \
    FAKE_RELEASE_STATE_FILE="$release_state_file" \
    FAKE_TAG_COUNT_FILE="$tag_count_file" \
    PATH="$fake_bin:$PATH" \
    "$repository_root/scripts/publish-draft-release.sh" \
    --asset-directory "$asset_directory" \
    --release-id 123 \
    --release-sha aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa \
    --recovery-workflow-sha dddddddddddddddddddddddddddddddddddddddd \
    v1.2.3
}

attestation_operations=""
while IFS= read -r asset_name; do
  attestation_operations+="attest:$asset_name"$'\n'
done < <(
  find "$asset_directory" -mindepth 1 -maxdepth 1 -type f \
    -exec basename {} \; |
    LC_ALL=C sort
)

unset FAKE_RELEASE_MODE
run_publisher >/dev/null
expected_operations=$'get:1\n'"$attestation_operations"$'get:2\ntag:1\nget:3\npatch\nget:4\ntag:2'
if [[ "$(<"$operation_log")" != "$expected_operations" ]]; then
  echo "publisher did not verify assets, identity, tag, and final state" >&2
  diff \
    <(printf '%s\n' "$expected_operations") \
    "$operation_log" >&2 || true
  exit 1
fi

export FAKE_RELEASE_MODE=delayed
run_publisher >/dev/null
expected_operations=$'get:1\n'"$attestation_operations"$'get:2\ntag:1\nget:3\npatch\nget:4\nget:5\ntag:2'
if [[ "$(<"$operation_log")" != "$expected_operations" ]]; then
  echo "publisher did not retry a stale post-publication read" >&2
  exit 1
fi

export FAKE_RELEASE_MODE=query-error
run_publisher >/dev/null
if ! grep -Fxq get:5 "$operation_log"; then
  echo "publisher did not retry a failed post-publication query" >&2
  exit 1
fi

assert_rejected_before_patch() {
  export FAKE_RELEASE_MODE="$1"
  if run_publisher >/dev/null 2>&1; then
    echo "publisher accepted invalid state: $1" >&2
    exit 1
  fi
  if grep -Fxq patch "$operation_log"; then
    echo "publisher mutated invalid state: $1" >&2
    exit 1
  fi
}

assert_rejected_before_patch wrong-id
assert_rejected_before_patch wrong-tag
assert_rejected_before_patch published
assert_rejected_before_patch prerelease
assert_rejected_before_patch invalid-attestation
assert_rejected_before_patch asset-drift
assert_rejected_before_patch moved-tag

export FAKE_RELEASE_MODE=annotated-tag
run_publisher >/dev/null
expected_operations=$'get:1\n'"$attestation_operations"$'get:2\ntag:1\npeel\nget:3\npatch\nget:4\ntag:2\npeel'
if [[ "$(<"$operation_log")" != "$expected_operations" ]]; then
  echo "publisher did not resolve an annotated release tag" >&2
  exit 1
fi

export FAKE_RELEASE_MODE=bad-patch
if run_publisher >/dev/null 2>&1; then
  echo "publisher accepted an invalid PATCH response and unchanged draft" >&2
  exit 1
fi
grep -Fxq patch "$operation_log" || {
  echo "publisher did not attempt the expected exact-ID PATCH" >&2
  exit 1
}

export FAKE_RELEASE_MODE=patch-error-visible
run_publisher >/dev/null
grep -Fxq tag:2 "$operation_log" || {
  echo "publisher did not resolve an ambiguous PATCH result by exact ID" >&2
  exit 1
}

export FAKE_RELEASE_MODE=post-moved-tag
if run_publisher >/dev/null 2>&1; then
  echo "publisher accepted a tag move after publication" >&2
  exit 1
fi
grep -Fxq rollback "$operation_log" || {
  echo "publisher did not restore the release draft after a tag move" >&2
  exit 1
}
[[ "$(<"$release_state_file")" == "draft" ]] || {
  echo "publisher reported rollback without restoring draft state" >&2
  exit 1
}

echo "Exact-ID draft publication tests passed."
