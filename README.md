# bgzf

<p align="center">
  <a href="https://github.com/fulcrumgenomics/bgzf/actions?query=workflow%3ACheck"><img src="https://github.com/fulcrumgenomics/bgzf/actions/workflows/build_and_test.yml/badge.svg" alt="Build Status"></a>
  <img src="https://img.shields.io/crates/l/bgzf.svg" alt="license">
  <a href="https://crates.io/crates/bgzf"><img src="https://img.shields.io/crates/v/bgzf.svg?colorB=319e8c" alt="Version info"></a><br>
</p>

This library provides both high level readers and writers for the BGZF format as well as lower level compressor and decompressor functions.

Bgzf is a multi-gzip format that adds an extra field to the header indicating how large the complete block (with header and footer) is.


<p>
<a href="https://fulcrumgenomics.com">
<picture>
  <source media="(prefers-color-scheme: dark)" srcset="https://raw.githubusercontent.com/fulcrumgenomics/bgzf/main/.github/logos/fulcrumgenomics-dark.svg">
  <source media="(prefers-color-scheme: light)" srcset="https://raw.githubusercontent.com/fulcrumgenomics/bgzf/main/.github/logos/fulcrumgenomics-light.svg">
  <img alt="Fulcrum Genomics" src="https://raw.githubusercontent.com/fulcrumgenomics/bgzf/main/.github/logos/fulcrumgenomics-light.svg" height="100">
</picture>
</a>
</p>

[Visit us at Fulcrum Genomics](https://www.fulcrumgenomics.com) to learn more about how we can power your Bioinformatics with bgzf and beyond.

<a href="mailto:contact@fulcrumgenomics.com?subject=[GitHub inquiry]"><img src="https://img.shields.io/badge/Email_us-%2338b44a.svg?&style=for-the-badge&logo=gmail&logoColor=white"/></a>
<a href="https://www.fulcrumgenomics.com"><img src="https://img.shields.io/badge/Visit_Us-%2326a8e0.svg?&style=for-the-badge&logo=wordpress&logoColor=white"/></a>

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
