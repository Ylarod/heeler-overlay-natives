#!/bin/bash
# Cross-checks the pinned submodules against GitHub's commit archives: each
# archive is downloaded (into the download cache), verified against its
# *_ARCHIVE_SHA256 in sources.lock, and compared file by file with
# `git archive` of the submodule commit, which is what scripts/build.sh
# compiles. Network access is needed only for archives not yet cached.
#
#   scripts/audit-upstream.sh
set -euo pipefail

# shellcheck source=lib/common.sh
source "$(dirname "${BASH_SOURCE[0]}")/lib/common.sh"

require_commands git curl shasum tar diff
check_submodules

SCRATCH="$(mktemp -d "${TMPDIR:-/tmp}/heeler-natives-audit.XXXXXX")"
trap 'rm -rf "${SCRATCH}"' EXIT

audit() {
    local name="$1" repository_url="$2" commit="$3" expected="$4" checkout="$5"
    local archive
    archive="$(fetch_verified "${name}" "${repository_url}/archive/${commit}.tar.gz" "${expected}")"
    mkdir -p "${SCRATCH}/${name}/archive" "${SCRATCH}/${name}/git"
    tar -xzf "${archive}" -C "${SCRATCH}/${name}/archive" --strip-components 1
    export_tree "${checkout}" "${commit}" "${SCRATCH}/${name}/git"
    # GitHub's archive leaves submodule directories empty, as git archive does.
    if diff -r --no-dereference "${SCRATCH}/${name}/archive" "${SCRATCH}/${name}/git" >"${SCRATCH}/${name}.diff"; then
        echo "${name} ${commit}: archive sha256 ${expected} matches git archive of the submodule"
    else
        head -40 "${SCRATCH}/${name}.diff" >&2
        die "${name}: GitHub's archive and git archive of ${commit} differ"
    fi
}

libzt="${ROOT_DIR}/upstream/libzt"
audit libtailscale "${LIBTAILSCALE_REPO}" "${LIBTAILSCALE_COMMIT}" "${LIBTAILSCALE_ARCHIVE_SHA256}" "${ROOT_DIR}/upstream/libtailscale"
audit libzt "${LIBZT_REPO}" "${LIBZT_COMMIT}" "${LIBZT_ARCHIVE_SHA256}" "${libzt}"
audit ZeroTierOne "${ZEROTIERONE_REPO}" "${ZEROTIERONE_COMMIT}" "${ZEROTIERONE_ARCHIVE_SHA256}" "${libzt}/ext/ZeroTierOne"
audit lwip "${LWIP_REPO}" "${LWIP_COMMIT}" "${LWIP_ARCHIVE_SHA256}" "${libzt}/ext/lwip"
audit lwip-contrib "${LWIP_CONTRIB_REPO}" "${LWIP_CONTRIB_COMMIT}" "${LWIP_CONTRIB_ARCHIVE_SHA256}" "${libzt}/ext/lwip-contrib"
audit EasyTier "${EASYTIER_REPO}" "${EASYTIER_COMMIT}" "${EASYTIER_ARCHIVE_SHA256}" "${ROOT_DIR}/upstream/EasyTier"
echo "Every submodule commit matches the commit archive Heeler audited."
