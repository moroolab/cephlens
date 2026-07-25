# Changelog

Notable changes are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project uses
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.5] - 2026-07-25

### Added

- Added a flow view, opened with `m`, that shows the OSD, placement group, and
  object behind the ops in the trace buffer. radostrace lines already name all
  three, so the view needs no extra remote command; with only osdtrace running
  it collapses to OSD and placement group. `o` orders it by op count or by
  latency and `s` reverses the order. Each row also carries the mean op size,
  which separates a slow heavy request from one that is slow for the little it
  asks for. A read reports the length it requested rather than bytes returned,
  so a client that always asks for 4MiB reports 4MiB whatever the object holds.
- Insights now name the checks behind a `HEALTH_WARN` or `HEALTH_ERR` instead of
  telling the operator to go run `ceph health detail`. `ceph -s` already carried
  them, so this costs no extra remote command.
- The OSD table shows the commit and apply latency from `ceph osd perf`, which
  puts Ceph's own view of a slow OSD next to the eBPF numbers. The query is
  optional, so a cluster that refuses it keeps the rest of its status.
- The node table shows the share of the last 10s that a host spent stalled on IO,
  read from `/proc/pressure/io`, and an insight fires past 5%. Unlike a device
  utilization figure this needs no OSD to block device mapping to be meaningful.

### Fixed

- `x` now clears every captured trace source rather than osdtrace alone.
- Node readiness no longer breaks on hosts without `ceph-osd` processes. The
  remote OSD count fell back through `pgrep -c ... || echo 0`, which emitted two
  lines because `pgrep -c` prints `0` and exits 1 on no match. The extra line
  made the node stream payload invalid JSON on mon-only hosts.

### Changed

- The cluster status stream now runs its three admin queries at once instead of
  one after another. On a four node microceph cluster the tick period dropped
  from 2278ms to 1400ms at the default `refresh_secs = 1`, with the same payload.
  `refresh_secs` is the pause between ticks, not the period, and the README now
  says so.
- `doctor` and `snapshot` now probe hosts concurrently instead of one at a time,
  with at most 8 hosts in flight. Each host still sees one SSH connection at a
  time and the doctor report keeps its previous order.
- Raised the declared minimum supported Rust version to 1.88. The source uses
  let chains, which are stable only from 1.88, so builds on 1.85 through 1.87
  failed despite the previous `rust-version = "1.85"`. CI now builds against the
  declared MSRV.

## [0.1.4] - 2026-07-07

### Added

- Added `cephlens report <session>` to export recorded sessions as Markdown.
- Live TUI sessions now write `report.md` on exit when snapshots were recorded.
- Added `cephlens doctor` for SSH, sudo, Ceph CLI, and tracer preflight checks.
- Added `cephlens lab` to run a short benchmark with optional trace capture and
  write a session report.

### Changed

- Moved diagnostic insight rules out of the TUI so CLI reports reuse the same
  checks.
- Replaced MicroCeph-specific node readiness wording with Ceph
  version/deployment data.

## [0.1.3] - 2026-07-06

### Added

- Recorded raw trace logs under live TUI session directories.
- Documented sudo whitelist setup for trace and install commands.

## [0.1.2] - 2026-07-05

### Added

- Added `cephlens --version` so installed binaries report the Cargo package
  version.

## [0.1.1] - 2026-07-05

### Security

- Hardened SSH destination validation to reject option-like, empty, or
  whitespace-containing host values before invoking `ssh`.
- Pinned bundled cephtrace artifacts to `v1.6` and verified their SHA256
  digests during release builds.

### Changed

- Improved bundled cephtrace GPL notice and release artifact attribution.

## [0.1.0] - 2026-07-05

Initial release.

- SSH-driven Ceph investigation TUI: live cluster, OSD, and host status.
- Three eBPF trace sources driven through cephtrace: osdtrace (OSD server side),
  kfstrace (CephFS MDS client), and radostrace (RADOS client).
- Trace controls with view switching and start/stop confirmation, per-source and
  cross-source operator insights, in-TUI config editing, and session replay.
- Cross-platform controller (Linux, macOS, Windows) with cargo-dist release
  archives that bundle the cephtrace tracers.

[Unreleased]: https://github.com/xtrusia/cephlens/compare/v0.1.5...HEAD
[0.1.5]: https://github.com/xtrusia/cephlens/compare/v0.1.4...v0.1.5
[0.1.4]: https://github.com/xtrusia/cephlens/compare/v0.1.3...v0.1.4
[0.1.3]: https://github.com/xtrusia/cephlens/compare/v0.1.2...v0.1.3
[0.1.2]: https://github.com/xtrusia/cephlens/compare/v0.1.1...v0.1.2
[0.1.1]: https://github.com/xtrusia/cephlens/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/xtrusia/cephlens/releases/tag/v0.1.0
