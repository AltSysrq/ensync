# Unreleased

- Fix `ensync setup` falsely identifying UNIX file systems as not supporting
  UNIX permissions.

- All commands which operate on the server now take a `-k`/`--key` argument
  that can override the configuration's passphrase.

- `ensync sync` now supports a `--quiescence` argument which can adjust the
  debouncing period for local FS events in conjunction with `--watch`.

- `ensync sync` now detects if the internal state was improperly copied from
  another machine.

# 0.2.5

- Add `reset-server` and `reset-client` sync mode aliases.

- Add `--mode-override` flag to `ensync sync` to force syncing to proceed with
  a particular sync mode instead of the configured one.

# 0.2.4

- Added `--reconnect` flag which can be used in conjunction with `--watch` to
  automatically reconnect if errors occur.
  https://github.com/altsysrq/ensync/issues/3

# 0.2.3

- Fix handling of `ensync key ls`.

# 0.2.2

- `ensync key list` is now `ensync key ls`. The old command name is
  still available as an alias.

- When used with `--watch`, if the connection to the server is lost,
  ensync now wakes up to respond to the event.

# 0.2.1

- It is now possible to use sync rules to not propagate the UNIX mode of
  files in the local filesystem to the remote.
  https://github.com/AltSysrq/ensync/issues/2

# 0.2.0

- Introduce file change monitoring (inotify, etc) support, activated via
  `ensync sync --watch`.

- Passing `-q` to `ensync sync` now actually silences all uninteresting
  messages.

- Fix a case where scanning a directory full of very large files would prevent
  other parts of the sync process from completing until that scan completed.

# 0.1.3

- Fix panic if `ensync key` or `ensync key group` were run without the needed
  subcommand.
