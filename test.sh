#!/usr/bin/env bash

set -euxo pipefail

cargo build --release
ENSYNC="$(cargo metadata --format-version 1 | jq -r '.target_directory')/release/ensync"

TEST_DIR="$(mktemp --directory --tmpdir ensync-test.XXXXXXXXX)"
cd "${TEST_DIR}"
trap "rm -rf '${TEST_DIR}'" exit

LOCAL="${PWD}/local"
CONFIG="${PWD}/config"
REMOTE="${PWD}/remote"
TEST_FILES="${PWD}/test-files"

mkdir "${LOCAL}" "${CONFIG}" "${TEST_FILES}"

cat > "${CONFIG}/config.toml" <<EOF
[general]
path = "${LOCAL}"
server = "path:${REMOTE}"
server_root = "root"
passphrase = "string:test"
compression = "best"

[[rules.root.files]]
mode = "conservative-sync"
trust_client_unix_mode = true
EOF

function randfile {
	local len="$1"
	local out="$(mktemp "${TEST_FILES}"/$len-XXXXXXXXXX)"
	openssl rand $len > "${out}"
	echo "${out}"
}

${ENSYNC} key init "${CONFIG}"
${ENSYNC} mkdir "${CONFIG}" /root
${ENSYNC} mkdir "${CONFIG}" remote-new-dir
${ENSYNC} mkdir "${CONFIG}" remote-dir-to-remove
${ENSYNC} put "${CONFIG}" "$(randfile 1)" remote-file-to-update
${ENSYNC} put "${CONFIG}" "$(randfile 2)" remote-file-to-remove
${ENSYNC} put "${CONFIG}" "$(randfile 3)" edit-edit-file
mkdir "${LOCAL}/local-new-dir"
cp "$(randfile 4)" "${LOCAL}/local-file-to-update"
cp "$(randfile 5)" "${LOCAL}/local-file-to-remove"
cp "$(randfile 6)" "${LOCAL}/edit-edit-file"
ln -s target "${LOCAL}/local-symlink"
mkdir "${LOCAL}/local-dir-to-remove"
mkdir "${LOCAL}/dir-to-remove-recursively"
cp "$(randfile 9)" "${LOCAL}/dir-to-remove-recursively/testfile"

JSON_STATUS_OUT="${PWD}/json-status-out"

exec 3>"${JSON_STATUS_OUT}"
${ENSYNC} sync --json-status-fd=3 "${CONFIG}"

sleep 1.5

cp "$(randfile 7)" "${LOCAL}/local-file-to-update"
rm "${LOCAL}/local-file-to-remove"
rmdir "${LOCAL}/local-dir-to-remove"
rm -r "${LOCAL}/dir-to-remove-recursively"
${ENSYNC} put --force "${CONFIG}" "$(randfile 8)" remote-file-to-update
${ENSYNC} rm "${CONFIG}" remote-file-to-remove
${ENSYNC} rmdir "${CONFIG}" remote-dir-to-remove
${ENSYNC} sync --json-status-fd=3 "${CONFIG}"

cat "${JSON_STATUS_OUT}"
