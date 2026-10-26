# squeezed CI with PureScript Dagger

Linux x86_64 and aarch64 releases, tarballs, Debian packages, x86_64 RPMs, Linux flake builds and checks, GitHub releases and optional Gemfury uploads. ARM builds still run on the ARM Linux runner.

The workflows use Dagger **v1.0.0-beta.21** and the vendored PureScript SDK.
The module lives in `ci/src/`; `Main.purs` exposes typed functions and `CI/`
contains the implementation. Packaging, release decisions, file enumeration, and command arguments are
implemented in PureScript. External tools run directly through `withExec`;
there are no project-owned shell scripts or embedded shell programs in the pipeline.
Images are pinned in the root `dagger.lock`.

## Run a function

Install Dagger v1.0.0-beta.21 using the [official installation guide](https://docs.dagger.io/install),
then enter the workspace. GitHub Actions uses the commit-pinned official Dagger
action in install-only mode; this repository does not maintain a CLI installer.

```sh
cd .dagger
dagger api call -m ci linux-release --source .. \
  --target x86_64-unknown-linux-gnu --version v0.1.0 --output ../dist
```

Linux Nix builds use the runner's architecture. Cachix credentials are optional;
an empty token performs a build without uploading. Squeezed runs both `nix build` and `nix flake check`; it does not upload to Cachix.

```sh
export CACHIX_AUTH_TOKEN="${CACHIX_AUTH_TOKEN:-}"
dagger api call -m ci nix-build --source .. \
  --cachix-token env://CACHIX_AUTH_TOKEN --run-id "manual-$(date +%s)"
```

GitHub Actions keeps event triggers, platform matrices, native jobs and artifact
transfer. Linux builds and package assembly execute inside Dagger. Manual release
builds use the requested tag's source separately from the pipeline revision.
Publishing functions accept Dagger `Secret` arguments and a unique `run-id` so
workflow retries execute uploads again. `publish-release` creates/updates a
public non-prerelease with generated notes on creation; `upload-release` only
attaches assets. Gemfury uploads run only when both secrets are configured.

## Cache volumes in GitHub Actions

`.github/actions/dagger/action.yml` restores existing named-volume snapshots
and runs the build. **Snapshotting and saving updated volumes are disabled**:
`Snapshot Dagger cache volumes` has `if: ${{ false }}`, and the dependent
`actions/cache/save` step only runs after a successful snapshot.

Existing GitHub cache entries can still be restored until they expire or are
evicted. Dagger's own caching remains enabled within the engine. The PureScript
`save-caches` and `restore-caches` functions remain available for manual use.

Cargo registry, Git and target directories are cached; web builds also cache Bun
packages. Nix builds cache `/nix`, seeded from the pinned Nix image on a cold run.
These caches do not contain registry publishing credentials. GitHub cache quotas
and eviction still apply. SDK bootstrap/compiler caches remain in the Dagger
engine; manual snapshots include only the explicitly listed application volumes.

## Develop the pipeline

Dagger installs Node 24, PureScript 0.15.16 and Spago 1.0.5 **inside the SDK
runtime container**, under `/usr/local`; Spago's dependency cache is under
`/root/.cache/spago-nodejs`. Module compilation happens under `/src`. You only
need the Dagger CLI and a supported engine/container runtime on the host.

For a fast type check without starting Dagger, install PureScript and Spago on
your development machine, then run `spago build --strict` in `.dagger/ci`.
The committed bindings and Spago lockfile make this independent of codegen.
After compiling `.dagger/ci`, run `node --test .dagger/tests/*.test.mjs` from the repository root for
SDK HTTP deadline/error and isolated release-upload regression checks. To intentionally regenerate
bindings, run `python3 .dagger/generate.py`; this starts Dagger and stages only
the pipeline workspace, not the application tree.

See [SDK provenance and local patches](sdk/purescript/VENDORED.md) before
updating the vendored SDK. Retain its LICENSE, NOTICE and schema license.

The migration was checked with strict PureScript compilation, workflow lint,
PureScript pipeline tests and SDK transport tests. Full Dagger builds and publishing
were deliberately left to GitHub Actions; they were not run locally.

Native macOS and BSD builds, and FlakeHub OIDC publishing, remain outside Dagger.

The x86_64 Linux flake coverage check uses `cargo-llvm-cov` with the pinned
Fenix Rust/LLVM toolchain. LLVM instrumentation runs inside Dagger without
Tarpaulin's ptrace/ASLR operations, which the container rejects with `EPERM`.
Coverage remains mandatory in `nix flake check`; the same derivation is exposed
as `packages.squeezed-llvm-coverage`. Its dependency artifacts are built with
the coverage toolchain rather than reused from the ordinary Rust build.
