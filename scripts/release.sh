#!/bin/bash
# Cuts a release locally, without pushing anything:
#
#   scripts/release.sh <version>            # e.g. 1.0.0
#
#  1. FORCE=1 scripts/build.sh (clean, verified build)
#  2. scripts/package.sh v<version> (zips, checksums, release Package.swift)
#  3. a release commit on top of HEAD whose only change is the generated
#     Package.swift (the work tree and branch are not touched), and the
#     annotated tag v<version> on it.
#
# It prints the commands that publish the release (push the tag, create the
# GitHub release with build/dist/v<version>/*). Pushing the tag also starts
# .github/workflows/release.yml, which rebuilds on CI and uploads the assets
# itself only if they reproduce the tag's checksums; publishing the local
# build with `gh release create` first is equally valid.
#
# Set HEELER_NATIVES_SKIP_BUILD=1 to package an existing, verified
# build/Artifacts instead of rebuilding (CI builds in a separate step).
set -euo pipefail

# shellcheck source=lib/common.sh
source "$(dirname "${BASH_SOURCE[0]}")/lib/common.sh"

VERSION="${1:?usage: scripts/release.sh <version>}"
VERSION="${VERSION#v}"
TAG="v${VERSION}"
[[ "${TAG}" =~ ^v[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?$ ]] || die "version must look like 1.2.3 (got ${VERSION})"
cd "${ROOT_DIR}"
git rev-parse -q --verify "refs/tags/${TAG}" >/dev/null && die "tag ${TAG} already exists"
[[ -z "$(git status --porcelain --ignore-submodules=none)" ]] \
    || die "the work tree has uncommitted changes; a release is cut from a clean commit"
[[ "${BUILD_DIR}" == "${ROOT_DIR}/build" ]] || die "unset HEELER_NATIVES_BUILD_DIR for releases"

if [[ "${HEELER_NATIVES_SKIP_BUILD:-0}" != "1" ]]; then
    FORCE=1 "${ROOT_DIR}/scripts/build.sh"
fi
"${ROOT_DIR}/scripts/package.sh" "${TAG}"
DIST="${BUILD_DIR}/dist/${TAG}"

# The release commit: HEAD's tree with Package.swift replaced, written
# through a temporary index so neither the work tree nor the branch moves.
index="$(mktemp "${TMPDIR:-/tmp}/heeler-natives-index.XXXXXX")"
trap 'rm -f "${index}"' EXIT
export GIT_INDEX_FILE="${index}"
git read-tree HEAD
blob="$(git hash-object -w "${DIST}/Package.swift")"
git update-index --cacheinfo "100644,${blob},Package.swift"
tree="$(git write-tree)"
unset GIT_INDEX_FILE
base="$(git rev-parse HEAD)"
commit="$(printf '%s\n\n%s\n' "chore(release): ${TAG}" \
    "Release manifest: binary targets download the ${TAG} release assets by URL and checksum. Built from ${base}." \
    | git commit-tree "${tree}" -p "${base}")"
git tag -a "${TAG}" "${commit}" -m "heeler-overlay-natives ${TAG}"

cat <<EOF

Tagged ${TAG} at ${commit} (release manifest on top of ${base}).
Nothing was pushed. To publish:

  git push origin ${TAG}
  gh release create ${TAG} --verify-tag --title "heeler-overlay-natives ${TAG}" \\
      --notes-file <(cat build/dist/${TAG}/checksums.txt) build/dist/${TAG}/*

Consumers then depend on:
  .package(url: "https://github.com/Ylarod/heeler-overlay-natives", exact: "${VERSION}")
EOF
