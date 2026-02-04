# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.3.0] - 2026-02-04

### Added
- `Writer::from_path_buffered()` for reduced syscall overhead (#7)
- `Writer::finish()` method for proper EOF handling
- Compression benchmarks using Criterion

### Changed
- Optimized compression with header template and direct footer writes (#5)
- Eliminated buffer zero-fill with unsafe resize for ~5% performance improvement (#6)

### Fixed
- Heap allocation eliminated in `header_inner()`

## [0.2.0] - 2022-03-04

Initial public release.
