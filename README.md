# bgzf

<p align="center">
  <a href="https://github.com/fulcrumgenomics/bgzf/actions?query=workflow%3ACheck"><img src="https://github.com/fulcrumgenomics/bgzf/actions/workflows/build_and_test.yml/badge.svg" alt="Build Status"></a>
  <img src="https://img.shields.io/crates/l/read_structure.svg" alt="license">
  <a href="https://crates.io/crates/bgzf"><img src="https://img.shields.io/crates/v/bgzf.svg?colorB=319e8c" alt="Version info"></a><br>
</p>

This library provides both high level readers and writers for the BGZF format as well as lower level compressor and decompressor functions.

Bgzf is a multi-gzip format that adds an extra field to the header indicating how large the complete block (with header and footer) is.

## Documentation and Examples

Please see the generated [Rust Docs](https://docs.rs/bgzf).

## How to use in your project

Add the following to your `Cargo.toml` dependencies section, updating the version number as needed.

```toml
[dependencies]
bgzf = "*"
```

## How to build and test locally

Assuming you have cloned the repo and are in the top level:

```bash
cargo test
# The following test is more comprehensive and may take up to 10 minutes to run
cargo test -- --ignored
```

## How to publish

This assumes that you have installed `cargo-release` via `cargo install cargo-release` and have set up credentials with `crates.io`.

```bash
cargo release <patch|minor|major>
```
