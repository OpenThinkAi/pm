#!/usr/bin/env bash
# Install cargo-dist (`dist`) at a pinned version, verifying the release
# tarball against a SHA-256 committed here (AGT-1372). Used by every
# release.yml job that runs `dist`, so no job executes an unverified
# downloaded installer or reuses a binary built by another job.
#
# To bump: change VERSION, refresh the four hashes from the release's
# `sha256.sum` (and cross-check by hashing the downloads yourself), and keep
# `cargo-dist-version` in Cargo.toml in step.
set -euo pipefail

VERSION="0.31.0"

case "$(uname -s)-$(uname -m)" in
  Darwin-arm64)  target="aarch64-apple-darwin";
                 sha="decb01c64c12501931c3cac3111b368a7f48adf8d9e65455c08e5757b9a1fd6f" ;;
  Darwin-x86_64) target="x86_64-apple-darwin";
                 sha="fd4d8f9f07802359cbcdc52bac3abd7d5201c4b73a7cbcdd6faca2232a389f0c" ;;
  Linux-x86_64)  target="x86_64-unknown-linux-gnu";
                 sha="cd355dab0b4c02fb59038fef87655550021d07f45f1d82f947a34ef98560abb8" ;;
  *) echo "install-dist.sh: unsupported platform $(uname -s)-$(uname -m)" >&2; exit 1 ;;
esac

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
asset="cargo-dist-${target}.tar.xz"
curl --proto '=https' --tlsv1.2 -fsSL -o "$tmp/$asset" \
  "https://github.com/axodotdev/cargo-dist/releases/download/v${VERSION}/${asset}"

if command -v sha256sum >/dev/null 2>&1; then
  actual="$(sha256sum "$tmp/$asset" | cut -d' ' -f1)"
else
  actual="$(shasum -a 256 "$tmp/$asset" | cut -d' ' -f1)"
fi
if [ "$actual" != "$sha" ]; then
  echo "install-dist.sh: SHA-256 mismatch for $asset" >&2
  echo "  expected $sha" >&2
  echo "  actual   $actual" >&2
  exit 1
fi

mkdir -p "$HOME/.cargo/bin"
tar -xJf "$tmp/$asset" -C "$tmp"
install -m 0755 "$tmp/cargo-dist-${target}/dist" "$HOME/.cargo/bin/dist"
echo "$HOME/.cargo/bin" >> "${GITHUB_PATH:-/dev/null}"
"$HOME/.cargo/bin/dist" --version
