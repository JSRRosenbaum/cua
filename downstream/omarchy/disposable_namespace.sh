#!/usr/bin/env bash
# Run a paired candidate in a private mount namespace. Never bind into the host.
set -euo pipefail

usage() {
  printf 'usage: %s ASSEMBLED_ROOT KIT_SHA256 [--join COMPOSITOR_PID] -- COMMAND [ARGS...]\n' "$0" >&2
  exit 2
}

(( $# >= 4 )) || usage
assembled="$(realpath -e -- "$1")"
kit_sha="$2"
shift 2
join_pid=''
if [[ "${1:-}" == --join ]]; then
  (( $# >= 4 )) || usage
  join_pid="$2"
  [[ "$join_pid" =~ ^[1-9][0-9]*$ ]] || usage
  shift 2
fi
[[ "${1:-}" == -- ]] || usage
shift
(( $# > 0 )) || usage
[[ "$kit_sha" =~ ^[a-f0-9]{64}$ ]] || usage
uid="$(id -u)"
gid="$(id -g)"
lab_root="$(realpath -e /tmp/cua-omarchy-lab)"
[[ "$uid" != 0 && "$lab_root" == /tmp/cua-omarchy-lab &&
   -n "${HOME:-}" && -n "${XDG_RUNTIME_DIR:-}" ]] || {
  printf 'error: a disposable private HOME/runtime and unprivileged caller are required\n' >&2
  exit 1
}
HOME="$(realpath -e -- "$HOME")"
XDG_RUNTIME_DIR="$(realpath -e -- "$XDG_RUNTIME_DIR")"
[[ "$HOME" == "$lab_root/home" && "$XDG_RUNTIME_DIR" == /tmp/d ]] || {
  printf 'error: candidate HOME and short private runtime must match the reviewed lab\n' >&2
  exit 1
}
for name in CONFIG CACHE DATA STATE; do
  variable="XDG_${name}_HOME"
  value="${!variable:-}"
  expected="$(printf '%s' "$name" | tr '[:upper:]' '[:lower:]')"
  [[ -n "$value" && "$(realpath -e -- "$value")" == "$lab_root/$expected" ]] || {
    printf 'error: %s must resolve within the reviewed private lab\n' "$variable" >&2
    exit 1
  }
  printf -v "$variable" '%s' "$lab_root/$expected"
done
for private_dir in "$lab_root" "$HOME" "$XDG_RUNTIME_DIR" \
                   "$XDG_CONFIG_HOME" "$XDG_CACHE_HOME" "$XDG_DATA_HOME" "$XDG_STATE_HOME"; do
  [[ -d "$private_dir" && ! -L "$private_dir" &&
     "$(stat -c %u -- "$private_dir")" == "$uid" &&
     "$(stat -c %a -- "$private_dir")" == 700 ]] || {
    printf 'error: disposable directories must be owned mode-0700 real directories\n' >&2
    exit 1
  }
done
# Hyprland appends a 40-hex revision and two decimal fields to its IPC path.
# Reserve 19 digits for each field; sun_path has 108 bytes including the NUL.
max_signature='ffffffffffffffffffffffffffffffffffffffff_9223372036854775807_9223372036854775807'
socket2_path="$XDG_RUNTIME_DIR/hypr/$max_signature/.socket2.sock"
(( ${#socket2_path} < 108 )) || {
  printf 'error: private runtime is too long for Hyprland IPC sockets\n' >&2
  exit 1
}
if [[ -n "${DBUS_SESSION_BUS_ADDRESS:-}" &&
      "$DBUS_SESSION_BUS_ADDRESS" != unix:path=/tmp/dbus-* &&
      "$DBUS_SESSION_BUS_ADDRESS" != unix:path="$lab_root"/* ]]; then
  printf 'error: host user-session bus must never enter a disposable candidate\n' >&2
  exit 1
fi
for name in usr/bin/Hyprland usr/bin/hyprctl usr/bin/cua-driver \
            usr/bin/cua-omarchy \
            usr/lib/cua/hyprland/cua-hyprland-plugin.so \
            usr/lib/systemd/user/cua-driver-omarchy.service \
            usr/share/cua-hyprland-plugin/KIT-PROVENANCE.json \
            usr/share/cua-driver-omarchy/BUILD-PROVENANCE.json \
            usr/share/cua-driver-omarchy/PROFILE.json \
            usr/share/cua-omarchy/hypr/cua-input.lua \
            usr/share/cua-omarchy/menu-items.json \
            usr/share/licenses/cua-omarchy-integration/LICENSE.md \
            usr/share/hyprland-cua/PROFILE.json \
            usr/share/cua-hyprland-plugin/profile_verify.py; do
  [[ -f "$assembled/$name" && ! -L "$assembled/$name" ]] || {
    printf 'error: missing candidate payload: %s\n' "$name" >&2
    exit 1
  }
done
kit_file="$assembled/usr/share/cua-hyprland-plugin/KIT-PROVENANCE.json"
verifier="$assembled/usr/share/cua-hyprland-plugin/profile_verify.py"
[[ "$(sha256sum "$kit_file" | cut -d' ' -f1)" == "$kit_sha" ]] || {
  printf 'error: assembled provenance differs from reviewed kit SHA\n' >&2
  exit 1
}
verifier_sha="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["verifier_sha256"])' \
  "$kit_file")"
[[ "$verifier_sha" =~ ^[a-f0-9]{64}$ &&
   "$(sha256sum "$verifier" | cut -d' ' -f1)" == "$verifier_sha" ]] || {
  printf 'error: assembled verifier code differs from reviewed kit\n' >&2
  exit 1
}
python3 "$assembled/usr/share/cua-hyprland-plugin/profile_verify.py" verify-consumer \
  --kit "$assembled/usr/share/cua-hyprland-plugin" \
  --kit-sha256 "$kit_sha" \
  --consumer "$assembled/usr/lib/cua/hyprland/cua-hyprland-plugin.so" \
  --compositor "$assembled/usr/bin/Hyprland" \
  --driver "$assembled/usr/bin/cua-driver" \
  --driver-provenance "$assembled/usr/share/cua-driver-omarchy/BUILD-PROVENANCE.json"
python3 - "$assembled" <<'PY'
import hashlib
import json
from pathlib import Path
import re
import sys

root = Path(sys.argv[1])
profile = json.loads((root / "usr/share/cua-hyprland-plugin/PROFILE.json").read_text())
recipe = root / "integration-kit/PKGBUILD"
if not recipe.is_file() or recipe.is_symlink() or recipe.parent.is_symlink():
    raise SystemExit("error: immutable integration kit recipe missing")
digest = lambda path: hashlib.sha256(path.read_bytes()).hexdigest()
if digest(recipe) != profile["integration"]["recipe_sha256"]:
    raise SystemExit("error: integration recipe differs from reviewed profile")
match = re.search(r"(?ms)^sha256sums=\((.*?)\)", recipe.read_text())
if not match or not re.fullmatch(r"\s*(?:'[0-9a-f]{64}'\s*){5}", match.group(1)):
    raise SystemExit("error: integration recipe has no five literal SHA-256 pins")
pins = re.findall(r"'([0-9a-f]{64})'", match.group(1))
payloads = (
    "usr/bin/cua-omarchy",
    "usr/lib/systemd/user/cua-driver-omarchy.service",
    "usr/share/cua-omarchy/hypr/cua-input.lua",
    "usr/share/cua-omarchy/menu-items.json",
    "usr/share/licenses/cua-omarchy-integration/LICENSE.md",
)
for name, expected in zip(payloads, pins, strict=True):
    path = root / name
    if not path.is_file() or path.is_symlink() or digest(path) != expected:
        raise SystemExit(f"error: integration payload differs from pinned recipe: {name}")
hyprctl = root / "usr/bin/hyprctl"
if not hyprctl.is_file() or hyprctl.is_symlink() or \
        digest(hyprctl) != profile["compositor"]["hyprctl_sha256"]:
    raise SystemExit("error: hyprctl differs from reviewed compositor stage")
PY

display=''
auth=''
if [[ -n "$join_pid" ]]; then
  [[ -r "/proc/$join_pid/ns/mnt" &&
     "$(readlink -- "/proc/$join_pid/ns/mnt")" != "$(readlink -- /proc/self/ns/mnt)" &&
     "$(stat -c %u -- "/proc/$join_pid")" == "$uid" ]] || {
    printf 'error: candidate compositor PID is not an owned private namespace\n' >&2
    exit 1
  }
  signature="${HYPRLAND_INSTANCE_SIGNATURE:-}"
  [[ "$signature" =~ ^[0-9a-f]{40}_[0-9]+_[0-9]+$ ]] || {
    printf 'error: joined session requires an explicit Hyprland instance signature\n' >&2
    exit 1
  }
  revision="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["compositor"]["revision"])' \
    "$assembled/usr/share/cua-hyprland-plugin/PROFILE.json")"
  [[ "${signature%%_*}" == "$revision" ]] || {
    printf 'error: selected instance does not advertise candidate revision\n' >&2
    exit 1
  }
  lock="$XDG_RUNTIME_DIR/hypr/$signature/hyprland.lock"
  [[ -f "$lock" && ! -L "$lock" &&
     "$(realpath -e -- "$lock")" == "$lock" ]] || {
    printf 'error: selected private compositor lock missing\n' >&2
    exit 1
  }
  mapfile -t selected < "$lock"
  [[ "${selected[0]:-}" == "$join_pid" && "${selected[1]:-}" == "${WAYLAND_DISPLAY:-}" &&
     "$(stat -Lc '%d:%i' -- "/proc/$join_pid/exe")" == \
       "$(stat -Lc '%d:%i' -- "$assembled/usr/bin/Hyprland")" &&
     "$(sha256sum "/proc/$join_pid/exe" | cut -d' ' -f1)" == \
       "$(sha256sum "$assembled/usr/bin/Hyprland" | cut -d' ' -f1)" ]] || {
    printf 'error: selected PID/executable/display does not match candidate session\n' >&2
    exit 1
  }
  for name in usr/bin/Hyprland usr/bin/hyprctl usr/bin/cua-driver \
              usr/bin/cua-omarchy \
              usr/lib/cua/hyprland/cua-hyprland-plugin.so \
              usr/lib/systemd/user/cua-driver-omarchy.service \
              usr/share/cua-hyprland-plugin/PROFILE.json \
              usr/share/cua-hyprland-plugin/KIT-PROVENANCE.json \
              usr/share/cua-driver-omarchy/BUILD-PROVENANCE.json \
              usr/share/cua-driver-omarchy/PROFILE.json \
              usr/share/cua-omarchy/hypr/cua-input.lua \
              usr/share/cua-omarchy/menu-items.json \
              usr/share/hyprland-cua/PROFILE.json; do
    [[ "$(sha256sum "/proc/$join_pid/root/$name" | cut -d' ' -f1)" == \
       "$(sha256sum "$assembled/$name" | cut -d' ' -f1)" ]] || {
      printf 'error: joined namespace contains a different %s\n' "$name" >&2
      exit 1
    }
  done
  if [[ -n "${DISPLAY:-}" ]]; then
    [[ "$DISPLAY" =~ ^:[0-9]+$ ]] || {
      printf 'error: joined XWayland display must be a local display number\n' >&2
      exit 1
    }
    mapfile -t xwayland_children < <(pgrep -P "$join_pid" -x Xwayland)
    [[ "${#xwayland_children[@]}" == 1 &&
       "$(stat -c %u -- "/proc/${xwayland_children[0]}")" == "$uid" ]] || {
      printf 'error: no unique XWayland child belongs to selected compositor\n' >&2
      exit 1
    }
    mapfile -d '' -t xwayland_args < "/proc/${xwayland_children[0]}/cmdline"
    [[ "${xwayland_args[0]##*/}" == Xwayland && "${xwayland_args[1]:-}" == "$DISPLAY" ]] || {
      printf 'error: XWayland display does not belong to selected compositor\n' >&2
      exit 1
    }
    display="$DISPLAY"
    if [[ -n "${XAUTHORITY:-}" ]]; then
      auth="$(realpath -e -- "$XAUTHORITY")"
      [[ ( "$auth" == "$lab_root"/* || "$auth" == "$XDG_RUNTIME_DIR"/* ) &&
         -f "$auth" && ! -L "$auth" &&
         "$(stat -c %u -- "$auth")" == "$uid" &&
         "$(stat -c %a -- "$auth")" == 600 ]] || {
        printf 'error: XWayland authority must be a private owned credential\n' >&2
        exit 1
      }
    fi
    for (( i=0; i<${#xwayland_args[@]}; i++ )); do
      if [[ "${xwayland_args[i]}" == -auth ]]; then
        [[ -n "$auth" && "$(realpath -e -- "${xwayland_args[i+1]:-}")" == "$auth" ]] || {
          printf 'error: XWayland authority differs from selected compositor\n' >&2
          exit 1
        }
      fi
    done
  fi
else
  signature=''
fi

# No sudo -E: pass only the private display/session environment explicitly.
session_env=(PATH=/usr/bin:/bin HOME="$HOME" XDG_RUNTIME_DIR="$XDG_RUNTIME_DIR"
             XDG_CONFIG_HOME="${XDG_CONFIG_HOME:-}" XDG_CACHE_HOME="${XDG_CACHE_HOME:-}"
             XDG_DATA_HOME="${XDG_DATA_HOME:-}" XDG_STATE_HOME="${XDG_STATE_HOME:-}"
             HYPRLAND_INSTANCE_SIGNATURE="$signature"
             WAYLAND_DISPLAY="${WAYLAND_DISPLAY:-}"
             DBUS_SESSION_BUS_ADDRESS="${DBUS_SESSION_BUS_ADDRESS:-}"
             LIBSEAT_BACKEND=noop AQ_DRM_DEVICES=/dev/dri/cua-lab-no-device)
if [[ -n "$display" ]]; then
  session_env+=(DISPLAY="$display")
fi
if [[ -n "$auth" ]]; then
  session_env+=(XAUTHORITY="$auth")
fi
if [[ -n "$join_pid" ]]; then
  exec sudo -n nsenter --target "$join_pid" --mount -- bash -ceu '
    user=$1 group=$2
    shift 2
    exec setpriv --reuid="$user" --regid="$group" --init-groups -- env -i "$@"
  ' bash "$uid" "$gid" "${session_env[@]}" "$@"
else
  # Overlay only these directories in the private mount namespace. The host
  # lacks candidate CLI/metadata/unit mountpoints; lowerdirs retain resources.
  overlay_dir="$(mktemp -d "$lab_root/.share-overlay.XXXXXXXX")"
  trap 'rmdir -- "$overlay_dir"' EXIT
  sudo -n unshare --mount --fork -- bash -ceu '
    assembled=$1 user=$2 group=$3 overlay_dir=$4
    shift 4
    mount --make-rprivate /
    mount -t tmpfs -o size=16m,mode=0700,nosuid,nodev tmpfs "$overlay_dir"
    # The nested Wayland backend needs render nodes, not the host input seat.
    # Hide all current and future evdev nodes in this private mount namespace.
    [[ -d /dev/input && ! -L /dev/input &&
       -c /dev/uinput && ! -L /dev/uinput ]] || {
      printf "error: expected input device mountpoints are unavailable\n" >&2
      exit 1
    }
    mount -t tmpfs -o size=64k,mode=000,nosuid,nodev,noexec tmpfs /dev/input
    : > "$overlay_dir/blocked-uinput"
    chmod 000 "$overlay_dir/blocked-uinput"
    mount --bind "$overlay_dir/blocked-uinput" /dev/uinput
    mount -o remount,bind,ro /dev/uinput
    for entry in "share:/usr/share" "bin:/usr/bin" "units:/usr/lib/systemd/user"; do
      name=${entry%%:*}
      directory=${entry#*:}
      mkdir "$overlay_dir/$name.upper" "$overlay_dir/$name.work"
      mount -t overlay overlay -o \
        "lowerdir=$directory,upperdir=$overlay_dir/$name.upper,workdir=$overlay_dir/$name.work" \
        "$directory"
    done
    for package_dir in cua-driver-omarchy hyprland-cua cua-omarchy; do
      mkdir -p "/usr/share/$package_dir"
      mount --bind "$assembled/usr/share/$package_dir" "/usr/share/$package_dir"
    done
    mount --bind "$assembled/usr/bin/Hyprland" /usr/bin/Hyprland
    mount --bind "$assembled/usr/bin/hyprctl" /usr/bin/hyprctl
    mount --bind "$assembled/usr/bin/cua-driver" /usr/bin/cua-driver
    : > /usr/bin/cua-omarchy
    mount --bind "$assembled/usr/bin/cua-omarchy" /usr/bin/cua-omarchy
    : > /usr/lib/systemd/user/cua-driver-omarchy.service
    mount --bind "$assembled/usr/lib/systemd/user/cua-driver-omarchy.service" \
      /usr/lib/systemd/user/cua-driver-omarchy.service
    mount --bind "$assembled/usr/lib/cua/hyprland/cua-hyprland-plugin.so" \
      /usr/lib/cua/hyprland/cua-hyprland-plugin.so
    mount --bind "$assembled/usr/share/cua-hyprland-plugin" /usr/share/cua-hyprland-plugin
    exec setpriv --reuid="$user" --regid="$group" --init-groups -- env -i "$@"
  ' bash "$assembled" "$uid" "$gid" "$overlay_dir" "${session_env[@]}" "$@"
fi
