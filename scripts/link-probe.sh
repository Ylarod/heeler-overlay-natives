#!/bin/bash
# Links the three binary targets of the development manifest (build/Artifacts)
# into Tests/LinkProbe and runs its tests on an iOS Simulator.
#
#   scripts/link-probe.sh                  # first booted or available iPhone
#   HEELER_NATIVES_TEST_DESTINATION='id=<udid>' scripts/link-probe.sh
set -euo pipefail

# shellcheck source=lib/common.sh
source "$(dirname "${BASH_SOURCE[0]}")/lib/common.sh"

for framework in "${FRAMEWORKS[@]}"; do
    [[ -d "${BUILD_DIR}/Artifacts/${framework}.xcframework" ]] \
        || die "build/Artifacts/${framework}.xcframework is missing; run scripts/build.sh"
done
[[ "${BUILD_DIR}" == "${ROOT_DIR}/build" ]] \
    || die "the development manifest reads build/Artifacts; unset HEELER_NATIVES_BUILD_DIR"

destination="${HEELER_NATIVES_TEST_DESTINATION:-}"
if [[ -z "${destination}" ]]; then
    udid="$(xcrun simctl list devices available -j | python3 -c '
import json, sys
devices = [d for runtime, ds in json.load(sys.stdin)["devices"].items() if "iOS" in runtime for d in ds]
iphones = [d for d in devices if d["name"].startswith("iPhone")]
booted = [d for d in iphones if d["state"] == "Booted"]
print((booted or iphones)[0]["udid"] if iphones else "")
')"
    [[ -n "${udid}" ]] || die "no iPhone Simulator is available"
    destination="id=${udid}"
fi

cd "${ROOT_DIR}/Tests/LinkProbe"
xcodebuild test -scheme LinkProbe-Package -destination "${destination}" \
    -derivedDataPath "${BUILD_DIR}/LinkProbe-DerivedData" -quiet
echo "Link probe passed on ${destination}."
