# Security Policy

## Supported versions

RustHouse is pre-1.0 software. Security fixes are applied to the latest commit
on the default branch; older commits and snapshot format versions do not
receive separate maintenance releases.

## Reporting

Report vulnerabilities through GitHub private vulnerability reporting for this
repository. Do not open a public issue for an unpatched vulnerability. Include
the affected revision, reproduction steps, impact, and any known mitigations.

## Dependency policy

- Keep runtime dependencies minimal; each addition requires a documented need.
- Commit `Cargo.lock` and use `--locked` in CI so reviewed versions are tested.
- Pin CI actions to full commit hashes and let Dependabot propose updates.
- Dependabot checks Cargo and GitHub Actions dependencies weekly.
- Before a release, review `cargo tree --locked` and run `cargo audit`; resolve
  applicable advisories or document why they do not affect the shipped code.
- The minimum supported Rust version is declared in `Cargo.toml` and the exact
  CI toolchain is pinned in `rust-toolchain.toml`.
