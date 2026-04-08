# DISCLAIMER

This is a highly unoptimized, vibe-coded project meant for learning and experimentation. It’s not intended for production use or as a general-purpose cursor effect.

# fire-cursor (Windows)

A Windows-only transparent overlay that hides the system cursor and draws a GPU-accelerated fire-like particle trail at the global mouse position.

## How it works

### Overlay window

- The app creates a single borderless `winit` window sized to the **virtual desktop** (all monitors).
- The window is:
  - **Always on top**
  - **Transparent** (`winit` transparency + DWM glass extension)
  - **Click-through** (so it does not block mouse/keyboard input)

Windows API calls are isolated in [src/window.rs](src/window.rs):

- `GetCursorPos` — reads the global mouse position.
- `ShowCursor(false/true)` — hides and restores the system cursor using a small RAII helper.
- `SetSystemCursor` — replaces common system cursors with a fully transparent cursor while the app runs (most reliable with a click-through overlay).
- `SystemParametersInfoW(SPI_SETCURSORS)` — restores the system cursor scheme on exit.
- `SetWindowLongW(GWL_EXSTYLE, …)` — adds:
  - `WS_EX_LAYERED` (layered window)
  - `WS_EX_TRANSPARENT` (hit-test transparency / click-through)
  - `WS_EX_TOOLWINDOW` (hide from Alt-Tab)
- `SetWindowPos(HWND_TOPMOST, …)` — enforces topmost without activating the window.
- `UpdateLayeredWindow(…, ULW_ALPHA)` — Windows composes a per-pixel alpha bitmap for the overlay (used when `wgpu` surface alpha modes are opaque-only).

### Rendering (wgpu)

- Uses `wgpu` to render at ~60 FPS (`PresentMode::Fifo`).
- On many systems, `wgpu` reports the window surface alpha mode as `Opaque` on Windows.
  To still provide a real overlay, the app uses a **layered window color-key**:
  - the renderer clears to **black**
  - Win32 sets black as the transparent key (`SetLayeredWindowAttributes(..., LWA_COLORKEY)`)
  - particles are drawn with **additive blending** so the keyed background stays clean
- Each particle is rendered as an **instanced quad** in [src/renderer.rs](src/renderer.rs).

### Particle math

Implemented in [src/particles.rs](src/particles.rs):

- Coordinates are in screen pixels.
- Y increases downward, so an upward velocity has **negative Y**.
- Each particle has `age` and `lifetime`:
  - Normalized lifetime: $t = \frac{age}{lifetime}$
- Color is a simple piecewise gradient:
  - yellow → orange → red → transparent
- Motion:
  - randomized initial velocity with upward bias
  - constant upward acceleration
  - simple drag
- Expired particles are removed automatically.

## How to run

Requirements:

- Windows 10+
- Rust stable

Commands:

```powershell
cargo run
```

Close the program via the window close button (Alt+F4 works too).

## Limitations

- Cursor hiding uses the Win32 `ShowCursor` display counter (global). If other apps manipulate the counter, hiding/unhiding can get out of sync.
- The app also uses `SetSystemCursor` to hide the cursor globally. On exit it restores cursors using `SPI_SETCURSORS`, which may briefly reset the cursor scheme (especially if you use a custom cursor theme).
- True per-pixel alpha swapchains via `wgpu` are not consistently available on Windows; this project uses a layered-window color-key fallback to guarantee an overlay.
- Multi-monitor support is based on the virtual desktop rectangle; DPI scaling behavior depends on system settings.
- This is a minimal particle effect (no texture/noise). It’s designed to be easy to extend.

## Extending

The code is split to make adding new effects straightforward:

- [src/window.rs](src/window.rs) — Windows-only overlay and cursor APIs
- [src/renderer.rs](src/renderer.rs) — GPU pipeline + instanced drawing
- [src/particles.rs](src/particles.rs) — particle simulation
- [src/main.rs](src/main.rs) — main loop glue

To add another cursor effect, you can:

- create a new module similar to `particles.rs`
- produce a list of instances for the renderer
- swap the system based on a flag or hotkey
