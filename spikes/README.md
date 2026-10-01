# M0 spikes (throwaway)

## 1. popup: iced 0.14 + iced_layershell 0.19.1 on niri 26.04

Run:
```
cd spikes/popup
nix develop ../.. --command cargo run                            # client-drawn shadow, 32px transparent margin
QW_MARGIN=0 QW_ALPHA=0.7 nix develop ../.. --command cargo run   # surface == panel, for a compositor blur/shadow rule
```

Results (2026-09-30):
- ✅ Builds. Needs `winit-core = "=0.31.0-beta.2"`: Cargo picks beta.3, which breaks `iced_exdevtools` 0.19.1 (non-exhaustive `NativeKeyCode`).
- ✅ Overlay layer, `Exclusive` keyboard, `StartMode::Active` (focused output), transparent background. The rounded panel, hairline border, shadow and chips all render.
- ✅ Text input focused on boot via `iced::widget::operation::focus`; no errors.
- ✅ Typing, Esc, ↵ verified with wtype in the real app. ⏳ fcitx5/IME and blur: manual check by the author.
- Finding: niri blurs the **whole surface**, so the transparent shadow margin would blur as a square box. On niri, use a surface exactly the panel's size plus a `layer-rule` for the rounded clip, shadow and blur (validated with `niri validate`):
  ```kdl
  layer-rule {
      match namespace="^quillway$"
      geometry-corner-radius 16
      shadow { on; softness 28; spread 2; offset x=0 y=12; color "#00000073"; }
      background-effect { blur true; xray false; }
  }
  ```
  For other compositors, keep the client-drawn shadow and set a blur region through ext-background-effect (M6).
- Known gap: the surface height is fixed (211 logical px). Dynamic height (resize the surface to fit the content) comes in M3.

## 2. capture / copy: done in the real app (`quillway-wl`)
- ✅ PRIMARY and CLIPBOARD read through data-control before the surface maps; `--source stdin` over IPC.
- ✅ CLIPBOARD write is served by the daemon thread; read back with `wl-paste` after the popup closed.
- ⏳ Manual paste matrix (foot, Firefox, nvim `"+p`, GTK) and cliphist: author.

## 3. models: see `docs/research/model-eval.md`
- Vulkan iGPU: Qwen3.5 4B 29 tok/s generation, ~0.5 s to first token after warm-up. Zero preambles in 80 cases.
