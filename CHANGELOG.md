# Changelog

All notable changes to this project are documented here.
Format: [Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versions follow [SemVer](https://semver.org/).

## [Unreleased]

### Added
- **Battery icon** submenu in the right-click menu: show a small battery glyph, filled to
  the charge level, above or below the number, or hide it (the default). The choice is
  saved under `HKCU\Software\mousebatt`.

### Changed
- The icon is drawn at the tray's actual size (16 px at 100% scaling, 20/24 px when
  scaled; the process is now DPI aware) instead of a 32 px bitmap Windows shrinks.
- Digits are rendered like the taskbar clock: Segoe UI with the system's font smoothing
  (ClearType by default), blended against the taskbar colour next to the tray.
- On a light taskbar the icon uses dark colours (Windows light palette) instead of white,
  and it redraws when switching between light and dark mode.

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
