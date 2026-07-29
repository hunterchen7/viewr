#!/usr/bin/env bash
set -euo pipefail

repository_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
temporary_directory="$(mktemp -d "${TMPDIR:-/tmp}/viewr-release-asset-test.XXXXXX")"
trap 'rm -rf "$temporary_directory"' EXIT

asset_directory="$temporary_directory/assets"
fake_bin="$temporary_directory/bin"
mkdir -p "$asset_directory" "$fake_bin"
printf 'validated release asset\n' > "$asset_directory/viewr-test.bin"

if command -v sha256sum >/dev/null 2>&1; then
  asset_digest="$(sha256sum "$asset_directory/viewr-test.bin" | awk '{print $1}')"
elif command -v shasum >/dev/null 2>&1; then
  asset_digest="$(shasum -a 256 "$asset_directory/viewr-test.bin" | awk '{print $1}')"
else
  echo "sha256sum or shasum is required" >&2
  exit 1
fi
asset_size="$(wc -c < "$asset_directory/viewr-test.bin" | tr -d '[:space:]')"

cat > "$fake_bin/gh" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
if [[ "$*" != "api repos/example/viewr/releases/123" ]]; then
  echo "unexpected gh arguments: $*" >&2
  exit 64
fi
printf '%s\n' "$FAKE_GH_RESPONSE"
EOF
chmod +x "$fake_bin/gh"

run_verifier() {
  GITHUB_REPOSITORY="example/viewr" \
    PATH="$fake_bin:$PATH" \
    "$repository_root/scripts/verify-remote-release-assets.sh" \
    "$@" \
    --release-id 123 \
    "$asset_directory" \
    v1.2.3
}

export FAKE_GH_RESPONSE='{"id":123,"tag_name":"v1.2.3","prerelease":false,"assets":[]}'
run_verifier --allow-missing >/dev/null

export FAKE_GH_RESPONSE="$(
  jq -cn \
    --arg name viewr-test.bin \
    --argjson size "$asset_size" \
    --arg digest "sha256:$asset_digest" \
    '{
      id: 123,
      tag_name: "v1.2.3",
      prerelease: false,
      assets: [{name: $name, size: $size, digest: $digest, state: "uploaded"}]
    }'
)"
run_verifier >/dev/null

export FAKE_GH_RESPONSE="$(
  jq -cn \
    --arg name viewr-test.bin \
    --argjson size "$asset_size" \
    '{
      id: 123,
      tag_name: "v1.2.3",
      prerelease: false,
      assets: [{name: $name, size: $size, digest: null, state: "uploaded"}]
    }'
)"
if run_verifier >/dev/null 2>&1; then
  echo "verifier accepted an asset without a digest" >&2
  exit 1
fi

export FAKE_GH_RESPONSE="$(
  jq -cn \
    --arg name viewr-test.bin \
    --argjson size "$asset_size" \
    '{
      id: 123,
      tag_name: "v1.2.3",
      prerelease: false,
      assets: [{name: $name, size: $size, digest: "sha256:bad", state: "uploaded"}]
    }'
)"
if run_verifier >/dev/null 2>&1; then
  echo "verifier accepted an incorrect digest" >&2
  exit 1
fi

export FAKE_GH_RESPONSE="$(
  jq -cn \
    --arg name viewr-test.bin \
    --argjson size "$((asset_size + 1))" \
    --arg digest "sha256:$asset_digest" \
    '{
      id: 123,
      tag_name: "v1.2.3",
      prerelease: false,
      assets: [{name: $name, size: $size, digest: $digest, state: "uploaded"}]
    }'
)"
if run_verifier >/dev/null 2>&1; then
  echo "verifier accepted an incorrect size" >&2
  exit 1
fi

export FAKE_GH_RESPONSE="$(
  jq -cn \
    --arg name viewr-test.bin \
    --argjson size "$asset_size" \
    --arg digest "sha256:$asset_digest" \
    '{
      id: 123,
      tag_name: "v1.2.3",
      prerelease: false,
      assets: [{name: $name, size: $size, digest: $digest, state: "new"}]
    }'
)"
if run_verifier >/dev/null 2>&1; then
  echo "verifier accepted an asset that was not uploaded" >&2
  exit 1
fi

export FAKE_GH_RESPONSE='{"id":123,"tag_name":"v1.2.3","prerelease":false,"assets":[{"name":"unexpected.bin","size":1,"digest":"sha256:bad","state":"uploaded"}]}'
if run_verifier --allow-missing >/dev/null 2>&1; then
  echo "verifier accepted an unexpected asset" >&2
  exit 1
fi

export FAKE_GH_RESPONSE='{"id":124,"tag_name":"v1.2.3","prerelease":false,"assets":[]}'
if run_verifier --allow-missing >/dev/null 2>&1; then
  echo "verifier accepted a different release ID" >&2
  exit 1
fi

export FAKE_GH_RESPONSE='{"id":123,"tag_name":"v9.9.9","prerelease":false,"assets":[]}'
if run_verifier --allow-missing >/dev/null 2>&1; then
  echo "verifier accepted a different release tag" >&2
  exit 1
fi

export FAKE_GH_RESPONSE='{"id":123,"tag_name":"v1.2.3","prerelease":true,"assets":[]}'
if run_verifier --allow-missing >/dev/null 2>&1; then
  echo "verifier accepted a prerelease" >&2
  exit 1
fi

echo "Remote release asset verification tests passed."
