#!/usr/bin/env bash
#
# Render the README screenshots: the popup after Proofread, in the dark and
# light themes, as assets/screenshot-{dark,light}.png.
#
#   nix develop --command scripts/screenshots.sh [output-dir]   # from the repository root
#
# Everything runs in an isolated headless sway session with its own runtime
# directory, so keystrokes and the clipboard never touch your desktop, and a
# running Quillway daemon is left alone. Needs the default model installed
# (`quillway models install qwen3.5-4b`) and a GPU with a Mesa driver (Intel, AMD, …); the
# render node is picked from /dev/dri (override with QUILLWAY_RENDER_NODE=…).
#
# The popup pixels are exactly what Quillway draws; the backdrop, a soft drop
# shadow (Quillway's own shadow is turned off) and the framing are added.

set -euo pipefail

for tool in nix sway grim wtype wl-copy magick pngquant oxipng; do
  command -v "$tool" > /dev/null || { echo "missing $tool: run this inside \`nix develop\`" >&2; exit 1; }
done
[ -f flake.nix ] || { echo "run this from the repository root" >&2; exit 1; }
mesa=${QUILLWAY_SCREENSHOT_MESA:-}
[ -d "$mesa/lib/gbm" ] || { echo "QUILLWAY_SCREENSHOT_MESA isn't set: run this inside \`nix develop\`" >&2; exit 1; }
render_node=${QUILLWAY_RENDER_NODE:-$(find /dev/dri -name 'renderD*' 2>/dev/null | sort | head -n 1)}
[ -e "$render_node" ] || { echo "no GPU render node in /dev/dri; set QUILLWAY_RENDER_NODE" >&2; exit 1; }

out=${1:-assets}
mkdir -p "$out"
out=$(realpath "$out")
pkg=$(nix build .#default --no-link --print-out-paths)
q="$pkg/bin/quillway"
work=$(mktemp -d)
# Wayland socket paths are limited to 108 bytes, so keep the runtime dir short.
run=$(mktemp -d /tmp/qw.XXXXXX)
trap 'rm -rf "$work" "$run"' EXIT
mkdir -p "$work/cfg/quillway"

W=2000 H=1240 # the headless output, in pixels (scale 2)
text="hey team, quick update on the launch. its moved to thursday because their are still a few bugs in the payment flow. lets do one final check tomorow morning before we ship it."

backdrop() { # theme -> $work/bg-<theme>.png
  local points
  if [ "$1" = dark ]; then
    points="1000,470 #43349a  0,1240 #0f1c3d  2000,1240 #120d24  0,0 #160f2c  2000,0 #1a1240"
  else
    points="1000,470 #e6e0ff  0,1240 #ffe9dd  2000,1240 #dfe8fb  0,0 #f1eefa  2000,0 #ece7fb"
  fi
  # Shepards interpolation: smooth, seamless glows; the noise prevents banding.
  magick -size ${W}x${H} xc: -sparse-color Shepards "$points" \
    -attenuate 0.25 +noise Gaussian -blur 0x0.5 -depth 8 "$work/bg-$1.png"
}

capture() { # theme -> $work/review-<theme>.png
  local t=$1 sp
  printf '[model]\nactive = "qwen3.5-4b"\n[ui]\ntheme = "%s"\ntop_margin = 150\nclient_shadow = false\n[behavior]\nrecent_secs = 600\n' \
    "$t" > "$work/cfg/quillway/config.toml"
  printf 'output HEADLESS-1 resolution %sx%s scale 2\noutput HEADLESS-1 bg %s fill\n' $W $H "$work/bg-$t.png" > "$work/sway.conf"
  (
    export XDG_RUNTIME_DIR="$run" XDG_CONFIG_HOME="$work/cfg" WLR_BACKENDS=headless WLR_RENDERER=gles2 \
      WLR_RENDER_DRM_DEVICE="$render_node" WLR_LIBINPUT_NO_DEVICES=1
    # Use the Mesa drivers from this flake's nixpkgs for sway, quillway and
    # llama-server alike. The host's drivers (e.g. /run/opengl-driver on NixOS) only
    # work with GPU libraries from the very same nixpkgs build, which sway's libgbm
    # and libEGL here aren't. (Software rendering isn't an option either: iced's
    # software path can't draw the popup's transparent rounded corners.)
    vk_drivers=$(printf '%s:' "$mesa"/share/vulkan/icd.d/*.json)
    export GBM_BACKENDS_PATH="$mesa/lib/gbm" LIBGL_DRIVERS_PATH="$mesa/lib/dri" \
      __EGL_VENDOR_LIBRARY_FILENAMES="$mesa/share/glvnd/egl_vendor.d/50_mesa.json" \
      VK_DRIVER_FILES="$vk_drivers"
    # The dev shell's LD_LIBRARY_PATH (for `cargo run`) points sway at other GPU
    # libraries; the Nix-built quillway carries its own library paths anyway.
    unset WAYLAND_DISPLAY DISPLAY SWAYSOCK LD_LIBRARY_PATH
    sway -c "$work/sway.conf" > "$work/sway-$t.log" 2>&1 &
    sp=$!
    # On any exit, stop this session too, or a failed run leaves sway and a daemon
    # (with its llama-server) running where nothing else can see them.
    trap 'kill "$sp" ${dp:+"$dp"} 2> /dev/null || true' EXIT
    for _ in $(seq 1 50); do [ -S "$run/wayland-1" ] && break; sleep 0.1; done
    [ -S "$run/wayland-1" ] || { echo "sway didn't start:" >&2; tail -n 15 "$work/sway-$t.log" >&2; exit 1; }
    export WAYLAND_DISPLAY=wayland-1
    visible() { "$q" status 2>/dev/null | grep -q 'popup:  visible'; }

    "$q" daemon > "$work/daemon-$t.log" 2>&1 &
    dp=$!
    for _ in $(seq 1 90); do "$q" status 2>/dev/null | grep -q 'engine: ready' && break; sleep 1; done
    printf '%s' "$text" | wl-copy
    sleep 0.5
    "$q" toggle
    sleep 3 # map, resize and take keyboard focus
    visible || { echo "the popup didn't open" >&2; exit 1; }
    grim "$work/compose-$t.png"
    # The first keypress can race the keyboard focus: retry until the frame changes.
    for _ in 1 2 3; do
      wtype -k Shift_L -M ctrl 1 -m ctrl # Ctrl+1: Proofread (a throwaway first key, see RUNBOOK)
      sleep 1.5
      grim "$work/probe-$t.png"
      cmp -s "$work/probe-$t.png" "$work/compose-$t.png" || break
    done
    sleep 10 # generation
    grim "$work/review-$t.png"
    "$q" quit
    sleep 1
    kill "$sp"
    wait "$sp" 2>/dev/null || true
  )
}

finish() { # theme -> $out/screenshot-<theme>.png
  local t=$1 col wide tight bbox w h x y
  if [ "$t" = dark ]; then col='#05030d' wide=0.62 tight=0.30; else col='#3b2f78' wide=0.20 tight=0.12; fi
  cd "$work"
  # Popup alpha: the per-pixel colour difference from the known backdrop (keeps anti-aliased edges).
  magick "review-$t.png" "bg-$t.png" -compose difference -composite -separate -evaluate-sequence max \
    -level 0%,2% -morphology Close Disk:2 "mask-$t.png"
  # A wide ambient shadow and a tight contact shadow under the untouched popup pixels.
  magick "bg-$t.png" \
    \( -size ${W}x${H} xc:"$col" \( "mask-$t.png" -blur 0x44 -roll +0+34 -evaluate multiply $wide \) -alpha off -compose CopyOpacity -composite \) -compose over -composite \
    \( -size ${W}x${H} xc:"$col" \( "mask-$t.png" -blur 0x5 -roll +0+3 -evaluate multiply $tight \) -alpha off -compose CopyOpacity -composite \) -compose over -composite \
    "review-$t.png" "mask-$t.png" -compose over -composite "shadowed-$t.png"
  bbox=$(magick "mask-$t.png" -threshold 50% -format '%@' info:) # WxH+X+Y
  read -r w h x y <<< "${bbox//[x+]/ }"
  magick "shadowed-$t.png" -crop $((w + 300))x$((h + 300))+$((x - 150))+$((y - 130)) +repage "final-$t.png"
  pngquant --quality=80-95 --speed 1 --force --output "$out/screenshot-$t.png" "final-$t.png"
  oxipng -q -o 4 --strip safe "$out/screenshot-$t.png"
  cd - > /dev/null
}

for t in dark light; do
  backdrop $t
  capture $t
  finish $t
  echo "wrote $out/screenshot-$t.png"
done
