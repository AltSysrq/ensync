#!/usr/bin/env bash

set -euo pipefail

for prog in cargo jq openssl; do
	if ! [[ -x "$(which $prog)" ]]; then
		echo "Dependency cannot be found: $prog" >&2
		exit 1
	fi
done

cargo build --quiet --release
ENSYNC="$(cargo metadata --format-version 1 | jq -r '.target_directory')/release/ensync"

TEST_DIR="$(mktemp --directory --tmpdir ensync-test.XXXXXXXXX)"
cd "${TEST_DIR}"
trap "chmod -R 777 '${TEST_DIR}' && rm -rf '${TEST_DIR}'" exit

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

function remove_times_and_sort() {
	local sort_filter='sort_by([.path, .side, .type])'

	if [[ -n "${JSON_NO_SORT:-}" ]]; then
		sort_filter='.'
	fi

	jq -sS '
		def blur_if_exists(path): if path? then path = "..." else . end;
		map(
			blur_if_exists(.time) |
			blur_if_exists(.update_old_info.modified_time) |
			blur_if_exists(.update_new_info.modified_time) |
			blur_if_exists(.info.modified_time)
		) |
		'"${sort_filter}"' |
		.[]
	'
}

${ENSYNC} key init "${CONFIG}"
${ENSYNC} mkdir "${CONFIG}" /root
${ENSYNC} mkdir "${CONFIG}" remote-new-dir
${ENSYNC} mkdir "${CONFIG}" remote-dir-to-remove
${ENSYNC} put "${CONFIG}" "$(randfile 1)" remote-file-to-update
${ENSYNC} put "${CONFIG}" "$(randfile 2)" remote-file-to-remove
${ENSYNC} put "${CONFIG}" "$(randfile 3)" edit-edit-file
${ENSYNC} mkdir "${CONFIG}" dir-for-read-errors
${ENSYNC} mkdir "${CONFIG}" dir-for-write-errors
${ENSYNC} put "${CONFIG}" "$(randfile 10)" dir-for-write-errors/file-to-attempt-removal
mkdir "${LOCAL}/local-new-dir"
cp "$(randfile 4)" "${LOCAL}/local-file-to-update"
cp "$(randfile 5)" "${LOCAL}/local-file-to-remove"
cp "$(randfile 6)" "${LOCAL}/edit-edit-file"
ln -s target "${LOCAL}/local-symlink"
mkdir "${LOCAL}/local-dir-to-remove"
mkdir "${LOCAL}/dir-to-remove-recursively"
cp "$(randfile 9)" "${LOCAL}/dir-to-remove-recursively/testfile"

JSON_STATUS_OUT="${PWD}/json-status-out"

exec 3>"${JSON_STATUS_OUT}-1"
${ENSYNC} sync --json-status-fd=3 "${CONFIG}" >/dev/null 2>&1

sleep 1.5

cp "$(randfile 7)" "${LOCAL}/local-file-to-update"
rm "${LOCAL}/local-file-to-remove"
rmdir "${LOCAL}/local-dir-to-remove"
rm -r "${LOCAL}/dir-to-remove-recursively"
chmod 000 "${LOCAL}/dir-for-read-errors"
chmod 500 "${LOCAL}/dir-for-write-errors"
${ENSYNC} put --force "${CONFIG}" "$(randfile 8)" remote-file-to-update
${ENSYNC} rm "${CONFIG}" remote-file-to-remove
${ENSYNC} rmdir "${CONFIG}" remote-dir-to-remove
${ENSYNC} rm -r "${CONFIG}" dir-for-write-errors

exec 3>"${JSON_STATUS_OUT}-2"
${ENSYNC} sync --json-status-fd=3 "${CONFIG}" #>/dev/null 2>&1

(
	remove_times_and_sort < "${JSON_STATUS_OUT}-1"
	echo
	remove_times_and_sort < "${JSON_STATUS_OUT}-2"
)
