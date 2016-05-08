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

Ensync uses a simple client-server model, where the client operates on the
cleartext files and makes most decisions, and the server provides a dumb blob
store. Typically the server is invoked over SSH, but it can also be run locally
to, eg, sync with a removable device.

### Server Side

The server, for the most part, knows nothing about the sync model. Its main
functionality is storing a mapping from client-supplied 256-bit identifiers to
data payloads and reference counts. In practise, the identifiers are SHA-3 sums
of the cleartext with the pepper prepended and appended, though the server has
no way to verify this. The reference counts are also maintained via commands
from the client, since the server cannot identify references itself.

The server also maintains a table of cleartext identifiers to blob identifiers,
which serve as the roots of the file trees.

### Client Side

#### Regular Files and Symlinks

When Ensync encounters a regular file or symlink, it reads its mode and
contents into memory, determines its hash value, and queries the server whether
such a blob exists. If not, it is uploaded.

Files larger than 1MB are split into 1MB pieces and each piece is processed
separately, and a separate blob listing the pieces is created to stand in for
the file.

The hash values for files are cached, and it is assumed the file is unmodified
since the last sync if the modified date and mode are both unchanged.

The client attempts to map cleartext hashes to existing files, so that syncs of
file movement do not need to redownload the files from the server. When files
are created or updated in the client, they are first written in a temporary
location and then renamed into the desired location. When files are deleted in
the client, they are moved to a temporary location and deleted on exit.

#### Directories

Syncing directories is more complicated. A directory is modelled as a simple
sequence of (name,type,hash) tuples, sorted by name. The changes in a directory
are reconciled by comparing the below three states and deriving new
client/server states according to the current sync mode.

- The local state of the directory as calculated by the client.

- The state of the directory as stored in a blob on the server.

- The "ancestor state" stored locally by the client, which lists tuples which
  were in the same state at the end of the previous sync.

During reconciliation, tuples are paired by name, represented as
(client,ancestor,server) below. The sync mode is a set of 6 bits, notated
"cud/cud" for everything on (symmetric sync) or "---/---" for everything off
(no changes). These bits indicate respectively:

- Create Inbound. (nil,nil,a) -> (a,a,a). Ie, if the entry exists on the server
  but not in the client or ancestor state, bring it onto the client.

- Update Inbound. (a,a,b) -> (b,b,b). Ie, if the entry exists on both sides,
  but is unchanged on the client and updated on the server, throw the client's
  version away and fetch the server's version.

- Delete Inbound. (a,a,nil) -> nil. Ie, if the entry exists on the client and
  in the ancestor state but not on the server, delete from the client.

- Create Outbound. (a,nil,nil) -> (a,a,a). Ie, if the entry exists on the
  client but not in the server or the ancestor state, upload it to the server.

- Update Outbound. (b,a,a) -> (b,b,b). Ie, if the entry exists on both sides,
  but the client version differs from the ancestor state and the server version
  matches the ancestor state, throw the server version away and copy the client
  version to the server.

- Delete Outbound. (nil,a,a) -> nac. Ie, if the entry exists on the server and
  in the ancostr state mut not on the client, remove the entry from the server.

Regardless of sync mode, (a,a,a) is left unchanged, (a,nil,a) is reconciled to
(a,a,a), and (nil,a,nil) is reconciled to nil.

If each member of the sync triple is in a different state, it is termed
_unreconcilable_. When this occurs, the name in the server tuple is changed by
appending `~` and a number to make it unique, and the original sync triple is
reconciled as if the server had the same state as the ancestor. That is,
(a,b,c) is reconciled as two separate triples, (a,b,b) and (nil,nil,c).

Concurrency and Failure
-----------------------

Ensync will not behave incorrectly if any client files are modified while it is
running, insofar as that it will not corrupt the store on the server. However,
such files cannot be captured in an atomic state, and may therefore be less
than useful.

Multiple Ensync instances should not be run concurrently over the same
directory tree. A best effort is made to detect this condition and abort when
it happens. It is safe to run multiple Ensync instances concurrently against
the same server store.

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
files are treated as empty symlinks.

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
