#!/bin/sh
# Adds Jykre Studio to the desktop's applications, with its icon, for the
# current user. Run it from the extracted release folder, next to `jykre`;
# run it again after moving the folder. `--uninstall` removes the entry.
set -eu

APP_ID=io.github.alandariva.JykreStudio
here=$(cd "$(dirname "$0")" && pwd)
data=${XDG_DATA_HOME:-$HOME/.local/share}
applications="$data/applications"
icons="$data/icons/hicolor"
entry="$applications/$APP_ID.desktop"
icon="$icons/scalable/apps/$APP_ID.svg"

refresh() {
    if command -v update-desktop-database >/dev/null 2>&1; then
        update-desktop-database -q "$applications" || true
    fi
    if command -v gtk-update-icon-cache >/dev/null 2>&1; then
        gtk-update-icon-cache -q -t "$icons" || true
    fi
}

case "${1-}" in
--uninstall)
    rm -f "$entry" "$icon"
    refresh
    echo "Removed Jykre Studio from the applications."
    exit 0
    ;;
"") ;;
*)
    echo "usage: $0 [--uninstall]" >&2
    exit 2
    ;;
esac

binary="$here/jykre"
if [ ! -x "$binary" ]; then
    echo "jykre was not found next to this script ($binary)." >&2
    exit 1
fi
# The entry quotes the path; these characters would need escaping in it.
case "$binary" in
*[\"\`\$\\%]*)
    echo "Move the folder to a path without \" \` \$ \\ or %: $here" >&2
    exit 1
    ;;
esac

mkdir -p "$applications" "$icons/scalable/apps"
cp "$here/$APP_ID.svg" "$icon"
EXEC="\"$binary\"" awk '/^Exec=/ { print "Exec=" ENVIRON["EXEC"]; next } { print }' \
    "$here/$APP_ID.desktop" >"$entry"
refresh
echo "Added Jykre Studio to the applications ($entry)."
