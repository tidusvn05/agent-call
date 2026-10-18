# Releasing agent-call

Adapted from [okbot's release process](https://github.com/tidusvn05/okbot/blob/main/docs/releasing.md),
[helper](https://github.com/tidusvn05/okbot/blob/main/scripts/release.py), and
[workflow](https://github.com/tidusvn05/okbot/blob/main/.github/workflows/release.yml).

One tag `vX.Y.Z` pointing to a commit on `main` triggers the release. Jobs run in order:
`check` → `build` (if binaries exist) → `release` → `crates`. An optional `image` job also
runs after the GitHub release. No manual publishing workflow or stored crates.io token is needed.

| Output | Purpose | When |
|---|---|---|
| GitHub release with source `.tar.gz`, `.zip`, `SHA256SUMS`, git-cliff notes | Auditable source snapshot and checksums, with a human-readable change history | Every valid release tag |
| Four binary archives: x86_64/aarch64 Linux and macOS | Install without a Rust toolchain | Only if workspace metadata contains a binary target |
| All publishable workspace crates on crates.io | Rust dependencies and, if applicable, `cargo install` | Stable tags only |
| `ghcr.io/tidusvn05/agent-call:X.Y.Z` and `:latest` | Container deployments | Stable tags only, and only if a root `Dockerfile` exists |

**Today this repository is a library:** there is no binary, installer, or Dockerfile. Its example
program is not a binary target. Releases therefore ship source archives, checksums, notes, and the
`agent-call` crate. `cargo install agent-call` does not apply. Do not add a CLI crate or choose its
permanent crates.io name without the maintainer's explicit decision.

A tag `vX.Y.Z-rc.N` requires exactly `X.Y.Z-rc.N` in Cargo.toml and CHANGELOG.md. It produces a
GitHub **pre-release only** (including archives/checksums); it publishes neither crates nor images.

## Versioning and tools

- One version in `[workspace.package]`, inherited by member packages. `main` normally carries the
  next minor version with `-dev` (currently `0.6.0-dev`); release preparation temporarily removes it.
- Use [Conventional Commits](https://www.conventionalcommits.org/), such as `feat:`, `fix:`, and
  `chore(release):`. git-cliff writes CHANGELOG.md and release notes using `cliff.toml`. Historical
  commits without a conventional prefix remain in the changelog.
- Use current stable Rust/Cargo (including workspace publishing support), Python 3.11+, git-cliff,
  and cargo-audit. Install the latter tools with `cargo install git-cliff cargo-audit --locked`.
- `scripts/release.py version X.Y.Z` updates member versions, internal dependencies (including
  aliased, target-specific, build and dev dependencies), and Cargo.lock. It also accepts `-dev`
  and `-rc.N`. Review the resulting diff and update README dependency examples for stable releases.
- `scripts/release.py order` includes dev dependencies in topological order. Cycles are errors;
  restructure a cycle before publishing. `publish = false` and crates restricted to other registries
  are excluded. `pending` reports missing versions and identifies new crates.
- `scripts/release.py publish --dry-run` runs `cargo publish --workspace --dry-run --locked --allow-dirty`
  so it can verify version and changelog edits before committing. Actual publication requires a clean tree.
  Cargo verifies unpublished workspace dependencies together. Actual publication follows the
  explicit dependency order, skips versions already present, and waits on HTTP 429.

## Publishing without stored tokens

Every crate must trust `tidusvn05/agent-call`, workflow **`release.yml`**, environment **`crates-io`**.
The stable release's `crates` job runs in that environment with `permissions: id-token: write`
and `rust-lang/crates-io-auth-action@v1`. Only its temporary output token is passed to Cargo.
No crates.io token belongs in GitHub secrets. `trustpub --only` disables ordinary token publishing
for the crate after bootstrap; the helper reads its setup token from `cargo login`.

The maintainer creates the GitHub environment **crates-io** in Settings → Environments and may
add themself as a required reviewer. The maintainer also creates temporary crates.io tokens,
enters them locally, and explicitly authorizes every tag push. Never put a token in chat.

### First release / adopting this process

Trusted Publishing cannot create crates. Check every proposed name on crates.io before any first
publish, including hyphen/underscore variants; names are allocated permanently. Confirm ownership
for names already present. `scripts/release.py pending` distinguishes absent names from existing
crates but is not a reservation or an ownership check.

At setup, the sparse index already contains `agent-call` versions `0.4.1` and `0.4.2`; keep that existing
crate name and confirm you have owner access. There is no new crate to bootstrap today; an owner
can configure `trustpub` for `agent-call` directly without publishing a duplicate version.

For genuinely new crates:

1. Agree the binary crate name with the maintainer if adding a CLI, and check the names are free
   using crates.io search and the crate pages (including equivalent hyphen/underscore names).
   Resolve any collision before merging the release PR.
2. Prepare and merge the release PR below. On the exact merged stable-version commit, create a
   short-lived crates.io token with `publish-new`, `publish-update`, and `trusted-publishing` scopes,
   limited to the exact crate names and expiring in a few days. Obtain explicit publishing approval.
3. The maintainer runs `cargo login` interactively (use `! cargo login` in the client to keep the
   token out of chat). Publish **by hand**, in `scripts/release.py order` order:
   ```sh
   cargo publish --locked --registry crates-io -p <new-crate>
   # Wait until that dependency is indexed before publishing its dependents.
   scripts/release.py trustpub tidusvn05/agent-call --crates <new-crate> --only
   ```
   Repeat for each new crate. Manual Cargo publishing can hit HTTP 429: wait until the server's
   retry time before trying again. The automated helper deliberately stops on new crates.
4. For the existing `agent-call` crate, use a temporary token with `trusted-publishing` scope:
   ```sh
   scripts/release.py trustpub tidusvn05/agent-call --crates agent-call --only
   cargo logout
   ```
   For new crates, also run `cargo logout` after setup. **Delete/revoke the token on crates.io**;
   logout alone only removes the local credential. Create the GitHub environment before tagging.
5. Ask the maintainer before pushing the tag. The workflow skips any versions already published
   manually and publishes the remaining existing crates with Trusted Publishing.

`trustpub OWNER/REPO [--crates a,b] [--only]` is reusable for another repository. Defaults are
`--workflow release.yml --environment crates-io`; running it again preserves matching configurations.
Always pass `--only` when adopting this policy.

### Adding a new crate later

Check availability and confirm its permanent name with the maintainer first if it is a binary.
Use the shared workspace version and versioned internal dependencies. Run `pending` before tagging.
If it reports a new crate, stop and follow the manual bootstrap above with `--crates <new-crate>`.
If the workflow encounters one anyway, the `crates` job stops with its name **before** authenticating
or publishing any crates. Complete bootstrap, revoke the token, then rerun the failed job.

## Each release

1. Prepare on `release/vX.Y.Z`:
   ```sh
   git switch -c release/vX.Y.Z
   scripts/release.py version X.Y.Z
   git cliff --tag vX.Y.Z -o CHANGELOG.md
   # Update the README's stable dependency version as needed.
   scripts/verify.sh
   scripts/release.py publish --dry-run
   scripts/release.py pending
   ```
   Fix dependency advisories and yanked dependencies before tagging; `verify.sh` runs cargo-audit
   with warnings denied. If `pending` names new crates, stop and walk through bootstrap with the
   maintainer. Commit as `chore(release): vX.Y.Z`. Scan the full diff for secrets **before every
   push**, then push the branch and open a PR with that title. Wait for CI. Merge only with the
   maintainer's explicit go-ahead.
2. From the merged commit, ask for explicit approval **before pushing the tag**:
   ```sh
   git switch main
   git pull --ff-only
   git tag -a vX.Y.Z -m 'agent-call vX.Y.Z'
   # Review and scan the outgoing diff for secrets before this push too.
   git push origin vX.Y.Z
   ```
   The workflow checks exact version/changelog matching and that the tagged commit is on `main`,
   runs verification and packaging, then creates outputs in the order above.
3. Watch `Release` to completion. Report failures with the failing job's log:
   ```sh
   gh run list --workflow release.yml
   gh run view <run-id> --log-failed
   ```
4. Verify **each applicable output**:
   - Download the GitHub release assets with `gh release download vX.Y.Z`, then run
     `sha256sum -c SHA256SUMS` and inspect the extracted source version and release notes.
   - Check every crate's version on crates.io; create a fresh temporary Cargo project and
     `cargo add agent-call@=X.Y.Z && cargo check`.
   - If a binary is added: extract/run each native binary, test its installer if one exists,
     and run `cargo install <confirmed-binary-crate> --version X.Y.Z --locked` in a clean location.
   - If a Dockerfile is added: pull/run `ghcr.io/tidusvn05/agent-call:X.Y.Z`, verify the program's
     version, and confirm `latest` points at that stable release. RCs must have no crates/image.
5. Open a separate PR to resume development with `scripts/release.py version X.Y+1.0-dev`
   (for example `0.5.0` → `0.6.0-dev`). Scan before pushing and wait for CI and merge approval.
   Do not change the released tag.

## When something fails

- **Before the tag:** fix the release PR, regenerate notes if needed, and rerun verification. Do
  not tag with advisories or yanked dependencies outstanding.
- **Check/build:** inspect and report the failed job log. Rerun transient failures with
  `gh run rerun <run-id> --failed`. A code fix requires a new commit and a new version/tag, since
  reruns check out the original tagged source. Never move or overwrite an existing release tag.
- **GitHub release:** rerun failed jobs. If a release already exists, the workflow leaves its assets
  and notes untouched and continues to downstream publication. Never replace published assets or
  checksums. If they are wrong or incomplete, ship a new patch version.
- **crates.io:** rerun the failed job; already published versions are skipped, including yanked
  versions (a version number cannot be reused). HTTP 429 waits respect Retry-After when available.
  If the job timeout expires while waiting, rerun. Authentication failures require checking the
  repository, `release.yml`, `crates-io` environment, and each crate's Trusted Publishing settings.
  New crates require the manual bootstrap. A partially published release resumes in dependency order.
- **Image:** inspect its job log and rerun for transient failures. A bad published image needs a new
  release; do not replace a versioned image with different content.
- **Broken crate:** obtain explicit authorization, then yank every affected crate version with
  `cargo yank --version X.Y.Z <crate>`, and ship a fixed patch release. Yanking prevents new
  dependency resolution; it does not delete the version or invalidate existing lockfiles.
