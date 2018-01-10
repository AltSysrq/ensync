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

Why use Ensync?

- You want to sync files between multiple systems in a star model (i.e., one
  central repository to which all systems sync), but encryption is unavailable
  or problematic on the central point. For example, the central point may be a
  server that needs to be able to boot without human intervention, or a hosted
  server.

- You want to sync files between multiple systems and want something more
  flexible or transparent than otherwise comparable utilities like Unison.

- You want to back up multiple systems to the same storage and deduplicate
  shared data without being able to escalate from read access to one of those
  systems to read access to all of them. (For this use alone, it may also be
  worth looking at [Tarsnap](https://www.tarsnap.com/).

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

### Status

Beta. The application itself has proven fairly stable in testing. The protocol
between the client and server (when using a separate-process server) may still
be subject to incompatible change. The on-disk format will definitely not be
changed in a backwards-incompatible way.

### Contents

1. [Getting Started](#getting-started)
2. [About the Sync Process](#about-the-sync-process)
3. [Configuration Reference](#configuration-reference)
4. [Understanding the Sync Model](#understanding-the-sync-model)
5. [Advanced Sync Rules](#advanced-sync-rules)
6. [Key Management](#key-management)
7. [Using Key Groups](#using-key-groups)
8. [Ensync Shell](#ensync-shell)
9. [Security Considerations](#security-considerations)

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
```

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

- `mirror` is an alias for `---/CUD` (all outbound set to "force", all inbound
  set to "off"). This causes the server replica to be modified to exactly match
  the client side, without making any modifications to the client side at all.

- `conservative-sync` is an alias for `cud/cud`, i.e., conservative
  bidirectional sync.

- `aggressive-sync` is an alias for `CUD/CUD`, i.e., bidirectional sync with
  automatic resolution of all conflicts.

### Other Sync Flags

There are other less commonly-used flags which adjust the reconciliation
process. For more details, see the documentation for each flag.

- [`trust_client_unix_mode`](#trust_client_unix_mode)

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

The sync rules are defined in the configuration within the `rules` table. The
rules are divided into one or more _states_; the initial state is called
`root`. The initial current sync mode is `---/---`, i.e., "do nothing".

Each state has two rule-groups at which rules are matched, termed `files` and
`siblings`. These are sub-tables under the state; so, for example, the minimal
rules consist of `[[rules.root.files]]` and `[[rules.root.siblings]]` table
arrays.

The tables within each rule-group share a similar structure. Each rule consists
of any number of distinct conditions, and has one or more actions. (Zero
actions is permitted for consistency, but is not useful on its own.) Each rule
is evaluated in the order defined and any actions processed.

### Examples

Before going into detail, some examples will hopefully make the syntax of all
this clearer. First, the minimal configuration seen earlier in the
documentation:

```toml
[[rules.root.files]]
mode = "cud/cud"
```

This defines a single rule in the `root` state. It applies to all files since
it has no conditions, and sets the sync mode for every file to `cud/cud`.

Let's consider a more complex example:

```toml
[[rules.root.files]]
mode = "cud/cud"

[[rules.root.files]]
name = '~$'
mode = "---/---"

[[rules.root.siblings]]
name = '^\.git$'
switch = "git"

[[rules.git.files]]
mode = "---/---"
```

This configuration defines two rules in the `files` rule-group of the `root`
state, one rule in the `siblings` rule-group of the `root` state, and one rule
in the `files` rule-group of the `git` state. It can be read like the
following:

- Unless otherwise noted, use full symmetric sync for files. (First rule: no
  conditions, set mode to `cud/cud`.)

- Don't sync files ending in `~` at all. (Second rule: files whose names match
  `~$`, set mode to `---/---`.)

- If entering a directory containing a `.git` file, consult the `git` state
  instead. (Third rule: sibling matching `^\.git$`, switch to `git`.)

- Don't sync the contents of git repositories at all. (Fourth rule: in `git`
  state, no condition, set mode to `---/---`.)

### The `files` rule-group

The _files_ rule-group is used to match individual files (as in directory
entries). Generally, most configuration is done here, though it is not possible
to, e.g., match git repositories with it.

Each file in a directory is evaluated against the conditions in isolation. Any
matching actions are applied to that file and that file alone. For files which
are directories, general state (e.g., the current rules state, and the current
sync mode) are kept for the directory's contents.

Each file name in a directory is evaluated against the rule set exactly once.
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

#### `trust_client_unix_mode`

Either `true` or `false` (without quotes). Sets whether the reconciliation
process trusts the UNIX mode for files on the local filesystem.

If `false`, the reconciliation process will ignore the actual UNIX mode on
files which are also present on the remote replica, substituting it with the
mode of the corresponding file on the remote replica. This prevents propagation
of mode changes in either direction, which is useful if one client is syncing
to a local filesystem which does not store (e.g., FAT32) or return (e.g.,
`noexec` mount option) UNIX modes.

The default is `true`, i.e., sync UNIX permissions normally.

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

What the above does is establish a convention of files starting with `.!` as
being machine-specific, and then not syncing symlinks to those files. The files
themselves are still synced. (The purpose of this being that things like
`.bashrc` which might vary are still accessible to all clients in the correct
place while allowing the underlying files to sync normally.)

Key Management
--------------

Ensync allows associating any number of passphrases/keys with the server store.
The `ensync key` subcommands can be used to inspect and manipulate the key
store.

Separate keys do not — in and of themselves — control access to the underlying
data; rather, multiple keys are mainly useful for ensuring that it is easy to
revoke access to a particular client without needing to change keys everywhere
else.

For small passphrases (less than 64 characters), Ensync uses a very strong
hashing function that can take some systems multiple seconds per key in the key
store. If you plan to use a large number of keys, it helps to generate random
"passphrases" which are much longer (e.g., `dd if=/dev/urandom of=key count=1`,
then put `file:key` as the passphrase in the configuration).

Ensync requires all passphrases in the key store to be distinct. This is
admittedly an unusual requirement. Note though that nothing is being leaked;
clients already get the whole key store to test their passphrases against, so
the simple fact that a passphrase conflicts is not new information that could
be used in an attack. The restriction exists because Ensync does not, for the
sake of ergonomics, take a key name and passphrase pair, but rather only a
passphrase, so if two keys had the same passphrase, only one of them would be
accessible.

Using Key Groups
----------------

Each key in the key store can be associated with any number of _key groups_.
Unlike keys, key groups can actually restrict access to information.
Concretely, each key group corresponds to an internal encryption key, and
adding a key to a key group adds to that key the information needed to derive
that internal key.

By default, there are two key groups:

- `everyone`. All keys are always in this group. The `everyone` group is used
  as the key for the HMAC of all file blocks and is used as the encryption key
  of the physical root directory and by default all directories below.

- `root`. The `root` group grants the power to edit the key store, and is the
  write key for the physical root directory and by default all directories
  below.

The `ensync key group` subcommands can be used to create and assign key groups.
To make key groups useful, one must understand how the _read key_ and _write
key_ are used when encrypting things on the server.

The _read key_ of a directory is the encryption key used to encrypt its
content. It is cryptographically unfeasible for a reader without the read key
to read a directory or to manipulate the directory content meaningfully.

The _write key_ is required to write a directory or the key store _through a
normal ensync implementation_. Unlike the read key, it has _no cryptographic
significance_; rather, it is a defence-in-depth measure so that the compromise
of a client with some access to ensync (but nothing else) cannot be used to,
e.g., destroy the key store or cause more data loss.

The default read key is from the `everyone` group, and the default write key is
from the `root` group. Normally, a directory inherits the keys of its parent.
To change the keys, one puts special syntax in the _name_ of that directory.
(The fact that this is a very loud and "sticky" mechanism is by design.) The
syntax is to place `.ensync[config=value,...]` anywhere in the name. `config`
is one of the following:

- `r`. Set the read key to the internal key of the group identified by `value`.

- `w`. Set the write key to the internal key of the group identified by `value`.

- `rw`. Set both the read and the write key to the internal key of the group
  identified by `value`.

For example, a directory named `foo.ensync[r=my-group]` will cause the
directory to have the read key from the `my-group` group.
`foo.ensync[r=my-group,w=another-group]` sets the read and write keys to
`my-group` and `another-group`, respectively.

The simplest way to use this feature is in the logical root name (i.e., the
`server_root` configuration key). For example, to create a sync root which only
your client has access to, you might do something like this:

```
$ ensync key group create /path/to/config my-group
$ ensync mkdir /path/to/config /my-client.ensync[rw=my-group]
```

and then edit the configuration to have `my-client.ensync[rw=my-group]` as the
`server_root`.

Ensync Shell
------------

It is possible to make `ensync` a user's shell. This will cause it to always
behave as if invoked as `ensync server` with a particular (fixed) path. The
purpose of this is to make it possible for a remote client to access Ensync
over ssh without having actual shell access.

The basic procedure for this is:

- Add the full path to `ensync` to `/etc/shells`.

- Create the user with `ensync` as their shell.

- In the user's home directory, make a symlink named `ensync-server-dir` to the
  directory where Ensync should store the server data.

You can then simply use `shell:ssh -T user@host` as the `server` configuration.
For `ensync setup`, use `user@host:` as the remote path.

As always, defence-in-depth measures may also be advisable. E.g., make the
user's home directory read-only and not owned by them, or even run the whole
thing in a chroot (which is fairly easy since `ensync` does not need anything
beyond a fist-full of shared objects to run).

Security Considerations
-----------------------

### How Ensync does Encryption

For the purpose of not duplicating documentation, this section only skims over
the details. The [source code](src/server/crypt.rs) has more detailed
documentation.

Passphrases are hashed via [scrypt](http://www.tarsnap.com/scrypt.html). The
client verifies the passphrase is correct by comparing the SHA-3 of the derived
key with a value stored on the server.

Each key group represents a randomly-generated 32-byte internal key. To go from
a derived key to the internal key group, the client takes the SHA-3 HMAC of the
group name and the derived key, and then XORs every byte with a sequence stored
in the key store on the server. Each 32-byte internal key is split into a
16-byte AES key and a 16-byte HMAC secret.

Every directory file is encrypted via AES CBC (128-bit) with a random session
key and IV, which themselves are encrypted with the read key of that directory
and stored at the beginning of the directory file. Directories are internally
checksummed with SHA-3 HMAC using the HMAC secret from the read key.

File blocks are encrypted with AES CBC using a key and IV taken from the SHA-3
HMAC of the block content and the HMAC secret of the `everyone` group's
internal key.

The following things are saved in cleartext on the server and are considered
"essentially public" information:

- The names of keys and key groups.

- The algorithm used for hashing each passphrase.

- The salt for each passphrase.

- Other metadata about each passphrase.

- The SHA-3 sum of each passphrase's derived key.

- The XOR of the { HMAC of each passphrase's derived key and associated key
  group name } with the internal key of that key group.

- The random 32-byte ids of directories, and their encrypted version numbers.

- The SHA-3 sum of the HMAC of file blocks' content with the `everyone` group's
  HMAC secret.

- The length of directory files and the file blocks.

Of course, ideally one protects the server such that this isn't actually
revealed.

### What attackers can accomplish

While Ensync is designed to keep your files safe, there are some trade-offs to
be aware of (including some which open certain side-channels), and one should
understand what an attacker could learn or do if they gain access to various
things.

This should be prefaced by to notices:

- Practise defence-in-depth. I.e., take standard measures to keep the server
  data secure as well; use an encrypted channel for communicating with the
  server (e.g., ssh, not telnet); etc.

- Statements of what an attacker _can_ do are hard fact. Statements of what an
  attacker _cannot_ do are subject to things being broken.

The sections below are roughly ordered in ascending order by severity.

#### Passively observation of encrypted Ensync ssh session

It is likely possible to identify Ensync by its traffic patterns.

The observer can use timing to estimate how many key entries in the key store
were rejected. This side-channel does not reduce the strength of passphrase
handling below that of having only one passphrase.

The observer can get a reasonably good idea of how many directories are within
the tree being synced as well as a rough estimate of how many things have
changed based on the traffic quantity.

#### Connection to ensync server

E.g., the attacker gained access to an ssh session running `ensync server`.

Fetching the key store (which is cleartext) is trivial.

The encrypted physical root directory can be fetched trivially, which can be
used to get a rough estimate of the number of items in the root.

It is also possible to probe for particular directory ids or object ids and get
their contents, though finding what these ids are may not be easy.

#### Impersonating the Ensync server

E.g., man-in-the-middle of an insecure ssh setup.

Includes everything from the above scenarios.

The attacker will learn various directory ids, object ids, as well as the
"secret version" ids of some directories which guard write access to those
directories.

Assuming the attacker can proxy to the real Ensync server, it can get some
information about the hierarchy of directory ids due to the order in which the
client requests them.

#### Access to a dump of the Ensync server data

The attacker learns all directory ids, versions (secret and otherwise) and
object ids. This also includes the key store in cleartext. It may be possible
to determine how often certain directories are updated.

Combined to access to an Ensync server session, this allows overwriting
arbitrary directories with arbitrary data (though garbage since the attacker
could not correctly encrypt it without also being able to decrypt the data
anyway).

The actual data is still encrypted here, and an attacker would need to
determine the internal keys to do anything.

#### Shell access to the Ensync server data

All of the above. The attacker can, of course, tinker with the server data
stored arbitrarily.

*However,* the attacker cannot manipulate the server data in a way that the
clients won't notice. This includes even taking a snapshot of the data and
later reverting (in the hopes of causing the clients to revert their own data
in response); clients will detect this reversion and refuse to continue.

#### Compromise of a client (including its Ensync key)

The attacker can read and decrypt everything that the client's key has been
granted access to, and write everything that the Ensync server allows. The
attacker can determine what the HMAC of particular file blocks would be, and
probe the server to see if that content has been stored (even in an area the
attacker doesn't otherwise have access to).

Directories with a read key the attacker doesn't have are still safe, as are
the objects referenced by them.

### Recovering from Leaks

If a passphrase is leaked, but not access to the server, simply replace the
passphrase, using `ensync key change`. The important part here is to ensure
that no attacker that knows the passphrase ever sees the Ensync key store at a
time that that passphrase was represented in the key store.

If the Ensync key store is leaked, you have a bigger — but not immediate —
problem, since changing the passphrase will not change the underlying internal
key. That is, the if the attacker can use the key store to derive an internal
key in their own time, and when done, that internal key will still be valid.

Provided a sensible passphrase is used, deriving an internal key from a key
store should be intractable. You should still change the passphrases so that if
they are later leaked too, they cannot be used with the leaked key store.
However, if the possibility of the key store being broken in isolation is a
concern, you may want to replace the internal keys as well. There is no quick
way to do this; essentially, it is a multi-step process:

- Grab all the data from the server and store it somewhere safe. `ensync get`
  can be used for this.

- Destroy the data on the server.

- Set the server up again, recreate keys, etc.

- Put the data onto the new server store. `ensync put` can be used for this.

- Remove the `server-state.sqlite` file from _every_ client's `internal.ensync`
  directory. If this is not done, clients will detect the above steps as a
  possible reversion attack and not proceed.

License
-------

[GLPv3 or later](COPYING)
