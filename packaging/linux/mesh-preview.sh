#!/usr/bin/env bash
# The mesh preview build of this fork: DeckCraft with co-editing over the Macula mesh, under its
# own identity (io.macula.deckcraft-mesh, package and binary deckcraft-mesh) so it installs beside
# an upstream DeckCraft and never passes as one. Arguments go to package.sh (e.g. --formats).
#
# Source paths are remapped, so the binaries name no build machine's directories.
set -euo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
export DECKCRAFT_APP_ID=io.macula.deckcraft-mesh
export DECKCRAFT_APP_NAME="DeckCraft (mesh preview)"
export DECKCRAFT_PKG=deckcraft-mesh
export DECKCRAFT_MAINTAINER="${DECKCRAFT_MAINTAINER:-Macula <raf.lefever@erlef.org>}"
export DECKCRAFT_VENDOR=Macula
export DECKCRAFT_HOMEPAGE=https://github.com/rgfaber/deckcraft
ROOT="$(cd "$HERE/../.." && pwd)"
CARGO_HOME="${CARGO_HOME:-$HOME/.cargo}"
export RUSTFLAGS="${RUSTFLAGS:-} --remap-path-prefix=$CARGO_HOME=cargo --remap-path-prefix=$ROOT=deckcraft --remap-path-prefix=$HOME=home"
exec "$HERE/package.sh" "$@"
