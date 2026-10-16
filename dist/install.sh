#!/bin/sh
# Installs a prebuilt cmd-man binary from GitHub Releases.
#
#   curl -fsSL https://raw.githubusercontent.com/brennacodes/cmd-man/main/dist/install.sh | sh
#
# CMD_MAN_VERSION      release tag to install (default: the latest stable release)
# CMD_MAN_INSTALL_DIR  directory the binary is installed into (default: ~/.local/bin)

set -eu

REPO="brennacodes/cmd-man"

fail() {
    echo "cmd-man install: $*" >&2
    exit 1
}

detect_platform() {
    case "$1" in
        Darwin) os=darwin ;;
        Linux) os=linux ;;
        *) fail "unsupported operating system '$1' (cmd-man supports macOS and Linux)" ;;
    esac
    case "$2" in
        x86_64 | amd64) arch=x64 ;;
        arm64 | aarch64) arch=arm64 ;;
        *) fail "unsupported architecture '$2' (cmd-man supports x86_64 and arm64)" ;;
    esac
    echo "$os-$arch"
}

release_url() {
    asset="cmd-man-$1.tar.gz"
    if [ -n "$2" ]; then
        echo "https://github.com/$REPO/releases/download/$2/$asset"
    else
        echo "https://github.com/$REPO/releases/latest/download/$asset"
    fi
}

install_archive() {
    unpack_dir=$(mktemp -d)
    tar -xzf "$1" -C "$unpack_dir" || fail "could not extract $1"
    [ -f "$unpack_dir/cmd-man" ] || fail "archive does not contain a cmd-man binary"
    mkdir -p "$2"
    mv "$unpack_dir/cmd-man" "$2/cmd-man"
    chmod 0755 "$2/cmd-man"
    rm -rf "$unpack_dir"
}

main() {
    command -v curl >/dev/null 2>&1 || fail "curl is required"
    command -v tar >/dev/null 2>&1 || fail "tar is required"

    platform=$(detect_platform "$(uname -s)" "$(uname -m)")
    url=$(release_url "$platform" "${CMD_MAN_VERSION:-}")
    install_dir="${CMD_MAN_INSTALL_DIR:-$HOME/.local/bin}"

    download_dir=$(mktemp -d)
    trap 'rm -rf "$download_dir"' EXIT
    echo "Downloading $url"
    curl -fsSL "$url" -o "$download_dir/cmd-man.tar.gz" || fail "download failed: $url"

    install_archive "$download_dir/cmd-man.tar.gz" "$install_dir"
    "$install_dir/cmd-man" --version || fail "the installed binary failed to run"
    echo "Installed cmd-man to $install_dir/cmd-man"

    case ":$PATH:" in
        *":$install_dir:"*) ;;
        *) echo "Add $install_dir to your PATH to run cmd-man from any shell." ;;
    esac
    echo "Run 'cmd-man init' to wire cmd-man into your shells."
}

if [ -z "${CMD_MAN_INSTALL_SOURCED:-}" ]; then
    main
fi
