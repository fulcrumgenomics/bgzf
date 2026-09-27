# Releasing

This document describes how to release a new version of bgzf.

## Prerequisites

1. Ensure you have push access to the repository
2. Publishing uses crates.io [trusted publishing](https://crates.io/docs/trusted-publishing); no API token secret is needed. The `bgzf` crate on crates.io must list `fulcrumgenomics/bgzf` with workflow `release.yml` as a trusted publisher

## Release Process

1. Update `CHANGELOG.md` with the release date and any final changes

2. Update version in `Cargo.toml`:
   ```toml
   version = "X.Y.Z"
   ```

3. Commit the version bump:
   ```bash
   git add Cargo.toml CHANGELOG.md
   git commit -m "chore: release vX.Y.Z"
   ```

4. Create and push a tag:
   ```bash
   git tag vX.Y.Z
   git push origin main --tags
   ```

5. The GitHub Action will automatically:
   - Publish to crates.io
   - Create a GitHub Release with auto-generated notes

## Version Guidelines

- **Major (X.0.0)**: Breaking API changes
- **Minor (0.X.0)**: New features, deprecations
- **Patch (0.0.X)**: Bug fixes, performance improvements
