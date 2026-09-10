# Staging builds and releases

The CLI ships as one flat tarball per platform, like the BaseRT engine bundles:

| Bundle | Built on | Runs on |
| --- | --- | --- |
| `computearena-macos-arm64-<version>.tar.gz` | hosted macOS arm64 runner | Apple Silicon Macs |
| `computearena-linux-x86_64-<version>.tar.gz` | `cross` on the hosted Ubuntu runner | x86_64 Linux, glibc 2.31 or newer |
| `computearena-linux-arm64-<version>.tar.gz` | `cross` on the hosted Ubuntu runner | arm64 Linux (for example a DGX Spark), glibc 2.31 or newer |

Each archive holds `computearena`, `LICENSE`, and `README.md`, and has a
`.sha256` sidecar. `tar -xzf <bundle> -C <dir> computearena` is a complete
install. Windows and Intel Macs are not built; the runtime adapters and the
integration tests are Unix-only today.

Every bundle is built by `.github/workflows/build.yml` and smoke-tested before
upload (`.github/scripts/smoke_test.sh`): checksum, layout, `--version`,
`list`, an offline benchmark run against a stub llama-bench, `verify`,
`inspect`, and rejection of a tampered report. The Linux bundles run that test
inside a stock `ubuntu:22.04` container, the arm64 one under QEMU, which
proves the glibc floor rather than assuming it. Unit and integration tests
remain CI's job.

All releases, staging or final, live on this repository. There is no public
mirror repository and no mirror token. Public distribution through
downloads.basecompute.co is a separate step that can pick these assets up by
their stable names.

## Branch and version model

- Features merge into the current release-candidate branch, `rc-X.Y.Z`.
- `main` only advances through a pull request from an `rc-*` branch.
- The version lives in one place: `[workspace.package].version` in the root
  `Cargo.toml`; the crate inherits it. The rc branch name and the release tag
  must carry the same value, and `.github/scripts/check_version.sh` refuses to
  stage or release when they do not.

To start the next release:

```sh
git checkout -b rc-0.2.0 main
sed -i '' 's/^version = "0.1.0"$/version = "0.2.0"/' Cargo.toml   # macOS sed; drop '' on Linux
cargo check                                                        # refreshes Cargo.lock
git commit -am "chore: start 0.2.0"
git push -u origin rc-0.2.0
gh pr create --base main --head rc-0.2.0 --title "RC 0.2.0"
```

Commit `Cargo.lock` with the bump: every build runs `--locked`.

## Staging builds

`.github/workflows/rc-staging.yml` runs on every push to an `rc-*` branch,
which in practice means every feature merge. It

1. checks that the branch name's version equals the Cargo version,
2. builds and smoke-tests all three bundles with a `-staging` name suffix,
3. deletes and recreates the pre-release tagged `staging-<version>` at the
   pushed commit, with the bundles attached, and
4. updates a single sticky comment on the open `rc-* -> main` pull request
   with the link and an install snippet.

The tag is never a `v*` tag, so the release workflow cannot fire from it. A
newer push cancels a staging build still in progress for the same branch.

Install a staging build (the pull-request comment carries the same snippet):

```sh
case "$(uname -s)-$(uname -m)" in
  Darwin-arm64)  P=macos-arm64 ;;
  Linux-x86_64)  P=linux-x86_64 ;;
  Linux-aarch64) P=linux-arm64 ;;
esac
D="${COMPUTEARENA_INSTALL_DIR:-$HOME/.basert}"; mkdir -p "$D"
gh release download staging-0.1.0 --repo basecompute/computearena-cli \
  --pattern "computearena-${P:?}-*.tar.gz" -O - | tar -xzf - -C "$D" computearena
"$D/computearena" --version
```

This repository is internal, so the asset URLs need authentication; `gh`
supplies it. Point the binary at the staging environment with `--api-url`
or `COMPUTEARENA_API_URL`.

When the rc pull request closes, merged or not,
`.github/workflows/rc-cleanup.yml` deletes the staging pre-release and tag.

To re-stage a branch by hand, run the workflow from the Actions tab with the
rc branch selected under "Use workflow from". GitHub lists the workflow there
once `main` carries the file, so this is available from the second rc onward.

## Cutting a release

1. Merge the `rc-X.Y.Z -> main` pull request. Staging cleanup runs.
2. Optionally write `docs/release-notes/X.Y.Z.md`. If it exists, it becomes the
   release body; otherwise GitHub generates notes from the merged pull
   requests.
3. Tag the merge commit on `main` and push the tag:

   ```sh
   git fetch origin main
   git tag -a vX.Y.Z -m "ComputeArena CLI X.Y.Z" origin/main
   git push origin vX.Y.Z
   ```

`.github/workflows/release.yml` then gates the tag (Cargo version matches, the
version is newer than the previous `v*` tag, the commit is on `main`), builds
the same three bundles without a suffix, creates the release as a draft,
attaches every bundle, and only then publishes it and marks it latest. If any
platform fails, nothing is published: fix the problem, delete the tag, and
tag again once the fix is on `main` (which means through another rc).

The workflow refuses to touch a tag that already has a published release.
Deleting a published release is a manual decision.

## Troubleshooting

- **"Cargo workspace version is A but this ref advertises B"**: bump
  `Cargo.toml`, run `cargo check`, commit both files.
- **"Cargo.lock needs to be updated" during the gate or build**: the lockfile
  was not committed with a dependency or version change; run `cargo check`
  locally and commit `Cargo.lock`.
- **The arm64 Linux smoke test fails but the build passed**: the test runs
  under QEMU; check the run log for the exact step. The binary itself is
  identical in construction to the x86_64 one.
- **A staging release exists for an rc that already merged**: the cleanup
  workflow runs on the pull-request close event only; delete it with
  `gh release delete staging-X.Y.Z --cleanup-tag`.
