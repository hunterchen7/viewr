#!/usr/bin/env bash
set -euo pipefail

usage() {
  echo "usage: scripts/test-linux-deb-install.sh --allow-system-changes <deb-package>" >&2
}

if [[ "${1:-}" != "--allow-system-changes" || $# -ne 2 ]]; then
  usage
  exit 2
fi
package="$(realpath "$2")"

for command in \
  apt-get awk cat cmp cp dpkg dpkg-query grep mkdir mktemp realpath rm sudo \
  timeout update-desktop-database xdg-mime
do
  command -v "$command" >/dev/null 2>&1 || {
    echo "required command is unavailable: $command" >&2
    exit 1
  }
done
if [[ ! -s "$package" ]]; then
  echo "package is missing or empty: $package" >&2
  exit 1
fi
if dpkg-query --show --showformat='${Status}' viewr 2>/dev/null |
  grep -Fxq 'install ok installed'; then
  echo "refusing to test over an existing Viewr installation" >&2
  exit 1
fi
for managed_path in \
  /usr/bin/viewr \
  /usr/share/applications/viewr.desktop \
  /usr/share/applications/viewr-arw.desktop \
  /usr/share/doc/viewr
do
  if [[ -e "$managed_path" || -L "$managed_path" ]]; then
    echo "refusing to replace an existing $managed_path" >&2
    exit 1
  fi
done
sudo -n true

mime_cache_has_viewr() {
  local cache=/usr/share/applications/mimeinfo.cache
  if [[ ! -e "$cache" && ! -L "$cache" ]]; then
    return 1
  fi
  [[ -f "$cache" && -r "$cache" ]] || return 2
  awk -F= '
    $1 == "image/x-sony-arw" {
      count = split($2, handlers, ";")
      for (handler_index = 1; handler_index <= count; handler_index++) {
        if (handlers[handler_index] == "viewr-arw.desktop") {
          found = 1
        }
      }
    }
    END { exit found ? 0 : 1 }
  ' "$cache"
}

if mime_cache_has_viewr; then
  echo "refusing to replace a pre-existing Viewr ARW MIME-cache entry" >&2
  exit 1
else
  mime_cache_status=$?
  if [[ "$mime_cache_status" -ne 1 ]]; then
    echo "could not inspect the desktop MIME cache before installation" >&2
    exit 1
  fi
fi

temporary_state="$(mktemp -d)"
mimeapps_snapshot="$temporary_state/mimeapps.snapshot"
installed=0
cleanup() {
  original_status=$?
  trap - EXIT
  trap '' INT TERM
  cleanup_failed=0

  if [[ "$installed" == "1" ]]; then
    if ! sudo -n dpkg --purge viewr >/dev/null; then
      echo "cleanup could not purge the Viewr test package" >&2
      cleanup_failed=1
    fi
  fi
  rm -rf "$temporary_state" || cleanup_failed=1

  if [[ "$cleanup_failed" == "1" && "$original_status" == "0" ]]; then
    original_status=1
  fi
  exit "$original_status"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

export XDG_CONFIG_HOME="$temporary_state/config"
export XDG_DATA_HOME="$temporary_state/data"
mkdir -p "$XDG_CONFIG_HOME" "$XDG_DATA_HOME/applications"
cat >"$XDG_DATA_HOME/applications/viewr-test-existing.desktop" <<'EOF'
[Desktop Entry]
Type=Application
Name=Existing ARW viewer
Exec=/bin/true %f
MimeType=image/x-sony-arw;
NoDisplay=true
EOF
update-desktop-database "$XDG_DATA_HOME/applications"
xdg-mime default viewr-test-existing.desktop image/x-sony-arw

mimeapps="$XDG_CONFIG_HOME/mimeapps.list"
[[ -f "$mimeapps" ]] || {
  echo "xdg-mime did not create the explicit default test fixture" >&2
  exit 1
}
mimeapps_existed=0
if [[ -e "$mimeapps" ]]; then
  mimeapps_existed=1
  cp "$mimeapps" "$mimeapps_snapshot"
fi
default_before="$(xdg-mime query default image/x-sony-arw 2>/dev/null || true)"
[[ "$default_before" == "viewr-test-existing.desktop" ]] || {
  echo "could not establish the unrelated ARW default test fixture" >&2
  exit 1
}

installed=1
sudo -n apt-get install -y "$package"

[[ -x /usr/bin/viewr ]] || {
  echo "installed Viewr executable is missing" >&2
  exit 1
}
[[ -f /usr/share/applications/viewr.desktop ]] || {
  echo "installed desktop launcher is missing" >&2
  exit 1
}
[[ -f /usr/share/applications/viewr-arw.desktop ]] || {
  echo "installed desktop handler is missing" >&2
  exit 1
}
if mime_cache_has_viewr; then
  :
else
  mime_cache_status=$?
  if [[ "$mime_cache_status" -ne 1 ]]; then
    echo "could not inspect the desktop MIME cache after installation" >&2
    exit 1
  fi
  echo "desktop-file-utils did not register the installed ARW handler" >&2
  exit 1
fi
usage_output="$(timeout 10s /usr/bin/viewr 2>&1)" || {
  echo "installed Viewr usage smoke test failed" >&2
  exit 1
}
grep -Fq 'usage: viewr <folder|file.arw>' <<<"$usage_output" || {
  echo "installed Viewr did not print its usage text" >&2
  exit 1
}

default_after_install="$(xdg-mime query default image/x-sony-arw 2>/dev/null || true)"
[[ "$default_after_install" == "$default_before" ]] || {
  echo "installation changed the current user's ARW default" >&2
  exit 1
}

sudo -n dpkg --purge viewr
installed=0
[[ ! -e /usr/bin/viewr ]] || {
  echo "purge left /usr/bin/viewr behind" >&2
  exit 1
}
[[ ! -e /usr/share/applications/viewr.desktop ]] || {
  echo "purge left the desktop launcher behind" >&2
  exit 1
}
[[ ! -e /usr/share/applications/viewr-arw.desktop ]] || {
  echo "purge left the desktop handler behind" >&2
  exit 1
}
if mime_cache_has_viewr; then
  echo "purge left the ARW handler in the desktop MIME cache" >&2
  exit 1
else
  mime_cache_status=$?
  if [[ "$mime_cache_status" -ne 1 ]]; then
    echo "could not inspect the desktop MIME cache after purge" >&2
    exit 1
  fi
fi
[[ ! -e /usr/share/doc/viewr ]] || {
  echo "purge left the Viewr documentation directory behind" >&2
  exit 1
}
default_after_purge="$(xdg-mime query default image/x-sony-arw 2>/dev/null || true)"
[[ "$default_after_purge" == "$default_before" ]] || {
  echo "purge changed the current user's ARW default" >&2
  exit 1
}

if [[ "$mimeapps_existed" == "1" ]]; then
  cmp -s "$mimeapps_snapshot" "$mimeapps" || {
    echo "install or purge changed the current user's mimeapps.list" >&2
    exit 1
  }
elif [[ -e "$mimeapps" ]]; then
  echo "install or purge created a user mimeapps.list" >&2
  exit 1
fi

echo "Linux package install/purge integration test passed."
