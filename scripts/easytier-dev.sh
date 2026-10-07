#!/bin/bash
# Prepares native/easytier for local cargo work: writes EasyTier (submodule
# commit from sources.lock, with patches/easytier applied) into
# native/easytier/vendor/easytier, where the crate's path dependencies expect
# it. Re-run after changing the submodule or a patch.
#
#   scripts/easytier-dev.sh
#   cd native/easytier && cargo test --locked
set -euo pipefail

# shellcheck source=lib/common.sh
source "$(dirname "${BASH_SOURCE[0]}")/lib/common.sh"
# shellcheck source=lib/easytier.sh
source "${ROOT_DIR}/scripts/lib/easytier.sh"

require_commands git tar patch rsync shasum
check_submodules
sync_easytier_source "${ROOT_DIR}/native/easytier/vendor/easytier"
echo "native/easytier/vendor/easytier holds EasyTier ${EASYTIER_COMMIT} with patches/easytier applied."
