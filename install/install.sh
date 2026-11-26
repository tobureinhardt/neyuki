#!/bin/sh
# Installs Neyuki from a GitHub release on Linux and macOS:
#
#   curl -fsSL https://neyuki.cc/install.sh | sh
#
# The executable goes to ~/.neyuki/bin (or $NEYUKI_HOME/bin), which is added
# to PATH in your shell's startup file. Arguments, after `sh -s --` when
# piped:
#
#   --version <tag>   install that release (v0.12.0) instead of the latest
#   --no-modify-path  leave the shell startup files alone
#
# NEYUKI_VERSION and NEYUKI_NO_MODIFY_PATH=1 do the same.

set -eu

REPO="neyuki-lang/neyuki"

say() { printf '%s\n' "$*"; }
err() {
    printf 'neyuki install: %s\n' "$*" >&2
    exit 1
}

version="${NEYUKI_VERSION:-}"
modify_path=1
[ "${NEYUKI_NO_MODIFY_PATH:-}" = 1 ] && modify_path=0
while [ $# -gt 0 ]; do
    case "$1" in
        --version)
            [ $# -ge 2 ] || err "--version needs a release tag, such as v0.12.0"
            version="$2"
            shift 2
            ;;
        --version=*)
            version="${1#--version=}"
            shift
            ;;
        --no-modify-path)
            modify_path=0
            shift
            ;;
        -h | --help)
            say "usage: install.sh [--version <tag>] [--no-modify-path]"
            exit 0
            ;;
        *) err "unknown argument: $1" ;;
    esac
done

# download <url> <file>
if command -v curl >/dev/null 2>&1; then
    download() { curl --proto '=https' --tlsv1.2 -fsSL "$1" -o "$2"; }
elif command -v wget >/dev/null 2>&1; then
    download() { wget --https-only -q "$1" -O "$2"; }
else
    err "needs curl or wget"
fi

os=$(uname -s)
arch=$(uname -m)
case "$arch" in
    x86_64 | amd64) arch=x86_64 ;;
    aarch64 | arm64) arch=aarch64 ;;
    *) err "no prebuilt Neyuki for $arch; build it with \`cargo install neyuki\`" ;;
esac
case "$os" in
    Linux)
        if ldd --version 2>&1 | grep -qi musl; then
            err "no prebuilt Neyuki for musl Linux; build it with \`cargo install neyuki\`"
        fi
        target="$arch-unknown-linux-gnu"
        ;;
    Darwin)
        # A shell under Rosetta reports x86_64 on Apple Silicon.
        if [ "$arch" = x86_64 ] && [ "$(sysctl -n hw.optional.arm64 2>/dev/null)" = 1 ]; then
            arch=aarch64
        fi
        target="$arch-apple-darwin"
        ;;
    MINGW* | MSYS* | CYGWIN*)
        err "on Windows, run in PowerShell: irm https://neyuki.cc/install.ps1 | iex"
        ;;
    *) err "no prebuilt Neyuki for $os; build it with \`cargo install neyuki\`" ;;
esac

tmp=$(mktemp -d 2>/dev/null || mktemp -d -t neyuki)
trap 'rm -rf "$tmp"' EXIT INT TERM

if [ -z "$version" ]; then
    # The latest release's tag, from the API's `"tag_name": "v1.2.3"`.
    download "https://api.github.com/repos/$REPO/releases/latest" "$tmp/latest.json" ||
        err "cannot look up the latest release (set NEYUKI_VERSION to pick one)"
    version=$(sed -n 's/.*"tag_name"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' "$tmp/latest.json" | head -n 1)
    [ -n "$version" ] || err "cannot read the latest release's tag"
fi
case "$version" in v*) ;; *) version="v$version" ;; esac

name="neyuki-$version-$target"
base="https://github.com/$REPO/releases/download/$version"
say "Downloading Neyuki $version for $target"
download "$base/$name.tar.gz" "$tmp/$name.tar.gz" ||
    err "cannot download $base/$name.tar.gz"

# Releases publish SHA256SUMS since this script exists; older ones are
# installed without the check.
if download "$base/SHA256SUMS" "$tmp/SHA256SUMS" 2>/dev/null; then
    expected=$(grep " \*\{0,1\}$name.tar.gz\$" "$tmp/SHA256SUMS" | cut -d ' ' -f 1)
    [ -n "$expected" ] || err "SHA256SUMS does not list $name.tar.gz"
    if command -v sha256sum >/dev/null 2>&1; then
        actual=$(sha256sum "$tmp/$name.tar.gz" | cut -d ' ' -f 1)
    elif command -v shasum >/dev/null 2>&1; then
        actual=$(shasum -a 256 "$tmp/$name.tar.gz" | cut -d ' ' -f 1)
    else
        err "needs sha256sum or shasum to check the download"
    fi
    [ "$actual" = "$expected" ] || err "checksum mismatch for $name.tar.gz"
fi

tar -xzf "$tmp/$name.tar.gz" -C "$tmp"
[ -f "$tmp/$name/neyuki" ] || err "$name.tar.gz has no neyuki executable"

home="${NEYUKI_HOME:-$HOME/.neyuki}"
bin="$home/bin"
mkdir -p "$bin"
# Through a temporary name, so a running neyuki is never half overwritten.
cp "$tmp/$name/neyuki" "$bin/neyuki.new"
chmod 755 "$bin/neyuki.new"
mv -f "$bin/neyuki.new" "$bin/neyuki"
say "Installed $("$bin/neyuki" --version 2>/dev/null || echo "neyuki $version") to $bin/neyuki"

# add_line <file> <line>: appends the line unless the file already has it.
add_line() {
    if [ -f "$1" ] && grep -qF "$2" "$1"; then
        return 0
    fi
    mkdir -p "$(dirname "$1")"
    printf '\n# Neyuki\n%s\n' "$2" >>"$1"
    say "Added $bin to PATH in $1"
}

case ":$PATH:" in
    *":$bin:"*) on_path=1 ;;
    *) on_path=0 ;;
esac
hint="export PATH=\"$bin:\$PATH\""
if [ "$modify_path" = 1 ]; then
    export_line="export PATH=\"$bin:\$PATH\""
    case "$(basename "${SHELL:-sh}")" in
        zsh) add_line "${ZDOTDIR:-$HOME}/.zshrc" "$export_line" ;;
        bash)
            add_line "$HOME/.bashrc" "$export_line"
            # Login shells (macOS Terminal) read .bash_profile, not .bashrc.
            if [ "$os" = Darwin ] || [ -f "$HOME/.bash_profile" ]; then
                add_line "$HOME/.bash_profile" "$export_line"
            fi
            ;;
        fish)
            hint="fish_add_path \"$bin\""
            add_line "${XDG_CONFIG_HOME:-$HOME/.config}/fish/conf.d/neyuki.fish" "$hint"
            ;;
        *) add_line "$HOME/.profile" "$export_line" ;;
    esac
fi

# The startup files put $bin first in new shells, so another neyuki only
# matters when they were left alone or $bin was on PATH already.
other=$(command -v neyuki 2>/dev/null || true)
if [ -n "$other" ] && [ "$other" != "$bin/neyuki" ] && { [ "$modify_path" = 0 ] || [ "$on_path" = 1 ]; }; then
    say "Note: $other comes first on your PATH; remove it to use this install."
fi
if [ "$on_path" = 0 ]; then
    if [ "$modify_path" = 1 ]; then
        say "Open a new terminal, or run: $hint"
    else
        say "Add $bin to your PATH to run neyuki."
    fi
fi
say "Run \`neyuki help\` to get started."
