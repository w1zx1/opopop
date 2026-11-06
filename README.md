# opopop

A tiny Rust app: a fullscreen video kiosk that plays a video with sound on
startup and refuses to let you leave it.

## What it does

* Goes fullscreen across **all** monitors at once, on top of every window,
  with no border and no close button. It does not appear in `Alt+Tab`.
* Plays the video **with sound**, in a loop.
* Hooks the keyboard and swallows **every** key: `Alt+Tab`, `Alt+F4`, `Win`,
  `Ctrl+Esc`, `Ctrl+W`, `F12`, virtual desktop switching, and so on.
* Won't let the window fall into the background: every 150 ms it becomes
  topmost again and grabs focus.
* Keeps the display and system awake while the video is playing.
* Hides the cursor.
* **The only way out is `Ctrl+Shift+Esc`** — the window closes itself and the
  program shuts down cleanly.

## Build and run

```powershell
cargo build --release
.\target\release\opopop.exe
```

For debugging: `cargo run` (the debug build has a console, the release build
does not).

## Self-contained

The video `opopop.mp4` is embedded directly into the `.exe` via
`include_bytes!`. The finished `target\release\opopop.exe` (~2.2 MB) can be
copied anywhere and run without the mp4 file and without any DLLs: it only uses
Media Foundation, which is built into Windows.

On first launch the video is extracted to
`%TEMP%\opopop_embedded_video.mp4` (Media Foundation can only play a file, not
data from memory).

## Changing the video

Replace `opopop.mp4` in the project root and rebuild:

```powershell
cargo build --release
```

The video must be in a format Media Foundation understands (`.mp4`/H.264 is
the safest choice).

## How it works

| Task | Mechanism |
| --- | --- |
| Fullscreen, topmost window | `CreateWindowExW` with `WS_POPUP`, `WS_EX_TOPMOST`, size = union of all monitors |
| Video + audio, no external dependencies | Media Foundation, `MFPCreateMediaPlayer` (renders into our window, audio via WASAPI) |
| Playback loop | a timer checks the player state; when it ends, `SetPosition(0)` + `Play()` |
| Keyboard blocking | a global low-level hook `WH_KEYBOARD_LL` that returns `1` (swallow) for every key |
| Exit | the hook sees `Ctrl+Shift+Esc` → flag → `PostQuitMessage` |
| Holding focus | 150 ms timer: `SetWindowPos(TOPMOST)` + `SetForegroundWindow` (+ `AttachThreadInput`) |

There are only two dependencies: `windows` (the official Win32 bindings) and
its internal crates. No third-party players, ffmpeg, etc.

## Files

```
Cargo.toml      — manifest (windows dependency + release profile)
src/main.rs     — the whole program
opopop.mp4      — the video, embedded into the exe at build time
target/release/opopop.exe — the finished self-contained binary
```
