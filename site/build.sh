#!/bin/sh
# site/index.html is a page body (no <html>/<head> wrapper), as published for preview.
# For static hosting at pail.build, wrap it into a full document: dist/index.html.
set -eu
here=$(cd "$(dirname "$0")" && pwd)
mkdir -p "$here/dist"
{
  printf '<!doctype html>\n<html lang="en">\n<head>\n<meta charset="utf-8">\n'
  printf '<meta name="viewport" content="width=device-width, initial-scale=1, viewport-fit=cover">\n'
  printf '<style>:root{padding-top:env(safe-area-inset-top,0px);padding-bottom:env(safe-area-inset-bottom,0px)}body{margin:0}img{max-width:100%%}[hidden]{display:none!important}</style>\n'
  printf '</head>\n<body>\n'
  cat "$here/index.html"
  printf '\n</body>\n</html>\n'
} > "$here/dist/index.html"
cp "$here/install.sh" "$here/dist/install.sh"
echo "wrote $here/dist/index.html and install.sh"
