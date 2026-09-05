# Contributing

This project uses **git flow**: `main` holds production, `develop` is the
integration branch, and all work happens on short-lived branches off `develop`.

## Branches

| Branch | Purpose |
|---|---|
| `main` | Production. Only ever receives merges from `release/*` and `hotfix/*`. Every merge to `main` is tagged `vX.Y.Z` and published by the release workflow. |
| `develop` | Integration. All features and regular fixes land here first. Default branch on GitHub — PRs and Dependabot target it. |
| `feature/<name>` | New functionality or changes. Branch off `develop`, merge back into `develop`. |
| `bugfix/<name>` | Non-urgent fixes. Branch off `develop`, merge back into `develop`. |
| `release/X.Y.Z` | Release preparation (version bump, changelog). Branch off `develop`, merge into `main` **and** `develop`. |
| `hotfix/X.Y.Z` | Urgent production fixes. Branch off `main`, merge into `main` **and** `develop`. |

## Day-to-day workflow

1. Branch off `develop`:

   ```sh
   git checkout develop
   git pull
   git checkout -b feature/my-change
   ```

   (With the `git-flow` extension installed: `git flow feature start my-change`.)

2. Commit there. Keep messages descriptive, one topic per branch.
3. Push the branch and open a **PR to `develop`** — not `main`. CI (fmt,
   clippy, tests) runs on every PR and must be green. Run locally first:

   ```sh
   cargo fmt --all --check
   cargo clippy --all-targets -- -D warnings
   cargo test --all
   ```

4. Merge with a merge commit (no squash — history is structured by branch),
   delete the branch, continue with the next one.

Releases happen from `develop`, so `develop` should stay shippable: update
`CHANGELOG.md` under `## [Unreleased]` as part of your PR when the change is
user-visible.

## Releases

1. Start a release branch off `develop`:

   ```sh
   git checkout -b release/0.3.0 develop
   ```

2. In that branch, move the CHANGELOG's `[Unreleased]` entries into a new
   `## [0.3.0] - YYYY-MM-DD` section, and bump `version` in `Cargo.toml`
   (`cargo check` will also update `Cargo.lock`). The tag **must** match
   `Cargo.toml` — the release workflow fails otherwise.
3. Merge to `main` and back to `develop` — both branches are protected, so
   this goes through PRs (merge commits, no squash):

   ```sh
   git push -u origin release/0.3.0
   gh pr create --base main --head release/0.3.0 --title "Release 0.3.0"
   gh pr merge --merge --delete-branch
   gh pr create --base develop --head main --title "Back-merge 0.3.0 into develop"
   gh pr merge --merge --delete-branch
   ```

   (Or open the two PRs in the browser: `release/0.3.0 → main`, then
   `main → develop`.)

4. Tag on `main` and push the tag:

   ```sh
   git checkout main && git pull && git tag -a v0.3.0 -m "auto-scanner-ocr 0.3.0" && git push origin v0.3.0
   ```

   Pushing the tag triggers the release workflow: cross-compiled binaries,
   deb/rpm packages, checksums and a GitHub release.

## Hotfixes

For a critical bug in production, branch off `main` instead:

```sh
git checkout -b hotfix/0.3.1 main
```

Fix, bump `Cargo.toml` + CHANGELOG to `0.3.1`, then PR `hotfix/0.3.1 → main`
and PR `main → develop`, tag `v0.3.1` on `main` — same as a release, just
branched off `main`.

## Notes

- Dependabot opens PRs against `develop`. Merge them like any other PR.
- `main` and `develop` are long-lived: never delete them, never commit
  directly to either — both are protected by rulesets (PR + CI required), so
  use feature/bugfix/hotfix branches and PRs.
- A `git-flow` extension (`git-flow-avh` on Arch/AUR, `git-flow` on
  Debian/Ubuntu) is optional — branch names above match its defaults and the
  prefix configuration is already stored in `.git/config` if you want it.