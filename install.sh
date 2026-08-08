#!/usr/bin/env bash
# Odyn installer — macOS only. Pulls a build from GitHub Releases, verifies its
# published SHA-256 checksum, installs it to /Applications, and clears the
# Gatekeeper quarantine flag. Fails closed: a missing or mismatched checksum
# aborts before anything is installed.
#
#   curl -fsSL --connect-timeout 10 https://raw.githubusercontent.com/aristocrat71/odyn/main/install.sh | bash
#
# macOS on Apple Silicon. Pin a specific version with ODYN_VERSION=vX.Y.Z;
# otherwise the latest release is used. No build toolchain required. Once
# installed, Odyn updates itself.
set -euo pipefail

REPO="aristocrat71/odyn"
API_BASE="https://api.github.com/repos/${REPO}/releases"
API="${API_BASE}/latest"
[ -n "${ODYN_VERSION:-}" ] && API="${API_BASE}/tags/${ODYN_VERSION}"

say() { printf '\033[1;32m==>\033[0m %s\n' "$1"; }
die() { printf '\033[1;31merror:\033[0m %s\n' "$1" >&2; exit 1; }

# GitHub serves these hosts from several CDN IPs, and a network that blackholes
# one (dropping SYNs rather than refusing them) leaves curl in SYN_SENT until the
# OS connect timeout — ~75s of silence per dead address. Cap the wait so curl
# moves on to the next IP quickly, and retry so a transient failure isn't fatal.
CURL=(curl --connect-timeout 10 --retry 3 --retry-connrefused)

[ "$(uname -s)" = "Darwin" ] || die "Odyn is macOS-only for now (got $(uname -s))."
# Apple Silicon only, and there is no Rosetta path: the embedding runtime
# (onnxruntime, via fastembed) publishes no x86_64-apple-darwin binary, so no
# Intel build exists to install. Say so here rather than let an arm64 bundle
# land on an Intel Mac and refuse to launch.
[ "$(uname -m)" = "arm64" ] || die "Odyn builds are Apple Silicon only (got $(uname -m)) — build from source instead."

# First asset download URL whose filename matches the given regex.
asset_url() {
  "${CURL[@]}" -fsSL "$API" \
    | grep -o '"browser_download_url": *"[^"]*"' \
    | sed 's/.*"\(https[^"]*\)"/\1/' \
    | grep -iE "$1" \
    | head -1
}

# Verify $1 against the "<file>.sha256" published next to its release asset ($2).
# Aborts on a missing checksum (fail closed) or any mismatch.
verify_sha() {
  local file="$1" url="$2" sums expected actual
  say "Verifying checksum…"
  sums="$("${CURL[@]}" -fsSL "${url}.sha256")" \
    || die "no published checksum for $(basename "$url") — refusing to install"
  expected="$(printf '%s\n' "$sums" | awk '{print $1}' | head -1)"
  [ -n "$expected" ] || die "empty checksum for $(basename "$url")"
  actual="$(shasum -a 256 "$file" | awk '{print $1}')"
  [ "$expected" = "$actual" ] \
    || die "checksum mismatch for $(basename "$url") (expected $expected, got $actual) — aborting"
}

say "Fetching the Odyn release…"
url="$(asset_url '\.dmg$')" || true
[ -n "${url:-}" ] || die "no macOS .dmg in that release"

tmp="$(mktemp -d)"
trap 'hdiutil detach "$tmp/mnt" -quiet 2>/dev/null || true; rm -rf "$tmp"' EXIT

say "Downloading $(basename "$url")…"
"${CURL[@]}" -fSL --progress-bar "$url" -o "$tmp/odyn.dmg"
verify_sha "$tmp/odyn.dmg" "$url"

mkdir -p "$tmp/mnt"
hdiutil attach "$tmp/odyn.dmg" -nobrowse -quiet -mountpoint "$tmp/mnt"
app="$(/usr/bin/find "$tmp/mnt" -maxdepth 1 -name '*.app' | head -1)"
[ -n "$app" ] || die "no .app inside the dmg"

name="$(basename "$app")"
running() { pgrep -f "/Applications/${name}/Contents/MacOS/" >/dev/null 2>&1; }

# Replacing a running copy pulls the bundle out from under it — and Odyn is a
# menu bar app, so it is easy to be running with no window to notice. Ask it to
# quit and only then overwrite. Matching on the install path, not the process
# name, so an editor with the word "odyn" in its window doesn't count, and so
# this never launches the app just to quit it.
if running; then
  say "Quitting the running copy…"
  osascript -e "quit app \"${name%.app}\"" >/dev/null 2>&1 || true
  for _ in 1 2 3 4 5 6 7 8 9 10; do
    running || break
    sleep 0.5
  done
  if running; then
    die "Odyn is still running — quit it from the menu bar and re-run this installer"
  fi
fi

say "Installing to /Applications…"
rm -rf "/Applications/$name"
cp -R "$app" /Applications/

# Odyn isn't notarized yet, so Gatekeeper would otherwise refuse to open it.
# Stripping the quarantine flag skips that prompt — safe here because the
# download was already checksum-verified above, and the Gatekeeper prompt on an
# un-notarized app is a click-through, not an integrity check.
xattr -dr com.apple.quarantine "/Applications/$name" 2>/dev/null || true

say "Done — launch Odyn from /Applications or Spotlight. Your config, brain and"
say "database live outside the bundle, so they survive every reinstall."
