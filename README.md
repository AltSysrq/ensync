Ensync
======

Introduction
------------

Ensync is an ENcrypted file SYNChroniser. That is, it synchronises files
between one or more clients via a central "server" (which might just be, e.g.,
a flash drive) location, but encrypts the data on the server such that it is
impossible to recover the data stored on the server without knowing the keys.

Features:

- Support for full bidirectional synchronisation.

- Flexible, hierarchical sync rules configuration.

- Optional transparent compression for file transfers and server-side storage.

- Transparent block deduplication. If two clients store the same file in the
  same storage location, the backing storage will be shared, even if the
  clients cannot read each others' data.

- Support for multiple passphrases / keys to access the server store.

- Support for separate key groups, to prevent some clients from reading/writing
  other clients' data.

Please note that Ensync's cryptographic properties have not been independently
verified.

Supported platforms:

- The author regularly uses Ensync on FreeBSD (AMD64), DragonFly BSD, and Linux
  (AMD64 and ARM7), so it is known to work well on those systems.

- In general, any POSIX-like system that Rust supports should be able to run
  Ensync.

- Windows is currently *not* supported. While not inherently impossible,
  Windows's filesystem quirks make support rather difficult to implement. There
  are currently no plans to implement such support, but contributions would be
  welcome.

### Contents

1. [Getting Started](#getting-started)
2. [About the Sync Process](#about-the-sync-process)
3. [Configuration Reference](#configuration-reference)
4. [Understanding the Sync Model](#understanding-the-sync-model)
5. [Advanced Sync Rules](#advanced-sync-rules)
6. [Key Management](#key-management)
7. [Using Key Groups](#using-key-groups)
8. [Security Considerations](#security-considerations)

Getting Started
---------------

The first thing to do is, of course, install Ensync. The easiest way to do this
right now is with cargo:

```shell
  cargo install ensync
```

If you are on DragonFly, you also need to pass `--no-default-features`.

If you will be syncing to a remote host, Ensync will also need to be installed
there.

After that, you need to set up configuration and if needed initialise the
remote storage. This can either be done automatically with `ensync setup`, or
by manually doing each step.

### Automatic Setup

The `ensync setup` command will write the configuration and perform all setup
steps automatically, as well as do some sanity checks. It is recommended if you
haven't used ensync before. Manual setup may be necessary for unusual setups.

The basic `ensync setup` invocation takes three arguments:

- The configuration location. This must name a directory to be created.
  Configuration files and sync state is placed within this directory.

- The local file location. This is a directory containing the files you want to
  sync.

- The remote storage location. This may be a directory on your local
  filesystem, or may be an scp-style `user@host:path`-format string. In the
  latter case, `ensync setup` will connect to the remote host with
  `ssh user@host`.

If you don't want your passphrase to be read from the terminal, you can pass
the `--key` parameter to `ensync setup`, which takes a string in the
[passphrase configuration](#passphrase-configuration) format. If you use a
`file` passphrase, the generated configuration will reference the absolute path
of that file. In the case of a `shell` passphrase, the configuration simply
holds the string you passed in. In both these cases, you may wish to move files
into the configuration directory after setup completes and edit the
configuration accordingly.

Once `ensync setup` completes successfully, you should be good to go. To
actually sync files, simply run:

```
  ensync sync /path/to/config
```

### Manual Setup

First, create a new directory to be the configuration directory. Within this
directory, create a file named `config.toml`. See the [Configuration
Reference](#configuration-reference) section for full details on what the
configuration means. A minimal example:

```toml
[general]
path = "/path/to/your/files"
server = "shell:ssh yourserver.example.com ensync server ensync-data"
server_root = "your-logical-root"
passphrase = "prompt"
compression = "best"

[[rules.root.files]]
mode = "cud/cud"
```

If this is the first time using that particular storage instance, you need to
initialise the key store:

```sh
  ensync key init /path/to/config
```

If your chosen `server_root` has not been created on the server yet, you also
need to take care of that now:

```sh
  ensync mkdir /path/to/config /your-logical-root
``

With that taken care of, you should be good to go, and can run

```sh
  ensync sync /path/to/config
```

to start syncing.

About the Sync Process
----------------------

### Overview

Ensync uses a three-phase sync process:

1. Search for changes.
2. Reconcile and apply changes.
3. Clean up.

In the first phase, Ensync scans every local directory it had previously marked
as clean to see if there may be any changes, and similarly tests whether
server-side directories marked as clean have been changed. This phase is only
used to determine what directories need to be processed.

In the "reconcile and apply changes" phase, Ensync recursively walks the client
and server directory trees, ignoring directories which are still clean
(including all subdirectories). Separate directories are processed in parallel.

For each examined directory, Ensync lists all files, matches them by name, and
then determines what change should be applied. When reconciliation is done, it
applies the selected change to each file in sequence. If all operations
succeed, the directory is marked clean.

In the cleanup phase, temporary and orphaned files are cleaned, and volatile
data committed.

### Efficient Rename Handling

Ensync is not aware of file rename operations; it simply sees them as a
deletion of one file followed by a creation of another file. However, Ensync
does take some steps to prevent needing to re-transfer file content in this
case.

On the server, blocks from deleted files linger until some client runs its
cleanup phase. This means that if a new file is created during the same sync
with the same content, it will be able to reuse those blocks instead of needing
to re-upload them.

Locally, deleted files are moved to a temporary location inside the
configuration/state directory and not actually deleted until the cleanup phase.
If the same content is encountered again, the data is copied out of the moved
file instead of being re-downloaded from the server. This functionality does
require, however, that the directory being synced is on the same filesystem as
the configuration directory. If it is not, deleted files are simply deleted
immediately, so renames will require re-downloading from the server in cases
where the deletion is seen first.

Keep in mind that due to these optimisations, disk usage will not reduce due to
deletions until Ensync completes, which could cause issues a sync
simultaneously creates and deletes a lot of very large files.

### Concurrency and Failure

Ensync will not behave incorrectly if any client files are modified while it is
running, insofar as that it will not corrupt the store on the server. However,
such files cannot be captured in an atomic state, and may therefore be less
than useful. For example, you should not try to use Ensync to replicate or back
up a live database.

Multiple Ensync instances cannot be run concurrently on the same configuration.
It is safe to run multiple Ensync instances with different configurations over
the same local directory tree, but be aware that they could see each other's
intermediate states and propagate them. It is safe to run any number of Ensync
instances against the same server store.

If the server process is killed gracelessly, it may leak temporary files but
will not corrupt the store.

If the client process dies before completion, some temporary files may be
leaked, and the filesystem may be left in an intermediate state, but no data
will be lost. In some cases, cached data may be lost, which will result in the
next sync being much slower than usual.

In case of power loss, all committed changes are expected to survive,
conditional on your operating system's `fsync` call actually syncing the data
and the underlying hardware behaving properly. One exception is the ancestor
state, which may in some cases be destroyed by unexpected termination of the
OS. If this happens, the next sync will simply be more conservative than
normal, which generally manifests in deletions being undone and additional
conflict files being created.

Errors that occur when processing a single file are generally logged without
interrupting the rest of the sync process.

### Filesystem Limitations

Only regular files, symlinks, and directories are supported. Other types of
files are not synced.

The basic read/write/execute permissions of regular files and directories are
synced, as well as the modified time of regular files. Other attributes and
alternate data streams are ignored.

Using Ensync with a case-insensitive filesystem (or, more generally, any
filesystem which considers one byte sequence to match a directory entry whose
name is a different byte sequence) is generally a bad idea, but is partially
supported so long as nothing causes two files on the server to exist which the
insensitive filesystem considers the same. If this happens, or if equivalent
but different names are created on the server and client, the result will
generally involve one version overwriting the other or the two being merged. No
guarantees are made here, nor is Ensync tested in these conditions.

Using Ensync with a filesystem which performs name normalisation (i.e., one
where trying to create a file whose name is one byte sequence results in
creating a file with a name which is a different byte sequence) is strongly
discouraged. These normalisations appear as a rename to Ensync and will be
propagated as such (i.e., as a deletion and a creation). In practise, there
won't be serious issues here as long as all participants use exactly the same
normalisation or if no names which would be changed by any normalisation are
ever created. If there are multiple participants using different normalisation,
the result will be rename fighting every time each participant syncs. Be aware
here that a particular fruit-flavoured OS not only has a normalising filesystem
by default, but uses a different normalisation than the rest of the world.

Ensync is not aware of hard links. Since it never overwrites files in-place,
having hard links will not cause issues, but Ensync may turn them into separate
files, and they will be created as separate files on other systems. Hard links
between directories, should your filesystem actually support them, are not
supported at all and will likely cause numerous issues.

If your system permits opening directories as regular files (eg, FreeBSD), you
may end up in a weird situation if something changes a regular file into a
directory at just the right moment. No data will be lost locally, but the raw
content of the directory may propagate as a regular file instead.

Configuration Reference
-----------------------

### At a Glance

The configuration is a [TOML](https://github.com/toml-lang/toml#toml) file
stored as `config.toml` under the configuration directory. It has two required
sections: `[general]` and `[rules]`. All file names are relative to the
configuration directory. The configuration looks like this:

```toml
[general]
# The path to the local files, i.e., the directory containing the cleartext
# files to be synced.
path = "/some/path"

# The server location, i.e., where the encrypted data is stored. The exact
# syntax for this described below.
server = "path:/another/path"

# The name of the logical root on the server to sync with. This is the name of
# a directory under the physical root of the server which is used as the top of
# the directory tree to be synced.
server_root = "the-root-name"

# How to get the passphrase to derive the encryption keys. The formats
# supported are described below.
passphrase = "prompt"

# What level of transparent file compression to use. Valid values are "none",
# "fast", "default", "best". This configuration can be omitted, in which case
# it defaults to "default".
compression = "default"

# Files uploaded to the server are split into blocks of this size. Identical
# blocks are only stored once on the server. A smaller block size may make this
# deduplication more effective, but will slow some things down. This can be
# omitted and will default to one megabyte. If you change it, take care that
# the block size actually corresponds to what you intend to deduplicate;
# generally, this should be a power of two since many things that have large
# random-write files align their writes to sector or page boundaries.
block_size = 1048576

# Specifies the sync rules. This is described in detail in the "Advanced Sync
# Rules" section. The example here is sufficient to apply one sync mode to
# all files.
[[rules.root.files]]
# The sync mode to use; that is, it describes how various changes are or are
# not propagated. This example is conservative full bidirectional sync. See
# "Understanding the Sync Model" for a full description of what this means.
mode = "cud/cud"
```

#### Server Configuration

The `server` configuration can take one of two forms.

`path:some-path` causes the "server" to be simply the path `some-path` on the
local filesystem. This is what is used to sync to a flash drive, for example.

`shell:some command` causes a server process `some command` to be spawned. The
client process communicates with the server via the child process's standard
input and output. The `ensync server` command is the server process this
normally communicates with. Because of this design, you can compose the command
with anything that forwards standard input and output, such as ssh, which is
the usual way of syncing to a remote host. For example, the configuration

```
server = "shell:ssh myserver.example.org ensync server sync-data
```

will cause the Ensync client to ssh into `myserver.example.org` and run `ensync
server sync-data`, which in turn causes the encrypted data to be stored in
`~/sync-data`.

### Passphrase Configuration

The `passphrase` configuration can take one of four forms.

`prompt` specifies to read the passphrase from the controlling terminal. This
is supported on most, but not all, platforms (DragonFly is the main exception).

`string:xxx` specifies to use `xxx` as the literal passphrase.

`file:some-file` specifies to read the content of `some-file` and use that as
the passphrase. Any trailing CR or LF characters are stripped from the input.

`shell:some command` specifies to pass `some command` to the shell, and use the
standard output of the command as the passphrase. As with `file`, trailing CR
and LF characters are stripped.

Understanding the Sync Model
----------------------------

### Introduction

Ensync sync model is to essentially perform a 3-way merge on the contents of
each directory.

The way this works may be clearer if we start by thinking about how syncing
might work as a _2-way_ merge of the contents of a directory. A 2-way directory
merge means to list the contents of each corresponding directory, then match
files on each replica together based on their name. Then, determine what needs
to change for each file or pair of files. For example, we might visit a
directory and build a table like this:

File name       | Client content        | Server content
----------------|-----------------------|-----------------------
hello.txt       | hello world           | hello world
password.txt    | hunter2               | hunter2

This directory is clearly in-sync; the client and server agree on everything.
Now let's edit one of the files.

File name       | Client content        | Server content
----------------|-----------------------|-----------------------
hello.txt       | hallo welt            | hello world
password.txt    | hunter2               | hunter2

When our 2-way merger runs, it sees that the two replicas disagree on the
content of `hello.txt`, so clearly one of them must be edited to match the
other. But there isn't a clear way to choose. One option is to use the one with
the later modified time; in this case, it would be the client, which does what
we want.

This approach is called "Last Write Wins", and does work for many cases, but
also has a lot of problems. For example, if you restore some files from backup
and the backup tool restores their modified time as well, our 2-way merger
would immediately undo the restoration since the server-side files would be
newer.

But 2-way merge completely breaks once we bring file creation and deletion into
the picture. Let's see what happens if we delete "password.txt" on the client
and someone else creates a new file on the server:

File name       | Client content        | Server content
----------------|-----------------------|-----------------------
hello.txt       | hello world           | hello world
memo.txt        | _(none)_              | some text here
password.txt    | _(none)_              | hunter2

Notice that for the two files in question, the table looks _exactly the same_.
No logic on this state can handle both the creation and the deletion correctly.
And we don't have any timestamps to work with this time.

What Ensync does is add a _third_ replica, the "ancestor" replica, which stores
the last state for each file that was in-sync for the client and server
replicas. This ancestor replica is used to determine which end replica(s) have
actually changed. Thus, our "everything in-sync" table actually looks like

File name       | Client content        | Ancestor content      | Server content
----------------|-----------------------|-----------------------|---------------
hello.txt       | hello world           | hello world           | hello world
password.txt    | hunter2               | hunter2               | hunter2

And the create-and-delete table actually looks like

File name       | Client content        | Ancestor content      | Server content
----------------|-----------------------|-----------------------|---------------
hello.txt       | hello world           | hello world           | hello world
memo.txt        | _(none)_              | _(none)_              | some text here
password.txt    | _(none)_              | hunter2               | hunter2

Now, the cases for the creation and deletion are different. We can clearly see
that "password.txt" once did exist on both end replicas (since it _is_ in the
ancestor replica) but is now gone from the client and was thus deleted, as well
as that "memo.txt" was never seen before and thus must be a creation.

Note that for simplicity we notate the ancestor replica as having particular
content, but in reality it only stores content hashes and no actual file
content.

### Sync Mode

The ancestor replica allows Ensync to determine what _has_ changed. The
configured _sync mode_ tells Ensync what _should be_ changed in response.

The sync is normally specified as a 7-character string formatted like
`cud/cud`. Each letter is a flag; a lowercase letter indicates "on", an
uppercase letter indicates "force", and replacing the letter with a hyphen
means "off". Below shows the name and meaning of each flag.

```
        ┌─────── "Sync inbound create"
        │        New remote files are downloaded to the local filesystem
        │┌────── "Sync inbound update"
        ││       Edits to remote files are applied to local files
        ││┌───── "Sync inbound delete"
        │││      Files deleted remotely are deleted in the local filesystem
        cud/cud
            │││
            ││└─ "Sync outbound delete"
            ││   Files deleted in the local filesystem are deleted remotely
            │└── "Sync outbound update"
            │    Edits to local files are applied to remote files
            └─── "Sync outbound create"
                 New local files are uploaded to remote storage
```

More specifically, each flag applies to changes that may be made to that
replica under any condition, so they can also be viewed as giving Ensync
"permission" to perform that type of operation. For example, if "sync inbound
create" is off, Ensync will never create any new files locally.

Setting a flag to "force" will cause Ensync to perform that operation if
necessary to bring the replicas in-sync, even if this could result in losing
data:

- "Force create" will cause a deleted file to be recreated if the
  opposite-bound delete setting is off.

- "Force delete" will cause a new file to be deleted if the opposite-bound
  create setting is off. It will also cause edit-delete conflicts to be
  resolved in favour of delete when the opposite-bound create setting is off.

- "Force update" will cause updates to be reverted if the opposite update
  setting is off. It will also cause edit-edit conflicts to be resolved in
  favour of the side _without_ "force update". If _both_ sides have "force
  update", edit-edit conflicts are automatically resolved in favour of the
  version with the newer modified time, or the client if tied.

The most useful sync modes also have aliases:

- "mirror" is an alias for "---/CUD" (all outbound set to "force", all inbound
  set to "off"). This causes the server replica to be modified to exactly match
  the client side, without making any modifications to the client side at all.

- "conservative-sync" is an alias for "cud/cud", i.e., conservative
  bidirectional sync.

- "aggressive-sync" is an alias for "CUD/CUD", i.e., bidirectional sync with
  automatic resolution of all conflicts.

### Non-Conflicting States

The below table shows what actions are taken for various (client, ancestor,
server) states and sync modes. A `*` in the sync mode or state indicates
"anything"; uppercase letters in the state indicate content (i.e., `(A,A,A)`
indicates client, ancestor, and server have identical file content;
`(A,∅,B)` indicates client and server have different file content and the
ancestor has no content). Conflicts are shown here, but discussed in the next
section.

State           | Sync mode     | Action
----------------|---------------|---------
`(∅,*,∅)`       | `***/***`     | None
`(∅,∅,A)`       | `c**/***`     | Create file on client
`(∅,∅,A)`       | `-**/**D`     | Delete file on server
`(∅,∅,A)`       | (otherwise)   | Leave out-of-sync
`(∅,A,A)`       | `***/**d`     | Delete file on server
`(∅,A,A)`       | `C**/**-`     | Recreate file on client
`(∅,A,A)`       | (otherwise)   | Leave out-of-sync
`(∅,A,B)`       | `***/***`     | _Edit-delete conflict_
`(A,∅,∅)`       | `***/c**`     | Create file on server
`(A,∅,∅)`       | `**D/-**`     | Delete file on client
`(A,∅,∅)`       | (otherwise)   | Leave out-of-sync
`(A,A,∅)`       | `**d/***`     | Delete file on client
`(A,A,∅)`       | `**-/C**`     | Recreate file on server
`(A,B,∅)`       | `***/***`     | _Edit-delete conflict_
`(A,*,A)`       | `***/***`     | None
`(A,A,B)`       | `*u*/***`     | Update client to B
`(A,A,B)`       | `*-*/*U*`     | Revert server to A
`(A,A,B)`       | (otherwise)   | Leave out-of-sync
`(A,B,B)`       | `***/*u*`     | Update server to A
`(A,B,B)`       | `*U*/*-*`     | Revert client to B
`(A,B,B)`       | (otherwise)   | Leave out-of-sync
`(A,∅,C)`       | `***/***`     | _Edit-edit conflict_
`(A,B,C)`       | `***/***`     | _Edit-edit conflict_

### Conflicting States

A conflict occurs when the client and server have both changed to different
states since the last sync. Ensync generally tries to handle conflicts as
conservatively as possible, but this can be controlled by the sync mode.

An edit-delete conflict occurs when one replica has modified a file since the
last sync, and the other has deleted it. Ensync resolves an edit-delete
conflict by taking the first choice below according to the sync mode:

- If create is enabled on the side that deleted the file, recreate the file
  with the new content, since dealing with an unwanted extra file is easier
  than recovering data that was deleted.

- If delete is set to "force" on the side that edited the file, delete it from
  that side.

- Otherwise, leave the file out-of-sync.

An edit-edit conflict occurs when both the client and server replicas have
changed the state of a file since the last sync, and those states are
different. If the replicas each create a file with different content, it is
also considered an edit-edit conflict. Edit-edit conflicts are handled by
taking the first choice below according to the sync mode:

- If both update settings are "force" and the file has a modified time on both
  replicas, use the version with the later modified time.

- If one update setting is "force", use the version from the opposite replica.

- If neither update setting is "on", leave the files out-of-sync.

- If _both_ create settings are at least "on", rename the conflicting file on
  the server (e.g., `foo.txt` → `foo~1.txt`) and then propagate both versions
  as creates to each opposing side.

- Otherwise, leave the file out-of-sync.

For "less important" file properties, like mode or timestamp, Ensync always
resolves conflicts implicitly and will not rename or create new files.
Additionally, these fields are always carried from whatever version was
propagated. For example, if you `chmod` a file locally, but the actual content
is updated on the server, and both "update" settings are on, Ensync will update
the file to have the content and mode from the server, essentially discarding
the effect of the `chmod`.

### Directory Weirdness

The fact that directories contain more files that are themselves subject to
syncing complicates the model presented above. Without going into too much
detail:

- If the above rules would delete a directory, Ensync instead recurses into it
  and syncs it normally, treating the replica as already deleted as having
  empty directories. If all files within the replica that does still have the
  directory do in fact get deleted, then the directory itself is deleted.
  However, if new files were introduced there, the directory path leading there
  will instead be [re]created on the other replica.

- If a directory is edited into a non-directory, Ensync will rename the
  directory on the replica that still holds it and then proceed according to
  the recursive delete case above.

Advanced Sync Rules
-------------------

TODO Review this section, provide examples

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
mode = "---/---"
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

Key Management
--------------

TODO

Using Key Groups
----------------

TODO

Security Considerations
-----------------------

TODO

License
-------

[GLPv3 or later](COPYING)
