# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project
aims to follow [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

This project has not been published to crates.io and carries no git tags, so the version below has no
release date. Entries for it were reconstructed from git history and are not exhaustive; anything
before the crate was split into its own package (2026-02-05) is recorded only in git.

## [Unreleased]

### Added

- `README.md`, included into the crate documentation via `#![doc = include_str!]` so its examples run
  as doctests.
- `README.ja.md`, a Japanese translation. Not included in the rendered crate documentation, but its
  Rust examples run as doctests so it cannot drift from `README.md`.
- `docs/format.md` — on-disk layout and the uniform-separator-count invariant that the record lookup
  depends on.
- `docs/design/2026-08-23-append-and-update.md` — analysis of adding append and in-place record
  update.
- `examples/` — compress, cat, and an inspect example that verifies the invariant.
- Rustdoc for every public item, with `#![warn(missing_docs)]` enabled to keep it that way.
- A rustdoc note on `old_cnt_of_separetor_in_frame_via_buf` marking it superseded by the
  `memchr`-based `cnt_of_separetor_in_frame_via_buf`.
- Package metadata: description, repository, keywords, categories, `rust-version`, and docs.rs
  configuration.
- MIT licensing (`LICENSE`).

## [0.2.0]

### Added

- `cat` subcommand: read a range of records by index.
- `inspect` subcommand: per-frame layout as text or JSON, with `--no-fast-mode` to count separators
  in every frame instead of extrapolating from the first.
- `--cnt-of-separator-per-frame` to pin records per frame instead of auto-detecting.
- `--limit-multiplier` to bound how far past the frame target the separator search may run.
- `--keep-cnt-of-separators-in-frame`, which maintains a uniform separator count across frames.

### Changed

- Separator search and counting use `memchr::memmem::Finder`.
- Compressed output is moved into place with `reflink_copy::reflink_or_copy`, falling back to a byte
  copy when the filesystem has no reflink support.

### Fixed

- Seek table corruption when the framing retry loop rewound the writer.
- Data loss when a record was larger than the frame size target.
- Separator counts not being collected per frame.
