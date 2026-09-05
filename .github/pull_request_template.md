<!-- git flow: feature/* and bugfix/* target develop; release/* and hotfix/* target main -->

## Summary

<!-- What does this change and why? -->

## Type

- [ ] `feature/*` → `develop`
- [ ] `bugfix/*` → `develop`
- [ ] `release/*` / `hotfix/*` → `main`

## Checklist

- [ ] `cargo fmt --all --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test --all` pass locally
- [ ] User-visible changes noted under `## [Unreleased]` in CHANGELOG.md