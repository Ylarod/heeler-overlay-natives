# shellcheck shell=bash
# CTailscale: libtailscale as a Go c-archive. Sourced by scripts/build.sh.

TAILSCALE_GO_FILE="native/tailscale/heeler_tailscale.go"
TAILSCALE_GO_MOD="native/tailscale/go.mod"
TAILSCALE_GO_SUM="native/tailscale/go.sum"

tailscale_fingerprint() {
    echo "library: tailscale"
    echo "deployment-target: ${DEPLOYMENT_TARGET}"
    echo "libtailscale: ${LIBTAILSCALE_COMMIT}"
    echo "go: ${GO_VERSION} ${GO_URL} ${GO_SHA256}"
    echo "tailscale.com: ${LIBTAILSCALE_UPSTREAM_TAILSCALE_VERSION} -> ${TAILSCALE_MODULE_VERSION} ${LIBTAILSCALE_GO_MOD_SHA256} ${LIBTAILSCALE_GO_SUM_SHA256}"
    echo "goproxy: ${HEELER_NATIVES_GOPROXY:-https://proxy.golang.org}"
    apple_toolchain_fingerprint
    hash_files scripts/build.sh scripts/lib/common.sh scripts/lib/tailscale.sh \
        "${TAILSCALE_GO_FILE}" "${TAILSCALE_GO_MOD}" "${TAILSCALE_GO_SUM}" \
        include/CTailscale/CTailscale.h include/CTailscale/module.modulemap
}

build_tailscale() {
    local stage="$1"
    local work="${BUILD_DIR}/work/tailscale"
    chmod -R u+w "${work}" 2>/dev/null || true
    rm -rf "${work}"
    mkdir -p "${work}/src"
    work="$(cd "${work}" && pwd -P)"
    local source="${work}/src/libtailscale"

    local go_archive
    go_archive="$(fetch_verified go "${GO_URL}" "${GO_SHA256}")"
    mkdir -p "${work}/go"
    tar -xzf "${go_archive}" -C "${work}/go" --strip-components 1

    export_tree "${ROOT_DIR}/upstream/libtailscale" "${LIBTAILSCALE_COMMIT}" "${source}"

    # libtailscale's own go.mod pins an older tailscale.com; the committed
    # go.mod and go.sum replace them, and -mod=readonly refuses any change.
    verify_file go.mod "${ROOT_DIR}/${TAILSCALE_GO_MOD}" "${LIBTAILSCALE_GO_MOD_SHA256}"
    verify_file go.sum "${ROOT_DIR}/${TAILSCALE_GO_SUM}" "${LIBTAILSCALE_GO_SUM_SHA256}"
    grep -Fxq "require tailscale.com ${LIBTAILSCALE_UPSTREAM_TAILSCALE_VERSION}" "${source}/go.mod" \
        || die "libtailscale ${LIBTAILSCALE_COMMIT} no longer requires tailscale.com ${LIBTAILSCALE_UPSTREAM_TAILSCALE_VERSION}; regenerate native/tailscale/go.mod and go.sum"
    awk -v version="${TAILSCALE_MODULE_VERSION}" '
        ($1 == "tailscale.com" && $2 == version && NF == 2) ||
        ($1 == "require" && $2 == "tailscale.com" && $3 == version && NF == 3) { found = 1 }
        END { exit !found }
    ' "${ROOT_DIR}/${TAILSCALE_GO_MOD}" || die "native/tailscale/go.mod does not require tailscale.com ${TAILSCALE_MODULE_VERSION}"
    cp "${ROOT_DIR}/${TAILSCALE_GO_MOD}" "${source}/go.mod"
    cp "${ROOT_DIR}/${TAILSCALE_GO_SUM}" "${source}/go.sum"

    # heeler_tailscale.go joins libtailscale's package main: it exports
    # heeler_tailscale_disable_log_upload, heeler_tailscale_log_upload_state,
    # and heeler_tailscale_logout.
    [[ ! -e "${source}/heeler_tailscale.go" ]] || die "libtailscale already contains heeler_tailscale.go"
    cp "${ROOT_DIR}/${TAILSCALE_GO_FILE}" "${source}/heeler_tailscale.go"

    (
        export GOROOT="${work}/go"
        export PATH="${GOROOT}/bin:${PATH}"
        export GOTOOLCHAIN=local
        export GOPATH="${work}/gopath"
        # The module cache is shared between builds; `go mod verify` below
        # re-checks every cached module against go.sum.
        export GOMODCACHE="${CACHE_DIR}/go-mod"
        if [[ "${FORCE:-0}" == "1" ]]; then
            export GOCACHE="${work}/gocache"
        else
            export GOCACHE="${CACHE_DIR}/go-build"
        fi
        export GOFLAGS="-mod=readonly -modcacherw"
        unset GOEXPERIMENT GOWORK
        export GOPROXY="${HEELER_NATIVES_GOPROXY:-https://proxy.golang.org}"
        export GOSUMDB="sum.golang.org"
        export GONOSUMDB="" GOPRIVATE="" GOINSECURE=""
        export ZERO_AR_DATE=1
        local prefix_map="-ffile-prefix-map=${work}=${REMAPPED_WORK_DIR}"

        [[ "$(go env GOVERSION)" == "${GO_VERSION}" ]] || die "Go toolchain reports $(go env GOVERSION), expected ${GO_VERSION}"

        local name sdk target
        for name in device simulator; do
            if [[ "${name}" == device ]]; then
                sdk=iphoneos target="arm64-apple-ios${DEPLOYMENT_TARGET}"
            else
                sdk=iphonesimulator target="arm64-apple-ios${DEPLOYMENT_TARGET}-simulator"
            fi
            write_cc_wrapper "${work}/cc-${name}.sh" "${sdk}" "${target}"
            mkdir -p "${work}/tailscale-${name}"
            log "go build CTailscale (${name})"
            (
                cd "${source}"
                env CGO_ENABLED=1 GOOS=ios GOARCH=arm64 CC="${work}/cc-${name}.sh" \
                    CGO_CFLAGS="-O2 ${prefix_map}" CGO_LDFLAGS="" \
                    go build -trimpath -buildvcs=false -tags ios \
                        -ldflags "-s -w -buildid=" \
                        -buildmode=c-archive \
                        -o "${work}/tailscale-${name}/libtailscale.a" .
            )
        done

        # Modules reused from the shared cache must still match go.sum.
        (cd "${source}" && go mod verify)

        local frameworks="${work}/Frameworks"
        create_static_framework CTailscale "${work}/tailscale-device/libtailscale.a" iPhoneOS \
            "${frameworks}/device/CTailscale.framework" "${source}/tailscale.h"
        create_static_framework CTailscale "${work}/tailscale-simulator/libtailscale.a" iPhoneSimulator \
            "${frameworks}/simulator/CTailscale.framework" "${source}/tailscale.h"
        create_xcframework CTailscale "${frameworks}/device/CTailscale.framework" \
            "${frameworks}/simulator/CTailscale.framework" "${stage}/CTailscale.xcframework"

        # Notices: every non-main module linked into the device archive,
        # with its licence files verbatim; a module without one is fatal.
        local notices="${stage}/Notices"
        mkdir -p "${notices}"
        cp "${source}/LICENSE" "${notices}/libtailscale-BSD-3-Clause.txt"
        cp "${GOROOT}/LICENSE" "${notices}/Go-BSD-3-Clause.txt"
        local modules="${work}/tailscale-modules.txt"
        (
            cd "${source}"
            env CGO_ENABLED=1 GOOS=ios GOARCH=arm64 CC="${work}/cc-device.sh" \
                go list -deps -tags ios \
                    -f '{{with .Module}}{{if not .Main}}{{.Path}} {{.Version}} {{.Dir}}{{end}}{{end}}' . \
                | awk 'NF == 3' | LC_ALL=C sort -u > "${modules}"
        )
        local tailscale_dir
        tailscale_dir="$(awk '$1 == "tailscale.com" { print $3 }' "${modules}")"
        [[ -n "${tailscale_dir}" ]] || die "tailscale.com is not in the linked module graph"
        cp "${tailscale_dir}/LICENSE" "${notices}/Tailscale-BSD-3-Clause.txt"

        local module_notice="${notices}/Tailscale-Go-modules.txt"
        {
            echo "Third-party Go modules linked into CTailscale (libtailscale ${LIBTAILSCALE_COMMIT},"
            echo "tailscale.com ${TAILSCALE_MODULE_VERSION}, ${GO_VERSION}). Each module's licence"
            echo "files follow verbatim."
        } > "${module_notice}"
        local module_path module_version module_dir licence_files licence_file
        while read -r module_path module_version module_dir; do
            [[ "${module_path}" == "tailscale.com" ]] && continue
            licence_files="$(find "${module_dir}" -maxdepth 1 -type f \
                \( -iname 'LICENSE*' -o -iname 'LICENCE*' -o -iname 'COPYING*' -o -iname 'NOTICE*' -o -iname 'PATENTS' \) \
                | LC_ALL=C sort)"
            [[ -n "${licence_files}" ]] || die "Go module ${module_path}@${module_version} has no licence file"
            while read -r licence_file; do
                {
                    echo
                    echo "================================================================================"
                    echo "${module_path} ${module_version} — $(basename "${licence_file}")"
                    echo "================================================================================"
                    echo
                    cat "${licence_file}"
                } >> "${module_notice}"
            done <<<"${licence_files}"
        done < "${modules}"
        chmod 644 "${notices}"/*

        local module_count
        module_count="$(wc -l < "${modules}" | tr -d ' ')"
        cat > "${stage}/provenance.md" <<EOF
## CTailscale

- libtailscale: commit ${LIBTAILSCALE_COMMIT} (no release tag; submodule upstream/libtailscale, ${LIBTAILSCALE_REPO})
- libtailscale addition: ${TAILSCALE_GO_FILE} (sha256 $(sha256_of "${ROOT_DIR}/${TAILSCALE_GO_FILE}")) in package main; exports heeler_tailscale_disable_log_upload (envknob.SetNoLogsNoSupport + logtail.Disable), heeler_tailscale_log_upload_state, and heeler_tailscale_logout (LocalClient.Logout)
- tailscale.com: ${TAILSCALE_MODULE_VERSION} (libtailscale's go.mod and go.sum replaced by ${TAILSCALE_GO_MOD} sha256 ${LIBTAILSCALE_GO_MOD_SHA256} and ${TAILSCALE_GO_SUM} sha256 ${LIBTAILSCALE_GO_SUM_SHA256}; module hashes verified by that go.sum)
- Go toolchain: ${GO_VERSION}, darwin-arm64 archive sha256 ${GO_SHA256}
- Go experiment: none (GOEXPERIMENT unset)
- Go build: -buildmode=c-archive -trimpath -tags ios -ldflags "-s -w -buildid="
- Go modules linked: ${module_count}
EOF
    )
}
