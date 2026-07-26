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

for command in apt-get cmp dpkg dpkg-query grep realpath sudo timeout xdg-mime; do
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
sudo -n true

mimeapps="$HOME/.config/mimeapps.list"
mimeapps_existed=0
mimeapps_snapshot="$(mktemp)"
if [[ -e "$mimeapps" ]]; then
  mimeapps_existed=1
  cp "$mimeapps" "$mimeapps_snapshot"
fi
default_before="$(xdg-mime query default image/x-sony-arw 2>/dev/null || true)"
installed=0

cleanup() {
  if [[ "$installed" == "1" ]]; then
    sudo -n dpkg --purge viewr >/dev/null || true
  fi
  rm -f "$mimeapps_snapshot"
}
trap cleanup EXIT

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
grep -Fxq \
  'image/x-sony-arw=viewr-arw.desktop;' \
  /usr/share/applications/mimeinfo.cache || {
    echo "desktop-file-utils did not register the installed ARW handler" >&2
    exit 1
  }
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
