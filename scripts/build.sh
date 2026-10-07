#!/bin/bash
# Builds CTailscale, CZeroTier, and CEasyTier XCFrameworks (arm64 iPhoneOS
# and arm64 iPhone Simulator) into build/Artifacts, with notices,
# PROVENANCE.md, and SHA256SUMS, then runs scripts/verify.sh.
#
#   scripts/build.sh                    # build what changed, skip the rest
#   scripts/build.sh --only zerotier    # one library (comma-separated list)
#   scripts/build.sh --sources-only     # check submodules, hashes, patches
#   FORCE=1 scripts/build.sh            # clean rebuild (releases, repro checks)
#
# Each library is skipped when its input fingerprint (upstream commits,
# patch hashes, glue sources, scripts, toolchain versions, deployment
# target) matches the last successful build of it in build/stage/<library>.
# Environment: HEELER_NATIVES_CACHE_DIR (downloads and Go caches, default
# ~/Library/Caches/heeler-overlay-natives), HEELER_NATIVES_BUILD_DIR (default
# build/), HEELER_NATIVES_JOBS, HEELER_NATIVES_GOPROXY,
# HEELER_NATIVES_SIGNING_IDENTITY (codesign the XCFrameworks).
set -euo pipefail

# shellcheck source=lib/common.sh
source "$(dirname "${BASH_SOURCE[0]}")/lib/common.sh"
# shellcheck source=lib/tailscale.sh
source "${ROOT_DIR}/scripts/lib/tailscale.sh"
# shellcheck source=lib/zerotier.sh
source "${ROOT_DIR}/scripts/lib/zerotier.sh"
# shellcheck source=lib/easytier.sh
source "${ROOT_DIR}/scripts/lib/easytier.sh"

SELECTED=("${LIBRARIES[@]}")
SOURCES_ONLY=0
while [[ $# -gt 0 ]]; do
    case "$1" in
        --only)
            IFS=',' read -r -a SELECTED <<<"${2:?--only needs a library list}"
            shift 2
            ;;
        --sources-only)
            SOURCES_ONLY=1
            shift
            ;;
        -h|--help)
            sed -n '2,19p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
            exit 0
            ;;
        *)
            die "unknown argument: $1"
            ;;
    esac
done
for library in "${SELECTED[@]}"; do
    case "${library}" in
        tailscale|zerotier|easytier) ;;
        *) die "unknown library: ${library} (tailscale, zerotier, easytier)" ;;
    esac
done

require_commands git curl shasum tar patch rsync make cmake xcodebuild xcrun codesign awk \
    cargo rustup rustc python3 unzip
[[ "$(uname -s)-$(uname -m)" == "Darwin-arm64" ]] \
    || die "the pinned Go toolchain is darwin-arm64; build on an Apple silicon Mac"
[[ "$(sed -n 's/^channel = "\(.*\)"/\1/p' "${ROOT_DIR}/native/easytier/rust-toolchain.toml")" == "${EASYTIER_RUST_TOOLCHAIN}" ]] \
    || die "native/easytier/rust-toolchain.toml does not pin ${EASYTIER_RUST_TOOLCHAIN}"

check_submodules
verify_licence_texts
verify_file go.mod "${ROOT_DIR}/${TAILSCALE_GO_MOD}" "${LIBTAILSCALE_GO_MOD_SHA256}"
verify_file go.sum "${ROOT_DIR}/${TAILSCALE_GO_SUM}" "${LIBTAILSCALE_GO_SUM_SHA256}"

if [[ "${SOURCES_ONLY}" == "1" ]]; then
    scratch="$(mktemp -d "${TMPDIR:-/tmp}/heeler-natives-sources.XXXXXX")"
    trap 'rm -rf "${scratch}"' EXIT
    fetch_verified go "${GO_URL}" "${GO_SHA256}" >/dev/null
    fetch_verified protoc "${PROTOC_URL}" "${PROTOC_SHA256}" >/dev/null
    export_tree "${ROOT_DIR}/upstream/libtailscale" "${LIBTAILSCALE_COMMIT}" "${scratch}/libtailscale"
    grep -Fxq "require tailscale.com ${LIBTAILSCALE_UPSTREAM_TAILSCALE_VERSION}" "${scratch}/libtailscale/go.mod" \
        || die "libtailscale no longer requires tailscale.com ${LIBTAILSCALE_UPSTREAM_TAILSCALE_VERSION}"
    prepare_libzt_source "${scratch}/libzt"
    sync_easytier_source "${scratch}/easytier"
    echo "Verified the submodule pins, Go ${GO_VERSION}, protoc ${PROTOC_VERSION}, the go.mod/go.sum overrides, the libzt and EasyTier patches, and the committed licence texts."
    exit 0
fi

STAGE_ROOT="${BUILD_DIR}/stage"
ARTIFACTS="${BUILD_DIR}/Artifacts"
if [[ "${FORCE:-0}" == "1" ]]; then
    log "FORCE=1: clean rebuild of ${SELECTED[*]}"
    for library in "${SELECTED[@]}"; do
        rm -rf "${STAGE_ROOT:?}/${library}" "${BUILD_DIR}/work/${library}"
    done
    rm -f "${BUILD_DIR}/Artifacts.fingerprint"
fi
mkdir -p "${STAGE_ROOT}"

stage_complete() {
    local stage="$1" framework="$2"
    [[ -f "${stage}/FINGERPRINT" && -f "${stage}/provenance.md" && -d "${stage}/Notices" \
        && -f "${stage}/${framework}.xcframework/Info.plist" ]]
}

framework_of() {
    case "$1" in
        tailscale) echo CTailscale ;;
        zerotier) echo CZeroTier ;;
        easytier) echo CEasyTier ;;
    esac
}

for library in "${SELECTED[@]}"; do
    framework="$(framework_of "${library}")"
    stage="${STAGE_ROOT}/${library}"
    fingerprint="$("${library}_fingerprint")"
    if [[ "${FORCE:-0}" != "1" ]] && stage_complete "${stage}" "${framework}" \
        && [[ "$(cat "${stage}/FINGERPRINT")" == "${fingerprint}" ]]; then
        log "${framework}: inputs unchanged, skipped (FORCE=1 rebuilds)"
        continue
    fi
    if [[ -f "${stage}/FINGERPRINT" ]]; then
        log "${framework}: inputs changed:"
        diff <(cat "${stage}/FINGERPRINT") <(echo "${fingerprint}") | sed -n 's/^[<>] /    /p' | head -20 || true
    fi
    log "${framework}: building"
    started=${SECONDS}
    rm -rf "${stage}.partial"
    mkdir -p "${stage}.partial"
    "build_${library}" "${stage}.partial"
    echo "${fingerprint}" > "${stage}.partial/FINGERPRINT"
    rm -rf "${stage}"
    mv "${stage}.partial" "${stage}"
    log "${framework}: built in $((SECONDS - started))s"
done

# ---------------------------------------------------------------------------
# Assemble build/Artifacts from the three stages
# ---------------------------------------------------------------------------

for library in "${LIBRARIES[@]}"; do
    if ! stage_complete "${STAGE_ROOT}/${library}" "$(framework_of "${library}")"; then
        log "build/stage/${library} is missing; build/Artifacts is not assembled (run without --only)"
        exit 0
    fi
done

SIGNING_IDENTITY="${HEELER_NATIVES_SIGNING_IDENTITY:-}"
assembly_fingerprint="$(
    for library in "${LIBRARIES[@]}"; do
        echo "${library}: $(sha256_of "${STAGE_ROOT}/${library}/FINGERPRINT")"
    done
    echo "signing: ${SIGNING_IDENTITY}"
    hash_files scripts/build.sh scripts/lib/common.sh
)"
if [[ "${FORCE:-0}" != "1" && -f "${BUILD_DIR}/Artifacts.fingerprint" && -d "${ARTIFACTS}" ]] \
    && [[ "$(cat "${BUILD_DIR}/Artifacts.fingerprint")" == "${assembly_fingerprint}" ]]; then
    log "build/Artifacts: up to date"
else
    log "build/Artifacts: assembling"
    rm -f "${BUILD_DIR}/Artifacts.fingerprint"
    assembly="${BUILD_DIR}/Artifacts.partial"
    rm -rf "${assembly}"
    mkdir -p "${assembly}/Notices"
    for library in "${LIBRARIES[@]}"; do
        framework="$(framework_of "${library}")"
        cp -Rc "${STAGE_ROOT}/${library}/${framework}.xcframework" "${assembly}/" 2>/dev/null \
            || cp -R "${STAGE_ROOT}/${library}/${framework}.xcframework" "${assembly}/"
        cp "${STAGE_ROOT}/${library}/Notices/"* "${assembly}/Notices/"
    done

    signing_status="unsigned (HEELER_NATIVES_SIGNING_IDENTITY not provided)"
    if [[ -n "${SIGNING_IDENTITY}" ]]; then
        for framework in "${FRAMEWORKS[@]}"; do
            codesign --timestamp --sign "${SIGNING_IDENTITY}" "${assembly}/${framework}.xcframework"
        done
        signing_status="signed with ${SIGNING_IDENTITY}"
    fi

    {
        cat <<EOF
# Overlay native artifact provenance

Built by scripts/build.sh of https://github.com/Ylarod/heeler-overlay-natives;
paths below are relative to that repository.

## Build environment

- Xcode: $(xcodebuild -version | paste -sd ';' -)
- Compiler: $(xcrun clang --version | sed -n '1p')
- iPhoneOS SDK: $(xcrun --sdk iphoneos --show-sdk-version)
- iPhone Simulator SDK: $(xcrun --sdk iphonesimulator --show-sdk-version)
- Deployment target: iOS ${DEPLOYMENT_TARGET}
- Configuration: Release, static libraries, arm64 device and arm64 Simulator
- Reproducibility: ZERO_AR_DATE=1; work directories remapped to ${REMAPPED_WORK_DIR}, the EasyTier crate to /heeler-easytier, its target directory to /target, Cargo's home to /cargo
- Signature: ${signing_status}
- Build command: scripts/build.sh

EOF
        for library in "${LIBRARIES[@]}"; do
            cat "${STAGE_ROOT}/${library}/provenance.md"
            echo
        done
    } > "${assembly}/PROVENANCE.md"

    (
        cd "${assembly}"
        find ./* -type f ! -name SHA256SUMS | sed 's|^\./||' | LC_ALL=C sort \
            | tr '\n' '\0' | xargs -0 shasum -a 256 > SHA256SUMS
    )
    rm -rf "${ARTIFACTS}"
    mv "${assembly}" "${ARTIFACTS}"
    echo "${assembly_fingerprint}" > "${BUILD_DIR}/Artifacts.fingerprint"
fi

"${ROOT_DIR}/scripts/verify.sh" "${ARTIFACTS}"
log "Built and verified ${ARTIFACTS#"${ROOT_DIR}/"}"
