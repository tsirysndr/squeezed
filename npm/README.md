# @tsiry/squeezed

npm distribution of [`squeezed`](https://github.com/tsirysndr/squeezed) — serve
a raw PCM audio stream to any Squeezelite/Squeezebox client over the SlimProto
protocol.

```sh
npm install -g @tsiry/squeezed
squeezed --help
```

On install, the package downloads the prebuilt binary for your platform from
the matching [GitHub release](https://github.com/tsirysndr/squeezed/releases)
(`v<package version>`), verifies its sha256 checksum, and exposes it as the
`squeezed` command. Supported platforms: macOS (x64/arm64), Linux (x64/arm64),
FreeBSD (x64). If the install script was skipped (`--ignore-scripts`), the
binary is fetched on first run instead.

See the [project README](https://github.com/tsirysndr/squeezed#readme) for
usage, configuration, and the macOS virtual audio device.

## Publishing (maintainers)

The package version **must match a tagged GitHub release** — `npm/package.json`
version `X.Y.Z` downloads assets from tag `vX.Y.Z`. To release:

1. Tag and push `vX.Y.Z` (the Release workflow uploads the binaries).
2. Set the same version in `npm/package.json`.
3. `cd npm && npm publish --access public`
