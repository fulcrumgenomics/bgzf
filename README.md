# bgzf

<p align="center">
  <a href="https://github.com/fulcrumgenomics/bgzf/actions?query=workflow%3ACheck"><img src="https://github.com/fulcrumgenomics/bgzf/actions/workflows/build_and_test.yml/badge.svg" alt="Build Status"></a>
  <img src="https://img.shields.io/crates/l/bgzf.svg" alt="license">
  <a href="https://crates.io/crates/bgzf"><img src="https://img.shields.io/crates/v/bgzf.svg?colorB=319e8c" alt="Version info"></a><br>
</p>

This library provides both high level readers and writers for the BGZF format as well as lower level compressor and decompressor functions.

Bgzf is a multi-gzip format that adds an extra field to the header indicating how large the complete block (with header and footer) is.

## Documentation and Examples

Please see the generated [Rust Docs](https://docs.rs/bgzf).

## Benchmarks

Run the compression benchmarks with:

```bash
cargo bench
```

This runs [Criterion](https://github.com/bheisler/criterion.rs) benchmarks measuring:
- Single block compression at various levels
- Writer throughput
