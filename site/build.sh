#!/bin/sh
# Pages in site/ are written as page bodies (everything above the "<!-- /head -->" line is
# head material), as published for preview. For static hosting at pail.build, wrap each
# into a full document in dist/, and copy the stylesheet and install.sh beside them.
set -eu
here=$(cd "$(dirname "$0")" && pwd)
mkdir -p "$here/dist"
for page in index how-it-works; do
  src="$here/$page.html"
  {
    printf '<!doctype html>\n<html lang="en">\n<head>\n<meta charset="utf-8">\n'
    printf '<meta name="viewport" content="width=device-width, initial-scale=1, viewport-fit=cover">\n'
    sed '/<!-- \/head -->/,$d' "$src"
    printf '</head>\n<body>\n'
    sed '1,/<!-- \/head -->/d' "$src"
    printf '\n</body>\n</html>\n'
  } > "$here/dist/$page.html"
done
cp "$here/site.css" "$here/install.sh" "$here/dist/"
echo "wrote $here/dist: index.html how-it-works.html site.css install.sh"
