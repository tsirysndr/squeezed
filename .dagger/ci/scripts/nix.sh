#!/bin/sh
set -eu
cd /src
nix build .#default -L
nix flake check -L
