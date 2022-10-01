//-
// Copyright (c) 2016, 2017, 2018, 2021, Jason Lingle
//
// This file is part of Ensync.
//
// Ensync is free software: you can  redistribute it and/or modify it under the
// terms of  the GNU General Public  License as published by  the Free Software
// Foundation, either version  3 of the License, or (at  your option) any later
// version.
//
// Ensync is distributed  in the hope that  it will be useful,  but WITHOUT ANY
// WARRANTY; without  even the implied  warranty of MERCHANTABILITY  or FITNESS
// FOR  A PARTICULAR  PURPOSE.  See the  GNU General  Public  License for  more
// details.
//
// You should have received a copy of the GNU General Public License along with
// Ensync. If not, see <http://www.gnu.org/licenses/>.

#![recursion_limit = "1024"]

// Not sure why the `rust_` prefix gets stripped by cargo
extern crate crypto as rust_crypto;
#[macro_use]
extern crate fourleaf;
#[macro_use]
extern crate error_chain;
#[macro_use]
extern crate lazy_static;
#[macro_use]
extern crate quick_error;

#[cfg(test)]
#[macro_use]
extern crate proptest;

mod defs;
mod errors;
mod interrupt;
mod sql;
mod work_stack;

mod ancestor;
mod block_xfer;
mod cli;
mod dry_run_replica;
mod log;
#[cfg(test)]
mod memory_replica;
mod posix;
mod reconcile;
mod replica;
mod rules;
mod server;

use std::fs::File;
use std::os::unix::io::{FromRawFd, RawFd};
use std::path::PathBuf;

use clap::AppSettings;
use structopt::StructOpt;

use crate::cli::config::PassphraseConfig;
use crate::errors::{Result, ResultExt};
use crate::rules::SyncMode;

fn main() {
    use std::io::{stderr, Write};
    use std::process::exit;

    let result = main_impl();

    if let Err(e) = result {
        let _ = writeln!(stderr(), "error: {}", e);
        for e in e.iter().skip(1) {
            let _ = writeln!(stderr(), "caused by: {}", e);
        }

        if let Some(bt) = e.backtrace() {
            let _ = writeln!(stderr(), "{:?}", bt);
        }

        exit(1);
    }
}

/// Encrypted bidirectional file synchroniser
#[derive(StructOpt)]
#[structopt(
    max_term_width = 100,
    setting(AppSettings::DontCollapseArgsInUsage),
    setting(AppSettings::DeriveDisplayOrder),
    setting(AppSettings::SubcommandRequiredElseHelp),
    setting(AppSettings::UnifiedHelpMessage),
    setting(AppSettings::VersionlessSubcommands)
)]
enum Command {
    Setup(SetupSubcommand),
    Sync(SyncSubcommand),
    Key(KeySubcommand),
    #[structopt(alias = "dir")]
    Ls(LsSubcommand),
    #[structopt(alias = "md")]
    Mkdir(MkdirSubcommand),
    #[structopt(alias = "rd")]
    Rmdir(RmdirSubcommand),
    #[structopt(alias = "type", alias = "dump")]
    Cat(CatSubcommand),
    Get(GetSubcommand),
    Put(PutSubcommand),
    #[structopt(alias = "del")]
    Rm(RmSubcommand),
    Server(ServerSubcommand),
}

/// Perform key management operations.
#[derive(StructOpt)]
#[structopt(setting(AppSettings::SubcommandRequiredElseHelp))]
enum KeySubcommand {
    Init(KeyInitSubcommand),
    Add(KeyAddSubcommand),
    Change(KeyChangeSubcommand),
    #[structopt(alias = "del")]
    Rm(KeyRmSubcommand),
    #[structopt(alias = "list")]
    Ls(KeyLsSubcommand),
    Group(KeyGroupSubcommand),
}

/// Manage key groups.
#[derive(StructOpt)]
#[structopt(setting(AppSettings::SubcommandRequiredElseHelp))]
enum KeyGroupSubcommand {
    Create(KeyGroupCreateSubcommand),
    Assoc(KeyGroupAssocSubcommand),
    #[structopt(alias = "dissoc")]
    Disassoc(KeyGroupDisassocSubcommand),
    Destroy(KeyGroupDestroySubcommand),
}

/// Wizard to set up simple ensync configurations.
#[derive(StructOpt)]
#[structopt(after_help(
    "\
`ensync setup` is an interactive process to walk you through the initial \
setup process and to generate a simple configuration automatically. It \
is suitable for many use-cases and is the recommended way to get started.

For example, to sync your whole home directory to a flash drive you have \
mounted on `/mnt/flash`, storing the configuration in `~/.ensync`, you would \
run

        ensync setup ~/.ensync ~ /mnt/flash

Similarly, to instead back up to a server have hosted in a folder named \
\"data\" under your home directory there, you would run something like

        ensync setup ~/.ensync ~ username@yourhost.example.com:data

<local-path> must be a directory that already exists. <config> will be \
created as a directory during the setup process and must not already \
exist. The path in <remote-path> will be created if it does not already \
exist."
))]
struct SetupSubcommand {
    /// Path in the local filesystem where ensync's configuration and state
    /// will be stored. The path must not already exist.
    #[structopt(parse(from_os_str))]
    config: PathBuf,

    /// How to get the passphrase. Defaults to `prompt` to read it
    /// interactively. Use `string:xxx` to use a fixed value, `file:/some/path`
    /// to read it from a file, or `shell:some shell command` to use the
    /// output of a shell command.
    #[structopt(short, long, default_value = "prompt")]
    key: PassphraseConfig,

    /// Path in the local filesystem of files to be synced. Must be an already
    /// existing directory.
    #[structopt(parse(from_os_str))]
    local_path: PathBuf,

    /// Wher to write encrypted data. This may be a local path (e.g.,
    /// `/path/to/files`) or an scp-style argument (e.g.,
    /// `user@host:/path/to/files`). For a remote host using ensync shell, use
    /// `user@host:`.
    #[structopt(parse(from_os_str))]
    remote_path: PathBuf,
}

/// Sync files according to configuration.
#[derive(StructOpt)]
#[structopt(after_help(
    "\
As one might expect, `ensync sync` is the main subcommand. When invoked, \
`ensync sync` will scan for file changes and automatically propagate them \
to and from the server according to the configuration.

All default output goes to standard error. Standard output is reserved for \
the output of `--itemise`.

The \"verbose\" and \"quiet\" arguments move the standard error verbosity \
along the below spectrum, where \"quiet\" is negative and \"verbose\" is \
positive:

        -∞ Fatal errors. These cannot be turned off.

        -2 Errors which prevented changes from being applied.

        -1 Warnings about sync conflicts or other irreconcilable situations.

         0 Changes being made to files, excluding creations of files underneath
           newly-created directories and deletions of files underneath
           directories being deleted. This is the default.

         1 All changes being made to files.

         2 Verbose informational messages, such as the state of files which are
           not being changed.

It is safe to interrupt the syncing process with Control-C. Interrupting the \
process less gracefully (e.g., with `kill -9`) will not result in data loss, \
but may leave stray temporary files or corrupt the cache."
))]
struct SyncSubcommand {
    #[structopt(flatten)]
    config: ConfigArg,

    #[structopt(flatten)]
    verbosity: VerbosityArgs,

    /// Output rsync-like itemisation to stdout.
    #[structopt(short, long, alias = "itemize")]
    itemise: bool,

    /// With `--itemise`, also include unchanged items.
    #[structopt(long, alias = "itemize-unchanged")]
    itemise_unchanged: bool,

    /// Log happenings in the internal ancestor replica.
    #[structopt(long)]
    include_ancestors: bool,

    /// Specify the number of threads to use [default: CPUs + 2]
    #[structopt(long)]
    threads: Option<u32>,

    /// Control colourisation of stderr.
    #[structopt(long, alias = "color", default_value = "auto",
                possible_values = &["auto", "always", "never"])]
    colour: String,

    /// Control whether the progress spinner is shown.
    #[structopt(long, alias = "spinner", default_value = "auto",
                possible_values = &["auto", "always", "never"])]
    spin: String,

    /// Don't actually make any changes.
    #[structopt(short = "n", long)]
    dry_run: bool,

    /// When done syncing, continue running and monitor for file changes and
    /// resync when detected. Syncs happen a short time after the changes occur
    /// to give the system time to reach quiescence. This requires extra
    /// resources both locally and on the server.
    #[structopt(short, long, conflicts_with = "dry_run")]
    watch: bool,

    /// Before responding to a filesystem notification (as part of `--watch`),
    /// delay this many seconds to wait for the filesystem to be quiescent.
    #[structopt(long, default_value = "30")]
    quiescence: u64,

    /// In conjunction with `--watch`, if an error occurs, back off for the
    /// given number of seconds, then attempt to restart the server process and
    /// resume operations. If this flag is given, `ensync sync` should not
    /// terminate on its own.
    #[structopt(long, requires = "watch")]
    reconnect: Option<u64>,

    /// Control what is checked for syncing. "fast" means to only scan
    /// directories that have obviously changed. "clean" causes all directories
    /// to be scanned. "scrub" additionally causes all cached hashes to be
    /// purged, which will make syncing quite slow but can be used to recover
    /// from a corrupted cache or changes to the local filesystem that were not
    /// reflected in its metadata.
    #[structopt(long, default_value = "auto",
                possible_values = &["fast", "clean", "scrub", "auto"])]
    strategy: String,

    /// Ignore all sync rules in the configuration and sync all files with the
    /// given sync mode. This is most useful in conjunction with the
    /// `reset-server` and `reset-client` mode aliases. If `--strategy` is also
    /// `auto`, implies `--strategy=clean`.
    #[structopt(long)]
    override_mode: Option<SyncMode>,

    /// Output line-delimited status messages to the given file descriptor. The output is
    /// unaffected by the `-v` or `-q` flags.
    #[structopt(long)]
    json_status_fd: Option<RawFd>,
}

/// Initialise the key store.
#[derive(StructOpt)]
#[structopt(after_help(
    "\
This command is used to initialise the key store in the server. It generates \
a new internal key set and associates one user key withthem, corresponding \
to the passphrase defined in the configuration. You should specify the key \
name if you plan on using multiple keys. This cannot be used after the key \
store has been initialised; see `key add` for that instead."
))]
struct KeyInitSubcommand {
    #[structopt(flatten)]
    config: ConfigArg,

    /// Name for the first key in the key store.
    #[structopt(long, default_value = "initial-key")]
    key_name: String,

    #[structopt(skip)]
    verbosity: NonVerbose,
}

/// Add a new key to the key store.
#[derive(StructOpt)]
#[structopt(after_help(
    "\
This command creates a new key / passphrase in the key store. This is used to \
have different clients use different credentials to access the store. \
Multiple keys can also be used in conjunction with key groups to restrict \
access to portions of the directory tree, or prevent certain clients from \
writing to particular locations, including the key store itself.

This operation requires an existing key from which to derive the data for the \
new key. By default, the existing key will be taken from the configuration. \
The new key will inherit all key groups of the old key. If this is undesired, \
they can be removed with `ensync key group disassoc`.

The passphrase for the new key must be distinct from other passphrases \
currently in use, since the passphrase is used to identify the key implicitly.

Since this operation modifies the key store, a key in the `root` group is \
required. If the existing key is in the `root` group, it will be used to \
do this implicitly. Otherwise, a separate key will need to be provided. \
By default, this prompts the terminal, but the `--root` argument can be \
used to use other passphrase methods."
))]
struct KeyAddSubcommand {
    #[structopt(flatten)]
    config: ConfigArg,

    #[structopt(flatten)]
    old: OldKeyArg,
    #[structopt(flatten)]
    new: NewKeyArg,
    #[structopt(flatten)]
    root: RootKeyArg,

    /// The name of the key to add.
    key_name: String,

    #[structopt(skip)]
    verbosity: NonVerbose,
}

/// Change a key in the key store.
#[derive(StructOpt)]
#[structopt(after_help(
    "\
This command changes the passphrase used to access a key in the key store.

This requires an existing key and the new passphrase. By default, the \
existing key is drawn from the configuration. The `--old` argument can \
be used to change this. The new key is by default read from the terminal; \
the `--new` argument can be used to override this.

The new passphrase for the key must be distinct from other passphrases \
currently in use, since the passphrase is used to identify the key implicitly.

Unless `--force` is passed, this command will fail if the passphrase provided \
as the existing key does not correspond to the key named on the command-line. \
If `--force` is passed, authorising with a different passphrase is permitted, \
but the key that passphrase unlocks must at least be in every key group that \
the key to be changed is in.

Since this operation modifies the key store, a key in the `root` group is \
required. If the existing key is in the `root` group, it will be used to \
do this implicitly. Otherwise, a separate key will need to be provided. \
By default, this prompts the terminal, but the `--root` argument can be \
used to use other passphrase methods.

This command does *not* change the internal keys used for encryption. Doing \
so would require rebuilding the entire server store. If doing this really is \
desired, there is some information on how to do this in the documentation."
))]
struct KeyChangeSubcommand {
    #[structopt(flatten)]
    config: ConfigArg,

    #[structopt(flatten)]
    old: OldKeyArg,
    #[structopt(flatten)]
    new: NewKeyArg,
    #[structopt(flatten)]
    root: RootKeyArg,

    /// The name of the key to edit.
    key_name: Option<String>,

    /// Change `key-name` even if the old passphrase does not correspond to
    /// that key.
    #[structopt(short, long)]
    force: bool,

    #[structopt(skip)]
    verbosity: NonVerbose,
}

/// Delete a key from the key store.
#[derive(StructOpt)]
#[structopt(after_help(
    "\
This command deletes the named key from the key store.

It is not legal to delete the last key from the key store. A key also may not \
be deleted if it is the last member of any particular key group, as doing so \
would destroy the group.

Since this operation modifies the key store, a key in the `root` group is \
required. By default, this prompts the terminal, but the `--root` argument \
can be used to use other passphrase methods. Note that you cannot use the \
passphrase of the key being deleted for this, even if that key is in the \
`root` group."
))]
struct KeyRmSubcommand {
    #[structopt(flatten)]
    config: ConfigArg,

    #[structopt(flatten)]
    root: RootKeyArg,

    /// The name of the key to delete.
    key_name: String,

    #[structopt(skip)]
    verbosity: NonVerbose,
}

/// List the keys in the key store.
#[derive(StructOpt)]
struct KeyLsSubcommand {
    #[structopt(flatten)]
    config: ConfigArg,

    #[structopt(skip)]
    verbosity: NonVerbose,
}

/// Create key group(s).
#[derive(StructOpt)]
#[structopt(after_help(
    "\
Creates one or more key groups. All groups will initially be granted \
to the key that was derived from the passphrase.

By default, this applies to the passphrase obtained as described by \
the configuration. The `--key` argument can be used to override this.

Since this operation modifies the key store, a key in the `root` group is \
required. If the existing key is in the `root` group, it will be used to \
do this implicitly. Otherwise, a separate key will need to be provided. \
By default, this prompts the terminal, but the `--root` argument can be \
used to use other passphrase methods."
))]
struct KeyGroupCreateSubcommand {
    #[structopt(flatten)]
    config: ConfigArg,

    #[structopt(flatten)]
    root: RootKeyArg,

    /// The name(s) of the group(s) to create.
    #[structopt(required = true)]
    group: Vec<String>,

    #[structopt(skip)]
    verbosity: NonVerbose,
}

/// Associate a key with key group(s).
#[derive(StructOpt)]
#[structopt(after_help(
    "\
Associates one or more key groups to a key, granting clients using that key \
access to resources protected by that key group.

This operation involves two passphrases; the first must be a member of all \
the listed groups and is used to derive the information needed to grant them. \
By default, it is obtained as described by the configuration, but this can be \
overridden with `--from`. The second is the one to which the groups are to be \
granted. By default, it is read from the terminal, but `--to` can override \
this.

Since this operation modifies the key store, a key in the `root` group is \
required. If either of the above keys are in the `root` group, it will be \
used to do this implicitly. Otherwise, a separate key will need to be \
provided. By default, this prompts the terminal, but the `--root` argument \
can be used to use other passphrase methods."
))]
struct KeyGroupAssocSubcommand {
    #[structopt(flatten)]
    config: ConfigArg,

    #[structopt(flatten)]
    root: RootKeyArg,
    #[structopt(flatten)]
    from: FromKeyArg,
    #[structopt(flatten)]
    to: ToKeyArg,

    /// The name(s) of the group(s) to grant.
    #[structopt(required = true)]
    group: Vec<String>,

    #[structopt(skip)]
    verbosity: NonVerbose,
}

/// Disassociate a key from key group(s).
#[derive(StructOpt)]
#[structopt(after_help(
    "\
Disassociates one or more key groups from a key identified by name, \
essentially undoing the effect of `key group assoc`.

This cannot be used to remove a group from the last key associated; use `key \
group destroy` for that instead. The `everyone` key group cannot be removed \
from any key.

Since this operation modifies the key store, a key in the `root` group is \
required. By default, this prompts the terminal, but the `--root` argument \
can be used to use other passphrase methods. Note that you cannot use the \
passphrase of the key being modified by this operation if removing the `root` \
group from it.

Cryptographically speaking, this is not a very strong operation, since the \
internal key behind the key group is unchanged. That is, if the key store \
was leaked before this operation, an attacker which knows the passphrase \
for the key can still derive the internal key of the key group and use it \
even after this operation has been performed."
))]
struct KeyGroupDisassocSubcommand {
    #[structopt(flatten)]
    config: ConfigArg,

    #[structopt(flatten)]
    root: RootKeyArg,

    /// The key from which to remove the listed group(s).
    key_name: String,

    /// The name(s) of the group(s) to remove.
    #[structopt(required = true)]
    group: Vec<String>,

    #[structopt(skip)]
    verbosity: NonVerbose,
}

/// Destroy key group(s).
#[derive(StructOpt)]
#[structopt(after_help(
    "\
Destroys the listed groups, implicitly disassociating them from any and all \
keys.

The `root` and `everyone` key groups may not be destroyed.

WARNING: This is an irreversible operation which will render any data \
protected by the destroyed key groups irrecoverable.

Since this operation modifies the key store, a key in the `root` group is \
required. By default, this prompts the terminal, but the `--root` argument \
can be used to use other passphrase methods."
))]
struct KeyGroupDestroySubcommand {
    #[structopt(flatten)]
    config: ConfigArg,

    #[structopt(flatten)]
    root: RootKeyArg,

    /// Don't prompt for confirmation.
    #[structopt(short, long)]
    yes: bool,

    /// The name(s) of the group(s) to destroy.
    #[structopt(required = true)]
    group: Vec<String>,

    #[structopt(skip)]
    verbosity: NonVerbose,
}

/// List directory contents on the server.
#[derive(StructOpt)]
#[structopt(after_help(
    "\
Lists the content of each given directory (or individual file) on the server.

If <path> does not start with `/`, it is relative to the `server_root` value \
in the configuration. Otherwise, it starts from the physical root of the \
server."
))]
struct LsSubcommand {
    #[structopt(flatten)]
    config: ConfigArg,

    /// Display sizes in human-readable format.
    #[structopt(short, long)]
    human_readable: bool,

    /// The path(s) to list.
    #[structopt(required = true, parse(from_os_str))]
    path: Vec<PathBuf>,

    #[structopt(skip)]
    verbosity: NonVerbose,
}

/// Directly create directories on the server.
#[derive(StructOpt)]
#[structopt(after_help(
    "\
Creates a directory entry on the server.

If <path> does not start with `/`, it is relative to `server_root` value \
in the configuration. Otherwise, it starts from the physical root of the \
server.

The main use of this is to manually create logical sync roots. For example, \
the below would be used to create the `myroot` logical root (i.e., \
`server_root = \"myroot\"` in the configuration):

        ensync mkdir /path/to/config /myroot

The UNIX permissions do not control access to content on the server; they \
are only used when a server directory is propagated to the local filesystem."
))]
struct MkdirSubcommand {
    #[structopt(flatten)]
    config: ConfigArg,

    /// UNIX permissions for the new directory.
    #[structopt(short, long, default_value = "0700")]
    mode: String,

    /// The path(s) to create.
    #[structopt(required = true, parse(from_os_str))]
    path: Vec<PathBuf>,

    #[structopt(skip)]
    verbosity: NonVerbose,
}

/// Remove empty directories on the server.
#[derive(StructOpt)]
#[structopt(after_help(
    "\
Removes the listed directory(ies), which must be empty. To remove non-empty \
directories, use `rm` with the `-r` flag.

If <path> does not start with `/`, it is relative to `server_root` value \
in the configuration. Otherwise, it starts from the physical root of the \
server."
))]
struct RmdirSubcommand {
    #[structopt(flatten)]
    config: ConfigArg,

    /// The path(s) to remove.
    #[structopt(required = true, parse(from_os_str))]
    path: Vec<PathBuf>,

    #[structopt(skip)]
    verbosity: NonVerbose,
}

/// Dump file contents to standard output.
#[derive(StructOpt)]
#[structopt(after_help(
    "\
Dumps the raw content of all listed files to standard output, without any \
other information. Every path must be a regular file (symlinks are not \
permitted).

If <path> does not start with `/`, it is relative to `server_root` value \
in the configuration. Otherwise, it starts from the physical root of the \
server."
))]
struct CatSubcommand {
    #[structopt(flatten)]
    config: ConfigArg,

    /// The path(s) to dump.
    #[structopt(required = true, parse(from_os_str))]
    path: Vec<PathBuf>,

    #[structopt(skip)]
    verbosity: NonVerbose,
}

/// Directly fetch files from the server.
#[derive(StructOpt)]
#[structopt(after_help(
    "\
Fetches the named file or directory tree from the server, copying it to the \
local filesystem.

This behaves roughly similarly to `cp -a`, in that it will copy files \
recursively with attributes, and doesn't treat symlinks specially.

If <src> does not start with `/`, it is relative to `server_root` value \
in the configuration. Otherwise, it starts from the physical root of the \
server."
))]
struct GetSubcommand {
    #[structopt(flatten)]
    config: ConfigArg,

    #[structopt(flatten)]
    verbosity: VerbosityArgs,

    /// Allow overwriting existing files.
    #[structopt(short, long)]
    force: bool,

    /// Path of the file or directory to fetch from the server.
    #[structopt(parse(from_os_str))]
    src: PathBuf,

    /// Location to which to copy the files.
    #[structopt(parse(from_os_str), default_value = ".")]
    dst: PathBuf,
}

/// Directly upload files to the server.
#[derive(StructOpt)]
#[structopt(after_help(
    "\
Copies/uploads the named file or directory from the local filesystem to the \
server.

This behaves roughly similarly to `cp -a`, in that it will copy files \
recursively with attributes, and doesn't treat symlinks specially.

If <dst> does not start with `/`, it is relative to `server_root` value \
in the configuration. Otherwise, it starts from the physical root of the \
server."
))]
struct PutSubcommand {
    #[structopt(flatten)]
    config: ConfigArg,

    #[structopt(flatten)]
    verbosity: VerbosityArgs,

    /// Allow overwriting existing files.
    #[structopt(short, long)]
    force: bool,

    /// Path of the file or directory to upload to the server.
    #[structopt(parse(from_os_str))]
    src: PathBuf,

    /// Path on the server to which to upload the file.
    ///
    /// If omitted, upload to a file of the same name in the logical root named
    /// in the configuration.
    #[structopt(parse(from_os_str))]
    dst: Option<PathBuf>,
}

/// Directly delete files or directory trees on the server.
#[derive(StructOpt)]
#[structopt(after_help(
    "\
Deletes the named file(s) from the server.

Like `rm`, this does not remove directories by default. `ensync rmdir` can be \
used to remove known-empty directories. Alternatively, the `-r` flag can be \
passed to `ensync rm` to recursively remove whole directory trees.

If <path> does not start with `/`, it is relative to `server_root` value \
in the configuration. Otherwise, it starts from the physical root of the \
server.

The main use for this command is to remove unwanted logical roots. For \
example, if you had a `myroot` logical root you no longer wanted, you could \
remove it with

        ensync rm -r /path/to/config /myroot"
))]
struct RmSubcommand {
    #[structopt(flatten)]
    config: ConfigArg,

    #[structopt(flatten)]
    verbosity: VerbosityArgs,

    /// Delete directories recursively.
    #[structopt(short, long)]
    recursive: bool,

    /// The path(s) to delete.
    #[structopt(required = true, parse(from_os_str))]
    path: Vec<PathBuf>,
}

/// Run the server-side component.
#[derive(StructOpt)]
#[structopt(after_help(
    "\
Runs the server-side component, for use in `shell:...` `server` \
configurations.

<path> is the directory where data will be stored. It and its parents \
will be created automatically as necessary. If the directory already existed, \
the permissions of that directory will be used for all files and directories \
created within it (except that regular files have the execute bits cleared). \
Otherwise, <path> is created with permissions according to umask and those \
permissions are used for new files.

This command is not for interactive use. It expects to receive on standard \
input and send on standard output the binary protocol that it uses to \
communicate with the client ensync process.

Also note that this is not some kind of daemon server process. It \
handles exactly one client, the one connected on its standard input/output \
pipe."
))]
struct ServerSubcommand {
    /// Path to server-side storage.
    #[structopt(parse(from_os_str))]
    path: PathBuf,
}

#[derive(StructOpt)]
struct ConfigArg {
    /// Path to ensync configuration and local data.
    #[structopt(parse(from_os_str))]
    config: PathBuf,

    /// If set, any time a passphrase would be obtained according to the
    /// configuration, use this value instead.
    /// This argument is in the same format as the config. e.g., `prompt` or
    /// `file:/some/path`.
    #[structopt(short, long)]
    key: Option<PassphraseConfig>,
}

#[derive(StructOpt)]
struct VerbosityArgs {
    /// Be more verbose.
    #[structopt(short, parse(from_occurrences))]
    verbose: i32,
    /// Be less verbose.
    #[structopt(short, parse(from_occurrences))]
    quiet: i32,
}

impl VerbosityArgs {
    fn is_verbose(&self) -> bool {
        self.verbose > self.quiet
    }
}

#[derive(Default, Clone, Copy)]
struct NonVerbose;
impl NonVerbose {
    fn is_verbose(&self) -> bool {
        false
    }
}

#[derive(StructOpt)]
struct OldKeyArg {
    /// Use this value instead of `passphrase` from the config to get the old
    /// passphrase. This argument is in the same format as the config. e.g.,
    /// `prompt` or `file:/some/path`.
    #[structopt(short, long)]
    old: Option<PassphraseConfig>,
}

#[derive(StructOpt)]
struct NewKeyArg {
    /// Use this value instead of `passphrase` from the config to get the new
    /// passphrase. This argument is in the same format as the config. e.g.,
    /// `prompt` or `file:/some/path`.
    #[structopt(short, long, default_value = "prompt")]
    new: PassphraseConfig,
}

#[derive(StructOpt)]
struct FromKeyArg {
    /// Use this value instead of `passphrase` from the config to get the
    /// passphrase of the key which has the groups to be granted. This argument
    /// is in the same format as the config. e.g., `prompt` or
    /// `file:/some/path`.
    #[structopt(short, long)]
    from: Option<PassphraseConfig>,
}

#[derive(StructOpt)]
struct ToKeyArg {
    /// Use this value instead of `passphrase` from the config to get the
    /// passphrase of the key which shall receive the groups to be granted.
    /// This argument is in the same format as the config. e.g., `prompt` or
    /// `file:/some/path`.
    #[structopt(short, long, default_value = "prompt")]
    to: PassphraseConfig,
}

#[derive(StructOpt)]
struct RootKeyArg {
    /// Specify how to get a passphrase in the `root` group if none of the
    /// operating keys are in that group.
    #[structopt(short, long, default_value = "prompt")]
    root: PassphraseConfig,
}

fn main_impl() -> Result<()> {
    use std::env;
    use std::fs;

    {
        let mut args = env::args().fuse();
        // If invoked as the login shell, go to a special server-only
        // code-path.
        if args.next().map_or(false, |arg0| arg0.starts_with("-")) {
            return cli::cmd_server::shell();
        }
        // If the second argument is `-c`, assume we're being run as the shell
        // by ssh.
        if args.next().map_or(false, |arg1| "-c" == &arg1) {
            return cli::cmd_server::shell();
        }
    }

    fn create_storage(
        verbose: bool,
        config: &cli::config::Config,
    ) -> errors::Result<std::sync::Arc<dyn server::Storage>> {
        let storage =
            cli::open_server::open_server_storage(&config.server, verbose)?;
        fs::create_dir_all(&config.private_root).chain_err(|| {
            format!(
                "Failed to create ensync private directory '{}'",
                config.private_root.display()
            )
        })?;
        Ok(storage)
    }

    let command = Command::from_args();

    macro_rules! set_up {
        ($sc:ident, $config:ident) => {
            let mut $config = cli::config::Config::read(&$sc.config.config)
                .chain_err(|| "Could not read configuration")?;
            if let Some(ref key) = $sc.config.key {
                $config.passphrase = key.to_owned();
            }
        };

        ($sc:ident, $config:ident, $storage:ident) => {
            set_up!($sc, $config);
            let $storage =
                create_storage($sc.verbosity.is_verbose(), &$config)?;
        };

        ($sc:ident, $config:ident, $storage:ident, $replica:ident) => {
            set_up!($sc, $config, $storage);
            let $replica = cli::open_server::open_server_replica(
                &$config,
                $storage.clone(),
                None,
            )?;
        };
    }

    macro_rules! passphrase_or_config {
        ($v:expr, $config:expr) => {
            if let Some(ref c) = $v {
                c.to_owned()
            } else {
                $config.passphrase.clone()
            }
        };
    }

    match command {
        Command::Server(sc) => cli::cmd_server::run(sc.path),

        Command::Key(KeySubcommand::Init(sc)) => {
            set_up!(sc, config, storage);
            cli::cmd_keymgmt::init_keys(&config, &*storage, &sc.key_name)
        }

        Command::Key(KeySubcommand::Add(sc)) => {
            set_up!(sc, config, storage);
            let old = passphrase_or_config!(sc.old.old, config);
            cli::cmd_keymgmt::add_key(
                &*storage,
                &old,
                &sc.new.new,
                &sc.root.root,
                &sc.key_name,
            )
        }

        Command::Key(KeySubcommand::Ls(sc)) => {
            set_up!(sc, config, storage);
            cli::cmd_keymgmt::list_keys(&*storage)
        }

        Command::Key(KeySubcommand::Change(sc)) => {
            set_up!(sc, config, storage);
            let old = passphrase_or_config!(sc.old.old, config);
            cli::cmd_keymgmt::change_key(
                &config,
                &*storage,
                &old,
                &sc.new.new,
                &sc.root.root,
                sc.key_name.as_deref(),
                sc.force,
            )
        }

        Command::Key(KeySubcommand::Rm(sc)) => {
            set_up!(sc, config, storage);
            cli::cmd_keymgmt::del_key(&*storage, &sc.key_name, &sc.root.root)
        }

        Command::Key(KeySubcommand::Group(KeyGroupSubcommand::Create(sc))) => {
            set_up!(sc, config, storage);
            cli::cmd_keymgmt::create_group(
                &*storage,
                &config.passphrase,
                &sc.root.root,
                sc.group.into_iter(),
            )
        }

        Command::Key(KeySubcommand::Group(KeyGroupSubcommand::Assoc(sc))) => {
            set_up!(sc, config, storage);
            let from = passphrase_or_config!(sc.from.from, config);
            cli::cmd_keymgmt::assoc_group(
                &*storage,
                &from,
                &sc.to.to,
                &sc.root.root,
                sc.group.into_iter(),
            )
        }

        Command::Key(KeySubcommand::Group(KeyGroupSubcommand::Disassoc(
            sc,
        ))) => {
            set_up!(sc, config, storage);
            cli::cmd_keymgmt::disassoc_group(
                &*storage,
                &sc.key_name,
                &sc.root.root,
                sc.group.into_iter(),
            )
        }

        Command::Key(KeySubcommand::Group(KeyGroupSubcommand::Destroy(sc))) => {
            set_up!(sc, config, storage);
            cli::cmd_keymgmt::destroy_group(
                &*storage,
                sc.yes,
                &sc.root.root,
                sc.group.into_iter(),
            )
        }

        Command::Setup(sc) => cli::cmd_setup::run(
            &sc.key,
            sc.config,
            sc.local_path,
            sc.remote_path,
        ),

        Command::Sync(sc) => {
            set_up!(sc, config);

            let num_threads =
                sc.threads.unwrap_or_else(|| num_cpus::get() as u32 + 2);
            if 0 == num_threads {
                return Err("Thread count must be at least 1".into());
            }

            let json_status_out =
                sc.json_status_fd.map(|fd| unsafe { File::from_raw_fd(fd) });

            fn do_run(
                sc: &SyncSubcommand,
                config: &cli::config::Config,
                json_status_out: Option<File>,
                num_threads: u32,
                key_chain: &mut Option<std::sync::Arc<server::KeyChain>>,
            ) -> errors::Result<()> {
                let storage =
                    create_storage(sc.verbosity.is_verbose(), config)?;

                cli::cmd_sync::run(
                    config,
                    storage,
                    sc.verbosity.verbose,
                    sc.verbosity.quiet,
                    sc.itemise,
                    sc.itemise_unchanged,
                    json_status_out,
                    &sc.colour,
                    &sc.spin,
                    sc.include_ancestors,
                    sc.dry_run,
                    sc.watch.then(|| sc.quiescence),
                    num_threads,
                    &sc.strategy,
                    sc.override_mode,
                    key_chain,
                )
            }

            let mut key_chain = None;

            loop {
                use std::io::{stderr, Write};

                match do_run(
                    &sc,
                    &config,
                    json_status_out.as_ref().map(|j| j.try_clone().expect("failed to clone FD")),
                    num_threads,
                    &mut key_chain,
                ) {
                    Ok(()) => return Ok(()),
                    Err(e) => {
                        if let Some(seconds) = sc.reconnect {
                            interrupt::clear_notify();

                            let _ = writeln!(
                                stderr(),
                                "Connection terminated due to error: {}\n\
                             Attempting reconnect in {} seconds...",
                                e,
                                seconds
                            );
                            ::std::thread::sleep(::std::time::Duration::new(
                                seconds, 0,
                            ));
                            // Continue loop
                        } else {
                            return Err(e);
                        }
                    }
                }
            }
        }

        Command::Ls(sc) => {
            set_up!(sc, config, storage, replica);
            cli::cmd_manual::ls(
                &replica,
                sc.path.iter(),
                sc.path.len() > 1,
                sc.human_readable,
            )
        }

        Command::Mkdir(sc) => {
            set_up!(sc, config, storage, replica);
            cli::cmd_manual::mkdir(
                &replica,
                sc.path.iter(),
                parse_mode(&sc.mode)?,
            )
        }

        Command::Rmdir(sc) => {
            set_up!(sc, config, storage, replica);
            cli::cmd_manual::rmdir(&replica, sc.path.iter())
        }

        Command::Cat(sc) => {
            set_up!(sc, config, storage, replica);
            cli::cmd_manual::cat(&replica, sc.path.iter())
        }

        Command::Get(sc) => {
            set_up!(sc, config, storage, replica);
            cli::cmd_manual::get(
                &replica,
                sc.src,
                sc.dst,
                sc.force,
                sc.verbosity.is_verbose(),
            )
        }

        Command::Put(sc) => {
            set_up!(sc, config, storage, replica);
            cli::cmd_manual::put(
                &replica,
                sc.src,
                sc.dst.unwrap_or(PathBuf::new()),
                sc.force,
                sc.verbosity.is_verbose(),
            )
        }

        Command::Rm(sc) => {
            set_up!(sc, config, storage, replica);
            cli::cmd_manual::rm(
                &replica,
                sc.path.iter(),
                sc.recursive,
                sc.verbosity.is_verbose(),
            )
        }
    }
}

fn parse_mode(s: &str) -> Result<defs::FileMode> {
    u32::from_str_radix(s, 8)
        .map(|m| m & 0o7777)
        .map_err(|_| format!("Invalid mode '{}'", s).into())
}
