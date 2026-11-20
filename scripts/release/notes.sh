#!/usr/bin/env bash
# Print the release notes for a version on stdout (Markdown).
#
# Usage:
#   scripts/release/notes.sh --version X.Y.Z [--tag vX.Y.Z] [--changelog CHANGELOG.md]
#                            [--repo owner/name] [--sums SHA256SUMS]
#
# The body is taken from the first of these that has content:
#   1. the "## [X.Y.Z]" section of the changelog,
#   2. its "## [Unreleased]" section,
#   3. the message of the annotated tag (git tag -l --format='%(contents)'),
#   4. a one-line placeholder.
# Then an "Install" section with the one-line installer, the Homebrew tap and,
# with --sums, the SHA256SUMS of the attached tarballs.
set -euo pipefail

die() { printf '[notes] error: %s\n' "$*" >&2; exit 1; }

VERSION=""
TAG=""
CHANGELOG="CHANGELOG.md"
REPO="veerareddyvishal144/warren"
SUMS=""
while [ $# -gt 0 ]; do
    case "$1" in
        --version) VERSION="${2#v}"; shift 2 ;;
        --tag) TAG="$2"; shift 2 ;;
        --changelog) CHANGELOG="$2"; shift 2 ;;
        --repo) REPO="$2"; shift 2 ;;
        --sums) SUMS="$2"; shift 2 ;;
        -h|--help) sed -n '2,/^set -euo pipefail/p' "$0" | sed '$d' | sed 's/^# \{0,1\}//'; exit 0 ;;
        *) die "unknown option $1" ;;
    esac
done
[ -n "$VERSION" ] || die "--version is required"
[ -n "$TAG" ] || TAG="v$VERSION"

# Lines of the changelog section whose heading starts with "## [<name>]",
# without the link-reference lines at the end of the file and without leading
# or trailing blank lines.
section() {
    [ -f "$CHANGELOG" ] || return 0
    awk -v h="## [$1]" '
        index($0, h) == 1 { on = 1; next }
        on && /^## \[/ { exit }
        on && /^\[[^]]+\]: https?:\/\// { next }
        on { print }
    ' "$CHANGELOG" | sed -e :a -e '/^[[:space:]]*$/{$d;N;ba' -e '}' | sed '/./,$!d'
}

body="$(section "$VERSION")"
source="the CHANGELOG section for $VERSION"
if [ -z "$body" ]; then
    body="$(section Unreleased)"
    source="the CHANGELOG Unreleased section"
fi
if [ -z "$body" ] && command -v git >/dev/null 2>&1; then
    body="$(git tag -l --format='%(contents)' "$TAG" 2>/dev/null | sed '/^-----BEGIN PGP SIGNATURE-----/,$d' | sed -e :a -e '/^[[:space:]]*$/{$d;N;ba' -e '}')"
    source="the message of tag $TAG"
fi
if [ -z "$body" ]; then
    body="Warren $VERSION."
    source="a placeholder (no CHANGELOG section and no tag message)"
fi
printf '[notes] body from %s\n' "$source" >&2

cat <<EOF
$body

## Install

\`\`\`sh
curl -fsSL https://raw.githubusercontent.com/$REPO/main/scripts/install.sh | WARREN_VERSION=$VERSION sh
\`\`\`

Homebrew: \`brew install ${REPO%%/*}/warren/warren\` (tap \`${REPO%%/*}/homebrew-warren\`). Tarballs: \`warren-$VERSION-darwin-arm64.tar.gz\` (Apple silicon), \`warren-$VERSION-linux-x86_64.tar.gz\`, \`warren-$VERSION-linux-arm64.tar.gz\`; each holds \`warrend\`, \`warren\`, \`warren-mcp\`, the static \`warren-gateway\` and the LICENSE. Verify a download against \`SHA256SUMS\` with \`sha256sum --check --ignore-missing SHA256SUMS\` (macOS: \`shasum -a 256 -c --ignore-missing SHA256SUMS\`). Details: [docs/install.md](https://github.com/$REPO/blob/$TAG/docs/install.md).
EOF

if [ -n "$SUMS" ]; then
    [ -f "$SUMS" ] || die "$SUMS does not exist"
    printf '\n### SHA256SUMS\n\n```\n'
    cat "$SUMS"
    printf '```\n'
fi
