#!/usr/bin/env bash

set -euo pipefail

if [[ -z "$1" ]]; then
	echo "Usage: $0 OLDFILE" >&2
	exit 1
fi

OUTPUT_DIR="$(mktemp --directory --tmpdir ensync-compare.XXXXXXXXX)"
trap "rm -rf '${OUTPUT_DIR}'" exit

"$(dirname $0)"/generate-json-output.sh > "${OUTPUT_DIR}/current-output.json"

diff ${DIFFOPTS:--u} "$1" "${OUTPUT_DIR}/current-output.json"
