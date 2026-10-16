#!/bin/sh
# Usage: check-release-tag.sh TAG [CARGO_TOML]
# Exits non-zero unless TAG is a release tag (X.Y.Z or X.Y.Z-rc.N) equal to the
# package version in CARGO_TOML (default: Cargo.toml).

set -eu

tag="$1"
manifest="${2:-Cargo.toml}"

if ! echo "$tag" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+(-rc\.[0-9]+)?$'; then
    echo "'$tag' is not a release tag (expected X.Y.Z or X.Y.Z-rc.N)" >&2
    exit 1
fi

version=$(awk '
    /^\[/ { in_package = ($0 == "[package]") }
    in_package && /^version[ \t]*=/ {
        sub(/^version[ \t]*=[ \t]*"/, "")
        sub(/".*/, "")
        print
        exit
    }
' "$manifest")

if [ "$version" != "$tag" ]; then
    echo "tag '$tag' does not match the package version '$version' in $manifest" >&2
    exit 1
fi
