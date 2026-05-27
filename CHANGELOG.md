# Changelog

All notable changes to rooms are documented here. Format adapted from [keepachangelog.com](https://keepachangelog.com).

## [Unreleased]

## [0.1.0] — TBD

### Added
- POC substrate: `rooms run --image <ext4> --command <cmd>`, with TAP networking, SSH-to-guest, exit-code propagation.
- M4: outbound HTTPS from guest (Anthropic curl example at `examples/drive-anthropic.sh`).
- Batch 1 productionization: structured `FirecrackerError`, `RoomGuard` cleanup, real `doctor` with `--json`, debootstrap rootfs builder, runner-contract artifact layout.

### Changed
- n/a (first release)

### Fixed
- n/a (first release)

[Unreleased]: https://github.com/itsHabib/rooms/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/itsHabib/rooms/releases/tag/v0.1.0
