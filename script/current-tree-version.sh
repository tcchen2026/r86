#!/bin/sh
# r86's version, v<yyyy>.<mm>.<dd>-<6 hex>: the date (UTC) and the first 6 hex
# digits of a commit (default HEAD), e.g. v2026.11.18-b0dd54; VibeOffice's
# scheme. The packaging scripts stamp it into what they build through
# R86_VERSION (build.rs embeds it); set R86_VERSION yourself to build a tree
# without .git, such as a git archive export. Where a platform wants numbers
# (Apple bundle versions, Windows FILEVERSION, Debian) the date is used, as
# 2026.11.18; Cargo.toml's version is only for builds without R86_VERSION.
#   script/current-tree-version.sh [commit]
set -eu
cd "$(dirname "$0")/.."
commit=${1:-HEAD}
day=$(TZ=UTC git log -1 --format=%cd --date=format-local:%Y.%m.%d "$commit")
hash=$(git rev-parse "$commit" | cut -c1-6)
echo "v$day-$hash"
