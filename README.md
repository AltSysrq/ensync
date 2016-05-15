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

Abstractly, Ensync compares three parallel streams of linearised directory
hierarchies, referred to as "client", "ancestor", and "server". The ancestor is
essentially a lightweight record of the last point at which the client and
server agreed. Each file in the stream is one of the following:

- Directory: name, mode, (contents)
- Regular file: name, mode, size, (last-modified), content hash
- Symlink: name, content
- Special (devices, sockets, etc): name

Two files "match" if they are of the same type and all properties not notated
in parentheses are equal.

Regular files use the content hash from the corresponding ancestor file if the
size and last-modified fields match, and otherwise use an "unknown" hash, so
that files are not read in unnecessarily.

### Reconciliation

As the three streams are iterated, files are intersected by their name, and the
resulting tuples are _reconciled_, producing zero to two outputs on each
stream and performing operations on the server and client as needed.

The exact behaviour of reconciliation is dependent on the _sync mode_ of the
file. A sync mode is notated like "cud/cud", using hyphens instead of letters
to indicate "off", lowercase letters for "on", and uppercase letters for
"force". The six settings are: sync inbound create, sync inbound update, sync
inbound delete, sync outbound create, sync outbound update, sync outbound
delete. Thus the sync mode "cud/cud" is a standard conservative symmetric sync;
"---/CUD" forces the server to look like the client; "---/---" causes no
changes to propagate, and so on.

Special files always have mode "---/---".

"Inbound" and "outbound" refer to changes propagating from server to client and
from client to server, respectively. "Create" describes the behaviour of new
files (ie, the name is found only on one side and not in the ancestor);
"update" describes the behaviour of changed files (ie, the name is found on
both client and ancestor); "delete" describes the behaviour of deleted files
(ie, the name is found on one of client and server and is found in the
ancestor).

An edit/edit conflict occurs when the client, ancestor, and server all mutually
mismatch. By default, the server file is renamed to avoid the conflict and the
ancestor discarded, generally resulting in one create each way. If "sync
inbound update" is set to force and "sync outbound update" is not, conflicts
will instead be resolved by discarding the client version and replacing it with
the server version. Conversely, if "sync outbound create" is set to force and
"sync inbound update" is not, the client version replaces the server version.
If both update settings are set to "force", the version with the later
last-modified value wins and the other is discarded; ties go to the client.

Edit/edit conflicts over file modes alone are handled specially. If a rename
due to the conflict would occur, instead the client mode is simply used. Note
that this means that two directory versions cannot be subject to an edit/edit
conflict resulting in the renaming of one of them; they will always be merged
recursively.

An edit/delete conflict occurs when the client or server mismatches the
ancestor, and the other side does not exist. By default, this is resolved by
discarding the ancestor, allowing the edit to propagate as a create. If the
side with the deleted version has the "sync delete" mode set to force, the file
is instead deleted on the other side.

There is a subtlety here with respect to directories. Ordinarily, if a
directory is to be deleted, it is "provisionally" emitted anyway in case it is
found that new files have been created within it. Setting a delete mode to
"force" prevents this; all files within force-deleted directories are dropped,
even if this would ordinarily be considered an edit/delete conflict (and even
if those files would have had a mode that caused them to be resurrected in that
case).

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

Install Rust 1.7.0 and Cargo 0.9.0 or later (https://www.rust-lang.org/).

Install `gpgme`, eg, `pkg install gpgme` as root.
