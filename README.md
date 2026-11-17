# Attack Shark X3 Taskbar Battery Monitor

A minimal, native Windows 11 taskbar widget written in Rust that displays the live battery percentage of the **Attack Shark X3** wireless mouse directly on the taskbar—no bloated vendor hub, Electron, or WebView required.

## Features

- **Native Windows 11 Look:** Transparent `WS_EX_LAYERED` widget rendered with Direct2D + DirectWrite (`Segoe UI Variable Text` / `Segoe UI`) and pixel-aligned vector graphics.
- **2.4 GHz & Wired USB Detection:**
  - Listens passively to the 2.4 GHz receiver (`VID 0x1D57`, `PID 0xFA60`, Usage Page `0x000A`) via Win32 overlapped HID I/O (`ReadFile`).
  - Automatically detects wired USB charging (`PID 0xFA61`) and switches to a charging bolt indicator.
  - Falls back to a dimmed sleep state after 10 seconds without telemetry packets.
- **Smart Taskbar Behavior:**
  - Adapts automatically to Windows dark and light taskbar themes.
  - Supports horizontal drag-and-drop repositioning along the taskbar (saved in `HKCU\Software\AttackSharkX3Battery`).
  - Automatically hides when fullscreen games/videos are active or when an auto-hidden taskbar collapses.
- **Zero Dependencies at Runtime:** Single standalone `.exe` (~153 KB in release) with an optional **Start with Windows** toggle (`HKCU\Software\Microsoft\Windows\CurrentVersion\Run`).

## Protocol Notes (Attack Shark X3)

- **2.4 GHz Receiver:** `VID 0x1D57`, `PID 0xFA60` — pushes 5-byte HID input reports every ~2 seconds on Usage Page `0x000A`:
  `[0x03, 0x10, 0x40, status, level]` (`0x4010` little-endian battery telemetry event).
- **Wired Mode:** `VID 0x1D57`, `PID 0xFA61`.
- **10% Increments:** The Attack Shark X3 firmware reports `level` on a `1–10` scale (`0x01`–`0x0A`), so the percentage updates in **10% steps** (`100%`, `90%`, `80%`, ...). This is a limitation of the mouse's own firmware/MCU rather than the app.

## Building from Source

### Prerequisites
- Windows 10 / 11
- [Rust](https://rustup.rs/) (`x86_64-pc-windows-msvc`) + Visual Studio C++ Build Tools

### Build & Run

```powershell
cargo test
cargo build --release
```

The standalone executable will be generated at `target\release\mousebattery.exe`.

## Usage

1. Run `mousebattery.exe` (no console window opens).
2. `[mouse] 100%` appears directly on the taskbar to the left of the system tray.
3. **Left-drag** horizontally to adjust its position on the taskbar.
4. **Right-click** the widget for the context menu:
   - Live status & last received HID packet (hex)
   - **Start with Windows**
   - **Refresh now**
   - **Reset position**
   - **Exit**
