module CI.Build (linuxRelease) where

import Effect (Effect)
import Prelude
import CI.Package as Package
import Native (Client)
import Native.Container as Container
import Native.Query as Query
import Native.Types (Container, Directory)
import Data.Maybe (Maybe(..))
import Effect.Exception (error, throwException)

type BuildArgs = { source :: Directory, target :: String, version :: String }

withSource :: Directory -> Container -> Container
withSource source = Container.withDirectoryWith "/src" source
  (Container.defaultWithDirectoryOpts
    { exclude = Just [ ".ci-tooling", ".fluentci", ".git", ".dagger", ".source", ".fluentci", "target", "**/node_modules", "**/.env", "**/.env.*", "result", "result-*", "dist/*.tar.gz*", "dist/*.deb*", "dist/*.rpm*", "dist/staging", "dist/rpmbuild", "*.db*", "covers", "smolsonic.toml" ] })

withCargoCaches :: Client -> String -> Container -> Container
withCargoCaches client target container = container
  # Container.withEnvVariable "CARGO_HOME" "/cargo"
  # Container.withMountedCache "/cargo/registry" (Query.cacheVolume "squeezed-cargo-registry" client)
  # Container.withMountedCache "/cargo/git" (Query.cacheVolume "squeezed-cargo-git" client)
  # Container.withMountedCache "/src/target" (Query.cacheVolume ("squeezed-target-" <> target) client)


linuxRelease :: Client -> BuildArgs -> Effect Directory
linuxRelease client args = do
  label <- case args.target of
    "x86_64-unknown-linux-gnu" -> pure "linux-amd64"
    "aarch64-unknown-linux-gnu" -> pure "linux-arm64"

    _ -> throwException (error "Unsupported Linux release target")
  let
    environment = Query.container client
      # Container.from "rust:bookworm"
      # Container.withExec [ "rustup", "target", "add", args.target ]
    built = environment
      # withSource args.source

      # Container.withWorkdir "/src"
      # withCargoCaches client args.target
      # Container.withExec [ "cargo", "build", "--release", "--locked", "--target", args.target, "--bin", "squeezed" ]
      -- Cache mounts cannot be exported directly: materialize the executable.
      # Container.withExec [ "mkdir", "-p", "/binary" ]
      # Container.withExec [ "cp", "target/" <> args.target <> "/release/squeezed", "/binary/squeezed" ]
  Package.packageRelease client
    { source: args.source
    , binary: Container.file "/binary/squeezed" built
    , target: args.target
    , label
    , version: args.version
    }
