Ensync
======

Introduction
------------

Ensync is a file synchronisation utility operating in a star topology, where
all files on the central hub are encrypted and signed via GPG. This makes it
suitable for use cases where the central hub is not considered completely
secure, eg, due to unavailability of disk encryption or because it is
controlled by a third party.

Emphasis is placed on simplicity and robustness rather than having lots of
features or being particularly fast.

Security Notes
--------------

Ensync is specifically designed to prevent the discovery of novel byte
sequences via compromise of the central hub. Attackers can still determine the
size of each file trivially; additionally, unless the `pepper` option is used,
it is possible for an attacker to determine whether you have a particular exact
file. Even then, you probably want to take extra measures if you're worried
about this type of identification.

Sync Model
----------

TODO

Operational Model
-----------------

Ensync uses a simple client-server model, where the client operates on the
cleartext files and makes most decisions, and the server provides a dumb blob
store. Typically the server is invoked over SSH, but it can also be run locally
to, eg, sync with a removable device.

### Server Side

The server, for the most part, knows nothing about the sync model. Its main
functionality is storing a mapping from client-supplied 256-bit identifiers to
data payloads. In practise, the identifiers are SHA-3 sums of the cleartext
with the pepper prepended and appended, though the server has no way to verify
this. The reference counts are also maintained via commands from the client,
since the server cannot identify references itself.

The server also maintains a table of cleartext identifiers to blob identifiers,
which serve as the roots of the file trees and the targets of directory
entries. (Note however that it is not possible to use these pointers alone to
reconstruct the directory structure.)

Sync Rules
----------

The sync rules are defined in the configuration within the `rules` table. The
rules are divided into one or more _states_; the initial state is called
`root`. The initial current sync mode is "---/---", i.e., "do nothing".

Each state has two rule-groups at which rules are matched, termed `files` and
`siblings`. These are sub-tables under the state; so, for example, the minimal
rules consist of `rules.root.files` and `rules.root.siblings` table arrays.

The tables within each rule-group share a similar structure. Each rule consists
of any number of distinct conditions, and has one or more actions. (Zero
actions is permitted for consistency, but is not useful on its own.) Each rule
is evaluated in the order defined and any actions processed.

### The `files` rule-group

The _files_ rule-group is used to match individual files (as in directory
entries). Generally, most configuration is done here, though it is not possible
to, e.g., match git repositories with it.

Each file in a directory is evaluated against the conditions in isolation. Any
matching actions are applied to that file and that file alone. For files which
are directories, general state (e.g., the current rules state, and the current
sync mode) are kept for the directory's contents.

Each file name in a directory is evaluated against the root set exactly once.
If the file exists client-side, the data there is used for matching purposes;
otherwise, the server-side file version is used instead.

### The `siblings` rule-group

The _siblings_ rule-group is used to affect the full contents of a directory
based on other parts of its contents. It is specifically designed to be able to
exclude git repositories, but may have other uses.

When a directory is entered, every file in the directory is examined and tested
against all conditions of every rule, keeping a set of which rules have
matched. Once all files have been examined, each matched rule is applied in
sequence.

It is important to keep in mind that this applies to the contents of the
directory as a whole; certain rule combinations can be matched in ways that are
impossible for a single file. For example, in the following configuration,

```
[[rules.root.siblings]]
name = '^a$'
mode = "cud/cud"

[[rules.root.siblings]]
name = '^b$'
stop = "all"

[[rules.root.siblings]]
name = '^a$'
mode = "---/---
```

the contents of a directory containing both files named `a` and `b` will apply
sync mode `cud/cud` to its contents, since all three rules match, but the
second blocks processing of the third.

Note that in order for a directory to be considered for processing _at all_,
the sync mode on the containing directory has to permit that directory to come
into existence in the first place; `siblings` is only evaluated once the
containing directory has been entered normally.

Unlike the `files` rule-group, every version of every file on the client and
server is tested for rules matching. This can result in surprising effects if
the two disagree. For example, in the following configuration, *both* rules
will apply if the file mode on the client was changed from 0666 to 0777 since
the last sync.

```
[[rules.root.siblings]]
permissions = "0666"
include = "other-state-A"

[[rules.root.siblings]]
permissions = "0777"
include = "other-state-B"
```

Because of this, care must be used when constructing sync rules which depend on
the contents of files.

### Conditions

Each condition is a string-value TOML pair. Note that this means you cannot use
two conditions in the same rule that happen to share the same name; this
shouldn't be an issue since most conditions take regexes.

All conditions which match a string interpret the configuration as a regular
expression. The target string is coerced to UTF-8, with invalid sequences
replaced with substitution characters. Regular expressions are not anchored; if
this is desired, `^` and `$` must be used explicitly.

Should Ensync be ported to a platform that does not conventionally use `/` as
the path delimiter, the rules engine will still use `/`, both for simplicity
and to keep the expressions readable.

#### `name`

Matches files whose base name matches the given expression. E.g., in the path
`/foo/bar/baz.txt`, `baz.txt` is the basename.

#### `path`

Matches files whose path _relative to the sync root_ matches the given
expression. E.g., if syncing `/foo/bar`, the file `/foo/bar/plugh/xyzzy` is
tested with `plugh/xyzzy`.

#### `permissions`

Matches files whose mode matches the given expression. Before matching, the
mode is converted to a 4-digit octal string left-padded with zeroes.

#### `type`

Matches files of the given physical type. The target string will be one of `f`
for regular files, `d` for directories, or `s` for symlinks. There is no way to
match against other types of files, as they are hardwired to have `---/---`
mode.

#### `target`

Matches symlinks whose target matches the given expression. Files which are not
symlinks do not match.

#### `bigger`

Matches regular files whose size is greater than the given number of bytes.
Non-regular files do not match.

#### `smaller`

Matches regular files whose size is smaller than the given number of bytes.
Non-regular files do not match.

### Actions

Each action is a string-value TOML pair. Actions within a rule are evaluated in
the order listed below.

#### `mode`

Sets the current sync mode to the given value. This completely replaces the
current sync mode.

#### `include`

The value is either a string or an array of strings. Each string identifies a
different rules state. Rule processing recurses into each listed rule state and
processes all rules from the current rule-group for the current file. If
processing was not stopped, it resumes on this state as normal. If a listed
state is already being evaluated when the `include` is evaluated, it is
ignored.

#### `switch`

The value is a string identifying a rules state. Sets the current "switch
state". When the current processing is complete, the rules state switches to
that state and the switch state is cleared. Essentially, this controls the
state used for files within a directory without affecting the directory itself.

#### `stop`

The value must be either `return` or `all`. If `return`, processing of the
current rules state stops; if this was reached via `include`, processing
continues in the superordinate state. If `all`, all processing stops, including
superordinate states that got here via `include`.

### Examples

The following is a simple example which bidirectionally syncs most things, but
excludes git and hg repositories and all backup files.

```
[[rules.root.files]]
mode = "cud/cud"

[[rules.root.files]]
name = "~$"
mode = "---/---"

[[rules.root.siblings]]
name = '^\.git$'
switch = "git"

[[rules.git.files]]
mode = "---/---"
```

Below is a possible convention for making certain files specific to each
machine:

```
[[rules.root.files]]
target = '/^\.![^/]*$'
mode = "---/---"

[[rules.root.files]]
name = '^\.!'
mode = "cud/cud"
switch = "private"

[[rules.private.files]]
mode = "cud/cud"
```

Concurrency and Failure
-----------------------

Ensync will not behave incorrectly if any client files are modified while it is
running, insofar as that it will not corrupt the store on the server. However,
such files cannot be captured in an atomic state, and may therefore be less
than useful.

Multiple Ensync instances should not be run concurrently over the same
directory tree. A best effort is made to detect this condition and abort when
it happens. Ensync does not permit multiple instances to run over the same
server store at once.

If the server process is killed gracelessly, it may leak temporary files but
will not corrupt the store.

If the client process dies before completion, some temporary files may be
leaked, and some sync changes will have been applied to the local filesystem,
but no changes at all will have occurred server-side or in the ancestor store.
No non-temporary files will exist in an intermediate state. This means that
restarting a failed sync behaves the way one would expect, since the only
changes occurring on the client side were to make it look like the server side.
However, in some cases additional edit conflict files may be created.

Errors that occur when processing a single file are generally logged without
interrupting the rest of the sync process. Ancestor state for failed files is
left unchanged.

Filesystem Limitations
----------------------

Only regular files, symlinks, and directories are supported. Other types of
files are not synced.

The basic read/write/execute permissions of regular files and directories are
synced. Other attributes and alternate data streams are ignored.

Using Ensync with a case-insensitive filesystem (or, more generally, any
filesystem which considers one byte sequence to match a directory entry whose
name is a different byte sequence) is generally a bad idea, but is partially
supported so long as nothing causes two files on the server to exist which the
insensitive filesystem considers the same. If this happens, or if equivalent
but different names are created on the server and client, the result will
generally involve one version overwriting the other or the two being merged. No
guarantees are made here, nor is Ensync tested in these conditions.

Ensync is not aware of hard links. Since it never overwrites files in-place,
having hard links will not cause issues, but Ensync may turn them into separate
files, and they will be created as separate files on other systems. Hard links
between directories, should your filesystem actually support them, are not
supported at all and will likely cause numerous issues.

If your system permits opening directories as regular files (eg, FreeBSD), you
may end up in a weird situation if something changes a regular file into a
directory at just the right moment. No data will be lost locally, but the raw
content of the directory may propagate as a regular file instead.

Building from Source
--------------------

Install Rust 1.10 and Cargo 0.9.0 or later (https://www.rust-lang.org/).

Install `gpgme` and sqlite, eg, `pkg install gpgme sqlite3` as root.
