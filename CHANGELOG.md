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
