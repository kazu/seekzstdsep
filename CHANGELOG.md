# Changelog

Generated from the commit log by [git-cliff](https://git-cliff.org). Do not edit it by hand: the
next release overwrites it. What belongs in a release note belongs in a commit message.

Only the crate's own tags (`v*`) mark a release here. The nushell plugin is versioned separately in
`nu_plugin_zstdsep/`, and `nu_v.*` records which revision a nushell version takes.

## [0.7.1] - 2026-09-19

### Changed

- edit: add copy-and-replace append with conflict detection
- cli: add copy-and-replace append option
- file_update: rename append_copy to copy_and_replace
- cli: add copy-and-replace truncate

## [0.7.0] - 2026-09-19

### Changed

- edit: allow appending records with an explicit count per frame

### Documentation

- docs: readme: add a related projects section

## [0.6.0] - 2026-09-18

### Changed

- nu_plugin_zstdsep: support --finder and --finder-arg
- nu_plugin_zstdsep: hook: forward --finder and --finder-arg
- Makefile: add release-plugin for a plugin-only release
- nu_plugin_zstdsep: release 0.5.1
- bench: sweep frame size and level, and chart it
- compress: default the frame size to 128 KiB
- bench: measure the reader's three walks, and profile them
- reader: verify the records per frame as a read walks them
- lib: drop convert_text_to_seekable_zst_reader

### Documentation

- docs: link the README's paths, and add a cat example through the API
- docs: add examples to the functions callers reach for
- docs: record that the nushell plugin has no --finder
- docs: record how flatbuffers support would get its decoder
- docs: record the disagreement between total_records and record

## [0.5.0] - 2026-09-05

### Changed

- seekzstdsep: bench: measure what the compressor holds
- seekzstdsep: take the record boundary as a finder
- seekzstdsep: edit: stop holding a whole frame to count its records

### Documentation

- docs: record that counting a frame's records holds the whole frame
- docs: record how the finder differs from its design, and what a box cost

## [0.4.1] - 2026-09-04

### Fixed

- seekzstdsep: fix the panic on a path that cannot be opened
- seekzstdsep: reader: fix the panic on a separator that occurs nowhere
- seekzstdsep: inspect: fix the panic on a frame that fails to decode
- seekzstdsep: fix the frames a record range is placed in

### Documentation

- docs: re-check every known bug and file it by what it is
- docs: generate CHANGELOG.md with git-cliff

## [0.4.0] - 2026-08-30

### Changed

- edit: refactor append's input and tune frame reading
- edit: add append --input-seekable
- edit: truncate only at a frame boundary
- seekzstdsep: rebuild the retry options with a struct update
- seekzstdsep: choose the zstd compression level
- seekzstdsep: leave one way to read a record range
- seekzstdsep: cat 4% faster, at memory the frame size cannot move
- seekzstdsep: hold the decoder inside the read window
- seekzstdsep: zstdsep: read a record by index through the window
- seekzstdsep: cut a release with one make target

### Documentation

- docs: drop compress --align
- docs: badge the README with version, CI and nushell

### CI

- ci: gather every check under make ci
- ci: cache cargo builds across runs

## [0.3.0] - 2026-08-27

### Changed

- seekzstdsep: seek to any record range in a zstd file
- nu_plugin_zstdsep: build against nushell 0.115
- nu_plugin_zstdsep: add a Japanese README
- Update branches for Rust workflow triggers
- edit: name the frame lookups the operations spell out
- edit: copy a record range out into a second file

### Documentation

- docs: record which nushell a plugin revision supports
- docs: lead the README with what the tool does
- docs: cut the README to what it has to say
- docs: name the prior art the README is closest to
- docs: link the prior art the README names
- docs: replace split and concat with three composable operations

### CI

- ci: run the plugin's tests too
- ci: publish from the workflow instead of from a laptop

