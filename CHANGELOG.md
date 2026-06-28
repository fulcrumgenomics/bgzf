# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- Store-only (compression level 0) fast paths: `Writer` emits DEFLATE stored blocks directly
  (no libdeflate, no intermediate `BytesMut`), and `Reader` reads a single final stored block
  straight into its decompressed buffer, bypassing libdeflate. Reading incompressible data (which
  deflate stores even at higher levels) benefits automatically.
- `Reader::with_crc_validation(false)` to skip per-block CRC32 verification for faster reads of
  trusted, transient uncompressed streams (CRC validation remains on by default).
- `CompressionLevel` now documents level 0 as "no compression" (stored blocks).

### Fixed
- Reject blocks whose footer ISIZE exceeds the maximum BGZF block size
  (`BgzfError::UncompressedSizeExceeded`) instead of indexing past the fixed-size buffer.
- Reject blocks smaller than a header plus footer.
- Report a truncated trailing block as an error rather than silently treating it as end-of-input.
- Prevent a `u16` `BSIZE` overflow in `Compressor::compress` for near-incompressible blocks
  (previously panicked in debug / wrote a corrupt size in release).
- Reject `blocksize == 0` in `Writer::with_capacity` (previously looped forever).

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
