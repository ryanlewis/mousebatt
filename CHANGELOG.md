# Changelog

All notable changes to this project are documented here.
Format: [Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versions follow [SemVer](https://semver.org/).

## [Unreleased]

### Added
- Pulsar X2A v3 (XS-1 platform, e.g. the X2A v3 eS Mini) on its cable or 8K dongle:
  battery, charging state and the **Polling rate** submenu (125 Hz–8 kHz). These mice
  use a 64-byte feature-report protocol on a different HID collection, so they were
  not detected before.
- **Icon style** submenu in the right-click menu: the digits alone (the default), digits
  with a small battery glyph above or below, or the battery glyph alone — a large glyph
  filled to the charge level with no digits. The choice is saved under
  `HKCU\Software\mousebatt`.

### Changed
- The icon is drawn at the tray's actual size (16 px at 100% scaling, 20/24 px when
  scaled; the process is now DPI aware) instead of a 32 px bitmap Windows shrinks.
- Digits are rendered like the taskbar clock: Segoe UI with the system's font smoothing
  (ClearType by default), blended against the taskbar colour next to the tray.
- On a light taskbar the icon uses dark colours (Windows light palette) instead of white,
  and it redraws when switching between light and dark mode.

### Fixed
- **Start with Windows** is ticked only when the startup entry launches this exe. After
  moving or rebuilding mousebatt in another folder it reads as off, and one click points
  the entry at the new location (previously it stayed ticked on the stale path).

## [0.2.0] - 2026-09-12

### Added
- **Polling rate** submenu in the right-click menu; the current rate is checked and also
  shown in the tooltip. Only rates the current link supports are listed:
  - Pulsar: 125 Hz–8 kHz (1 kHz max on cable, up to 8 kHz on the 8K dongle)
  - VAXEE: 500 Hz–4 kHz on the VXD02 4K dongle (8 kHz on the 8K receiver), 1 kHz max on
    cable or in a "Standard" tracking mode, matching the VAXEE Control Center

## [0.1.0] - 2026-08-23

### Added
- Tray icon showing battery percentage for Pulsar (X3 family, 8K Dongle Gen.2) and VAXEE (XE-S, 4K dongle) mice
- Colour states: normal / charging / low (≤20%) / stale
- Tooltip with model, percentage, charging state and voltage (Pulsar)
- Re-read on device plug/unplug and on resume from sleep; left-click to refresh
- "Start with Windows" toggle in the right-click menu
- Single ~290 KB exe with no dependencies beyond `windows-sys`

[0.2.0]: https://github.com/ryanlewis/mousebatt/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/ryanlewis/mousebatt/releases/tag/v0.1.0
