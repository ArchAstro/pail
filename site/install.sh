#!/bin/sh
# Installs pail: curl -fsSL https://pail.build/install.sh | sh
#   PAIL_VERSION=v0.1.0   a specific release (default: the latest)
#   PAIL_INSTALL_DIR=DIR  where to put the binary (default: ~/.local/bin)
set -eu
repo=ArchAstro/pail
dir=${PAIL_INSTALL_DIR:-$HOME/.local/bin}
case "$(uname -s)" in
  Darwin) os=darwin ;;
  Linux) os=linux ;;
  *) echo "pail: no build for $(uname -s); build from source: https://github.com/$repo" >&2; exit 1 ;;
esac
case "$(uname -m)" in
  arm64 | aarch64) arch=arm64 ;;
  x86_64 | amd64) arch=amd64 ;;
  *) echo "pail: no build for $(uname -m); build from source: https://github.com/$repo" >&2; exit 1 ;;
esac
if [ -n "${PAIL_VERSION:-}" ]; then
  base="https://github.com/$repo/releases/download/$PAIL_VERSION"
else
  base="https://github.com/$repo/releases/latest/download"
fi
name="pail-$os-$arch"
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
echo "downloading $name from $base"
curl -fsSL "$base/$name.tar.gz" -o "$tmp/$name.tar.gz"
curl -fsSL "$base/SHA256SUMS" -o "$tmp/SHA256SUMS"
want=$(grep " $name.tar.gz\$" "$tmp/SHA256SUMS" | cut -d' ' -f1)
if command -v sha256sum >/dev/null 2>&1; then
  got=$(sha256sum "$tmp/$name.tar.gz" | cut -d' ' -f1)
else
  got=$(shasum -a 256 "$tmp/$name.tar.gz" | cut -d' ' -f1)
fi
[ -n "$want" ] && [ "$want" = "$got" ] || { echo "pail: checksum mismatch for $name.tar.gz" >&2; exit 1; }
tar xzf "$tmp/$name.tar.gz" -C "$tmp"
mkdir -p "$dir"
mv "$tmp/$name/pail" "$dir/pail"
chmod +x "$dir/pail"
echo "installed $("$dir/pail" --version 2>/dev/null || echo pail) at $dir/pail"
case ":$PATH:" in
  *":$dir:"*) ;;
  *) echo "add it to your PATH: export PATH=\"$dir:\$PATH\"" ;;
esac
echo "next: pail auth install gcloud   (credential providers: https://github.com/$repo#auth-credential-providers-are-extensions)"
