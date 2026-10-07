# shellcheck shell=bash
# Shared helpers for scripts/*.sh. Sourced, never executed.

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd -P)"
SOURCE_LOCK="${ROOT_DIR}/sources.lock"
# shellcheck source=../../sources.lock
source "${SOURCE_LOCK}"

FRAMEWORKS=(CTailscale CZeroTier CEasyTier)
LIBRARIES=(tailscale zerotier easytier)

# Downloads (Go toolchain, audit archives) and the Go module and build
# caches live outside the repository so clean checkouts and FORCE builds
# reuse them; every cached download is verified again on each use.
CACHE_DIR="${HEELER_NATIVES_CACHE_DIR:-${HOME}/Library/Caches/heeler-overlay-natives}"
BUILD_DIR="${HEELER_NATIVES_BUILD_DIR:-${ROOT_DIR}/build}"
mkdir -p "${CACHE_DIR}" "${BUILD_DIR}"
CACHE_DIR="$(cd "${CACHE_DIR}" && pwd -P)"
BUILD_DIR="$(cd "${BUILD_DIR}" && pwd -P)"
JOBS="${HEELER_NATIVES_JOBS:-$(sysctl -n hw.ncpu 2>/dev/null || echo 4)}"

die() {
    echo "error: $*" >&2
    exit 1
}

log() {
    echo "==> $*"
}

require_commands() {
    local command
    for command in "$@"; do
        command -v "${command}" >/dev/null || die "required command not found: ${command}"
    done
}

sha256_of() {
    shasum -a 256 "$1" | awk '{print $1}'
}

# Verifies a committed file (licence texts, go.mod/go.sum overrides).
verify_file() {
    local name="$1" path="$2" expected="$3" actual
    [[ -f "${path}" ]] || die "${name} is missing: ${path}"
    actual="$(sha256_of "${path}")"
    [[ "${actual}" == "${expected}" ]] || die "${name} hash mismatch (expected ${expected}, actual ${actual})"
}

# curl with fail-fast on stalled connections and retries. Proxies come from
# the usual https_proxy/all_proxy environment variables.
download() {
    local url="$1" output="$2"
    curl --fail --silent --show-error --location --proto '=https' --tlsv1.2 \
        --connect-timeout 20 --speed-time 30 --speed-limit 1024 \
        --retry 3 --retry-delay 1 --retry-all-errors \
        "${url}" --output "${output}"
}

# Prints the path of a verified copy of url in the download cache. A cached
# file is hashed on every use; a mismatch is discarded and downloaded again,
# and a fresh download that does not match is fatal.
fetch_verified() {
    local name="$1" url="$2" expected="$3"
    local directory="${CACHE_DIR}/downloads"
    local path="${directory}/${expected}-${url##*/}"
    mkdir -p "${directory}"
    if [[ -f "${path}" ]]; then
        if [[ "$(sha256_of "${path}")" == "${expected}" ]]; then
            echo "${path}"
            return 0
        fi
        echo "warning: cached ${name} does not match its hash; downloading again" >&2
        rm -f "${path}"
    fi
    local partial="${path}.partial.$$"
    download "${url}" "${partial}" >&2 || {
        rm -f "${partial}"
        die "could not download ${name} from ${url}"
    }
    local actual
    actual="$(sha256_of "${partial}")"
    [[ "${actual}" == "${expected}" ]] || {
        rm -f "${partial}"
        die "${name} hash mismatch (expected ${expected}, actual ${actual})"
    }
    mv "${partial}" "${path}"
    echo "${path}"
}

# Checks that a submodule (or nested submodule) is pinned at the commit in
# sources.lock: the superproject's index gitlink (HEAD's in a clean CI
# checkout) for top-level ones, the parent commit's tree for nested ones,
# and that the commit object is present locally.
check_gitlink() {
    local parent="$1" path="$2" expected="$3" parent_commit="${4:-}" actual
    if [[ -z "${parent_commit}" ]]; then
        actual="$(git -C "${parent}" ls-files --stage -- "${path}" | awk '$1 == "160000" { print $2 }')"
    else
        actual="$(git -C "${parent}" ls-tree "${parent_commit}" -- "${path}" | awk '$2 == "commit" { print $3 }')"
    fi
    [[ "${actual}" == "${expected}" ]] || die "${parent#"${ROOT_DIR}/"}:${path} is pinned at '${actual}', sources.lock says ${expected}"
    git -C "${parent}/${path}" cat-file -e "${expected}^{commit}" 2>/dev/null \
        || die "${parent#"${ROOT_DIR}/"}/${path} does not contain commit ${expected}; run git submodule update --init --recursive"
}

check_submodules() {
    check_gitlink "${ROOT_DIR}" upstream/libtailscale "${LIBTAILSCALE_COMMIT}"
    check_gitlink "${ROOT_DIR}" upstream/libzt "${LIBZT_COMMIT}"
    check_gitlink "${ROOT_DIR}" upstream/EasyTier "${EASYTIER_COMMIT}"
    local path repository_url
    for path in libtailscale:"${LIBTAILSCALE_REPO}" libzt:"${LIBZT_REPO}" EasyTier:"${EASYTIER_REPO}"; do
        repository_url="$(git -C "${ROOT_DIR}" config --file .gitmodules "submodule.upstream/${path%%:*}.url")"
        [[ "${repository_url%.git}" == "${path#*:}" ]] \
            || die ".gitmodules points upstream/${path%%:*} at ${repository_url}, sources.lock at ${path#*:}"
    done
    local libzt="${ROOT_DIR}/upstream/libzt"
    check_gitlink "${libzt}" ext/ZeroTierOne "${ZEROTIERONE_COMMIT}" "${LIBZT_COMMIT}"
    check_gitlink "${libzt}" ext/lwip "${LWIP_COMMIT}" "${LIBZT_COMMIT}"
    check_gitlink "${libzt}" ext/lwip-contrib "${LWIP_CONTRIB_COMMIT}" "${LIBZT_COMMIT}"
    local name url expected
    while read -r name url expected; do
        local actual
        actual="$(git -C "${libzt}" config --file .gitmodules "submodule.${name}.url")"
        [[ "${actual%.git}" == "${url}" ]] || die "libzt's .gitmodules points ${name} at ${actual}, sources.lock at ${url}"
    done <<EOF
ext/ZeroTierOne ${ZEROTIERONE_REPO} -
ext/lwip ${LWIP_REPO} -
ext/lwip-contrib ${LWIP_CONTRIB_REPO} -
EOF
}

# Writes `git archive` of a pinned commit into destination: the same files
# GitHub's commit archive holds (export-ignore and export-subst applied),
# read from the object database, so the submodule's work tree is never used
# or changed.
export_tree() {
    local repository="$1" commit="$2" destination="$3"
    mkdir -p "${destination}"
    git -C "${repository}" archive --format=tar "${commit}" | tar -xf - -C "${destination}"
}

# Applies an ordered "file:sha256" patch list from patch_dir to tree. Every
# patch in patch_dir must be listed, each must match its hash, and each must
# apply exactly (no fuzz); anything else is fatal.
apply_patches() {
    local patch_dir="$1" list="$2" tree="$3" label="$4"
    local entry listed=() present
    for entry in ${list}; do
        listed+=("${entry%%:*}")
    done
    present="$(cd "${patch_dir}" && ls -1 ./*.patch 2>/dev/null | sed 's|^\./||' | LC_ALL=C sort)"
    [[ "${present}" == "$(printf '%s\n' "${listed[@]}" | LC_ALL=C sort)" ]] \
        || die "${patch_dir#"${ROOT_DIR}/"} does not match its patch list in sources.lock"
    for entry in ${list}; do
        local name="${entry%%:*}" expected="${entry##*:}" actual
        actual="$(sha256_of "${patch_dir}/${name}")"
        [[ "${actual}" == "${expected}" ]] || die "${name} hash mismatch (expected ${expected}, actual ${actual})"
        patch -d "${tree}" -p1 --fuzz=0 --batch --forward --no-backup-if-mismatch --silent \
            < "${patch_dir}/${name}" || die "${name} does not apply to ${label}"
        echo "Applied ${name}."
    done
}

verify_licence_texts() {
    verify_file gpl-3.0 "${ROOT_DIR}/${GPL3_TEXT_FILE}" "${GPL3_TEXT_SHA256}"
    local dir="${ROOT_DIR}/${RUST_LICENSE_DIR}"
    verify_file rust-COPYRIGHT "${dir}/COPYRIGHT" "${RUST_COPYRIGHT_SHA256}"
    verify_file rust-LICENSE-APACHE "${dir}/LICENSE-APACHE" "${RUST_LICENSE_APACHE_SHA256}"
    verify_file rust-LICENSE-MIT "${dir}/LICENSE-MIT" "${RUST_LICENSE_MIT_SHA256}"
    local files
    files="$(find "${dir}" -type f -exec basename {} \; | LC_ALL=C sort | paste -sd ' ' -)"
    [[ "${files}" == "COPYRIGHT LICENSE-APACHE LICENSE-MIT" ]] \
        || die "${RUST_LICENSE_DIR} holds '${files}', expected COPYRIGHT LICENSE-APACHE LICENSE-MIT"
}

# The prefix every work directory is remapped to in object files.
REMAPPED_WORK_DIR="/heeler-overlay"

# Writes a clang wrapper that targets one iOS platform.
write_cc_wrapper() {
    local path="$1" sdk="$2" target="$3" sdk_path clang
    sdk_path="$(xcrun --sdk "${sdk}" --show-sdk-path)"
    clang="$(xcrun --sdk "${sdk}" --find clang)"
    cat > "${path}" <<EOF
#!/bin/sh
exec "${clang}" -target "${target}" -isysroot "${sdk_path}" "\$@"
EOF
    chmod +x "${path}"
}

# Creates a static framework bundle for one slice.
create_static_framework() {
    local name="$1" library="$2" platform="$3" output="$4"
    shift 4
    mkdir -p "${output}/Headers" "${output}/Modules"
    cp "${library}" "${output}/${name}"
    local header
    for header in "$@"; do
        cp "${header}" "${output}/Headers/"
    done
    cp "${ROOT_DIR}/include/${name}/${name}.h" "${output}/Headers/${name}.h"
    cp "${ROOT_DIR}/include/${name}/module.modulemap" "${output}/Modules/module.modulemap"
    cat > "${output}/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleDevelopmentRegion</key>
    <string>en</string>
    <key>CFBundleExecutable</key>
    <string>${name}</string>
    <key>CFBundleIdentifier</key>
    <string>dev.bybee.heeler.${name}</string>
    <key>CFBundleInfoDictionaryVersion</key>
    <string>6.0</string>
    <key>CFBundleName</key>
    <string>${name}</string>
    <key>CFBundlePackageType</key>
    <string>FMWK</string>
    <key>CFBundleShortVersionString</key>
    <string>1.0</string>
    <key>CFBundleSupportedPlatforms</key>
    <array><string>${platform}</string></array>
    <key>CFBundleVersion</key>
    <string>1</string>
    <key>MinimumOSVersion</key>
    <string>${DEPLOYMENT_TARGET}</string>
</dict>
</plist>
EOF
}

# Combines the device and Simulator frameworks into an XCFramework whose
# Info.plist is written canonically (slices sorted), so identical inputs
# give identical bytes.
create_xcframework() {
    local name="$1" device="$2" simulator="$3" output="$4"
    rm -rf "${output}"
    xcodebuild -create-xcframework -framework "${device}" -framework "${simulator}" \
        -output "${output}" >/dev/null
    python3 - "${output}/Info.plist" <<'PY'
import plistlib
import sys

path = sys.argv[1]
with open(path, "rb") as handle:
    info = plistlib.load(handle)
info["AvailableLibraries"] = sorted(
    info["AvailableLibraries"], key=lambda library: library["LibraryIdentifier"])
with open(path, "wb") as handle:
    plistlib.dump(info, handle, fmt=plistlib.FMT_XML, sort_keys=True)
PY
}

# Prints the first C comment block of a file that contains a marker.
extract_comment_block() {
    local source_file="$1" marker="$2"
    awk -v marker="${marker}" '
        /^[[:space:]]*\/\*/ { in_block = 1; block = "" }
        in_block {
            block = block $0 ORS
            if ($0 ~ /\*\//) {
                if (index(block, marker) > 0) { printf "%s", block; exit }
                in_block = 0
            }
        }
    ' "${source_file}"
}

# Tool versions that reach every library's objects.
apple_toolchain_fingerprint() {
    echo "xcode: $(xcodebuild -version | paste -sd ';' -)"
    echo "clang: $(xcrun clang --version | sed -n '1p')"
    echo "iphoneos-sdk: $(xcrun --sdk iphoneos --show-sdk-version) $(xcrun --sdk iphoneos --show-sdk-build-version 2>/dev/null || true)"
    echo "iphonesimulator-sdk: $(xcrun --sdk iphonesimulator --show-sdk-version) $(xcrun --sdk iphonesimulator --show-sdk-build-version 2>/dev/null || true)"
    echo "python: $(python3 --version 2>&1)"
}

# Hash lines for files (relative to the repository root), sorted.
hash_files() {
    local file
    for file in "$@"; do
        [[ -f "${ROOT_DIR}/${file}" ]] || die "fingerprint input is missing: ${file}"
        echo "file: ${file} $(sha256_of "${ROOT_DIR}/${file}")"
    done | LC_ALL=C sort
}

# Hash lines for every tracked-or-not file under a directory (relative to the
# repository root), skipping build output directories.
hash_tree() {
    local directory="$1"
    (cd "${ROOT_DIR}" && find "${directory}" \
        \( -name target -o -name vendor -o -name out -o -name __pycache__ -o -name .DS_Store \) -prune \
        -o -type f -print | LC_ALL=C sort) | while read -r file; do
        echo "file: ${file} $(sha256_of "${ROOT_DIR}/${file}")"
    done
}
