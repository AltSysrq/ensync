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
#[macro_use] extern crate fourleaf;
#[macro_use] extern crate error_chain;
#[macro_use] extern crate lazy_static;
#[macro_use] extern crate quick_error;

#[cfg(test)] #[macro_use] extern crate proptest;

mod defs;
mod errors;
mod interrupt;
mod sql;
mod work_stack;

mod rules;
mod log;
mod replica;
#[cfg(test)] mod memory_replica;
mod dry_run_replica;
mod reconcile;
mod block_xfer;
mod ancestor;
mod posix;
mod server;
mod cli;

use crate::errors::{Result, ResultExt};

fn main() {
    use std::io::{Write, stderr};
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

fn main_impl() -> Result<()> {
    use std::env;
    use std::fs;

    use clap::*;

    use crate::cli::config::PassphraseConfig;

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

    let config_arg = Arg::with_name("config")
        .help("Path to Ensync configuration and local data")
        .required(true);
    let old_key_arg = Arg::with_name("old-key")
        .required(false)
        .takes_value(true)
        .short("o")
        .long("old")
        .help("Use this value instead of `passphrase` from \
               the config to get the old passphrase. This \
               argument is in the same format as the config, \
               i.e., `prompt`, `file:/some/path`, etc.");
    let alt_key_arg = Arg::with_name("key")
        .required(false)
        .takes_value(true)
        .short("k")
        .long("key")
        .help("Use this value instead of `passphrase` from \
               the config to get old passphrase. This \
               argument is in the same format as the config, \
               i.e., `prompt`, `file:/some/path`, etc.");
    let from_key_arg = Arg::with_name("from-key")
        .required(false)
        .takes_value(true)
        .short("f")
        .long("from")
        .help("Use this value instead of `passphrase` from \
               the config to get the passphrase of the key which has the \
               groups to be granted. This \
               argument is in the same format as the config, \
               i.e., `prompt`, `file:/some/path`, etc.");
    let new_key_arg = Arg::with_name("new-key")
        .required(false)
        .takes_value(true)
        .short("n")
        .long("new")
        .help("Specify how to get the new passphrase, like \
               with the `passphrase` line in the config")
        .default_value("prompt");
    let to_key_arg = Arg::with_name("to-key")
        .required(false)
        .takes_value(true)
        .short("t")
        .long("to")
        .help("Specify how to get the passphrase of the key to which \
               the groups are to be granted, like \
               with the `passphrase` line in the config")
        .default_value("prompt");
    let root_key_arg = Arg::with_name("root-key")
        .required(false)
        .takes_value(true)
        .short("r")
        .long("root")
        .help("Specify how to get a passphrase in the `root` group if \
               none of the operating keys are in that group")
        .default_value("prompt");
    let simple_verbose_arg = Arg::with_name("verbose")
        .required(false)
        .takes_value(false)
        .short("v")
        .help("Be verbose");

    let matches = App::new("ensync")
        .author(crate_authors!("\n"))
        .about("Encrypted bidirectional file synchroniser")
        .version(crate_version!())
        .max_term_width(100)
        .setting(AppSettings::DontCollapseArgsInUsage)
        .setting(AppSettings::DeriveDisplayOrder)
        .setting(AppSettings::SubcommandRequiredElseHelp)
        .setting(AppSettings::UnifiedHelpMessage)
        .setting(AppSettings::VersionlessSubcommands)
        .subcommand(SubCommand::with_name("setup")
                    .about("Wizard to set up simple ensync configurations")
                    .max_term_width(100)
                    .setting(AppSettings::DontCollapseArgsInUsage)
                    .arg(&config_arg)
                    .arg(Arg::with_name("key")
                         .required(false)
                         .takes_value(true)
                         .short("k")
                         .long("key")
                         .default_value("prompt")
                         .help("\
How to get the passphrase. Defaults to `prompt` to read it interactively. Use \
`string:xxx` to use a fixed value, `file:/some/path` to read it from a file, \
or `shell:some shell command` to use the output of a shell command."))
                    .arg(Arg::with_name("local-path")
                         .required(true)
                         .help("Path in the local filesystem of files to be \
                                synced. Must be an already existing \
                                directory."))
                    .arg(Arg::with_name("remote-path")
                         .required(true)
                         .help("Where to write encrypted data. This may be \
                                a local path (e.g., `/path/to/files`) or an \
                                scp-style argument (e.g., \
                                `user@host:/path/to/files`)."))
                    .after_help("\
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
exist."))
        .subcommand(SubCommand::with_name("sync")
                    .about("Sync files according to configuration")
                    .max_term_width(100)
                    .setting(AppSettings::DontCollapseArgsInUsage)
                    .arg(&config_arg)
                    .arg(Arg::with_name("verbose")
                         .short("v")
                         .required(false)
                         .multiple(true)
                         .takes_value(false)
                         .help("Be more verbose"))
                    .arg(Arg::with_name("quiet")
                         .short("q")
                         .required(false)
                         .multiple(true)
                         .takes_value(false)
                         .help("Be less verbose"))
                    .arg(Arg::with_name("itemise")
                         .long("itemise")
                         .alias("itemize")
                         .short("i")
                         .required(false)
                         .takes_value(false)
                         .help("Output rsync-like itemisation to stdout"))
                    .arg(Arg::with_name("itemise-unchanged")
                         .long("itemise-unchanged")
                         .alias("itemize-unchanged")
                         .required(false)
                         .takes_value(false)
                         .help("With --itemise, also include unchanged items"))
                    .arg(Arg::with_name("include-ancestors")
                         .long("include-ancestors")
                         .required(false)
                         .takes_value(false)
                         .help("Log happenings in the internal ancestor \
                                replica as well"))
                    .arg(Arg::with_name("threads")
                         .long("threads")
                         .required(false)
                         .takes_value(true)
                         .help("Specify the number of threads to use \
                                [default: CPUs + 2]"))
                    .arg(Arg::with_name("colour")
                         .long("colour")
                         .alias("color")
                         .required(false)
                         .takes_value(true)
                         .default_value("auto")
                         .possible_values(&["auto", "always", "never"])
                         .help("Control colourisation of stderr log"))
                    .arg(Arg::with_name("spin")
                         .long("spin")
                         .alias("spinner")
                         .required(false)
                         .takes_value(true)
                         .default_value("auto")
                         .possible_values(&["auto", "always", "never"])
                         .help("Control whether the progress spinner \
                                is shown"))
                    .arg(Arg::with_name("dry-run")
                         .short("n")
                         .long("dry-run")
                         .required(false)
                         .takes_value(false)
                         .help("Don't actually make any changes"))
                    .arg(Arg::with_name("watch")
                         .short("w")
                         .long("watch")
                         .required(false)
                         .conflicts_with("dry-run")
                         .takes_value(false)
                         .help("When done syncing, continue running and \
                                monitor for file changes and sync when \
                                detected. Note that these syncs typically \
                                happen 30 to 60 seconds after the file \
                                changes. This requires extra resources \
                                both locally and on the server."))
                    .arg(Arg::with_name("strategy")
                         .long("strategy")
                         .required(false)
                         .takes_value(true)
                         .default_value("auto")
                         .possible_values(&["fast", "clean", "scrub", "auto"])
                         .help("Control what is checked for syncing. \"fast\" \
                                means to only scan directories that have \
                                obviously changed. \"clean\" causes all \
                                directories to be scanned. \"scrub\" \
                                additionally causes all cached hashes to be \
                                purged (this will make the sync very slow)."))
                    .arg(Arg::with_name("reconnect")
                         .long("reconnect")
                         .required(false)
                         .takes_value(true)
                         .requires("watch")
                         .help("In conjunction with --watch, if an error \
                                occurs, back off for the given number of \
                                seconds, then attempt to restart the server \
                                process and resume operations. If this flag \
                                is given, `ensync sync` should not terminate \
                                on its own."))
                    .arg(Arg::with_name("override-mode")
                         .long("override-mode")
                         .required(false)
                         .takes_value(true)
                         .help("Ignore all sync rules in the configuration \
                                sync all files with the given sync mode. \
                                This is most useful in conjunction with \
                                the `reset-server` and `reset-client` mode \
                                aliases. If `--strategy` is `auto`, \
                                implies `--strategy=clean`."))
                    .after_help("\
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
but may leave stray temporary files or corrupt the cache."))
        .subcommand(
            SubCommand::with_name("key")
            .setting(AppSettings::SubcommandRequiredElseHelp)
            .about("Perform key management operations")
            .max_term_width(100)
            .subcommand(SubCommand::with_name("init")
                        .about("Initialise the key store")
                        .max_term_width(100)
                        .setting(AppSettings::DontCollapseArgsInUsage)
                        .arg(&config_arg)
                        .arg(Arg::with_name("key-name")
                             .help("Name for the first key in the key store")
                             .required(false))
                        .after_help("\
This command is used to initialise the key store in the server. It generates \
a new internal key set and associates one user key withthem, corresponding \
to the passphrase defined in the configuration. You should specify the key \
name if you plan on using multiple keys.This cannot be used after the key \
store hasbeen initialised; see `key add` for that instead."))
            .subcommand(SubCommand::with_name("add")
                        .about("Add a new key to the key store")
                        .max_term_width(100)
                        .setting(AppSettings::DontCollapseArgsInUsage)
                        .arg(&config_arg)
                        .arg(&old_key_arg)
                        .arg(&new_key_arg)
                        .arg(&root_key_arg)
                        .arg(Arg::with_name("key-name")
                             .required(true)
                             .help("The name of the key to add"))
                        .after_help("\
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
used to use other passphrase methods."))
            .subcommand(SubCommand::with_name("change")
                        .about("Change a key in the key store")
                        .max_term_width(100)
                        .setting(AppSettings::DontCollapseArgsInUsage)
                        .arg(&config_arg)
                        .arg(&old_key_arg)
                        .arg(&new_key_arg)
                        .arg(&root_key_arg)
                        .arg(Arg::with_name("key-name")
                             .required(false)
                             .help("The name of the key to edit"))
                        .arg(Arg::with_name("force")
                             .long("force")
                             .short("f")
                             .takes_value(false)
                             .required(false)
                             .help("Change <key-name> even if the old \
                                    passphrase does not correspond to \
                                    that key"))
                        .after_help("\
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
desired, there is some information on how to do this in the documentation."))
            .subcommand(SubCommand::with_name("rm")
                        .alias("del")
                        .about("Delete a key from the key store")
                        .max_term_width(100)
                        .setting(AppSettings::DontCollapseArgsInUsage)
                        .arg(&config_arg)
                        .arg(&root_key_arg)
                        .arg(Arg::with_name("key-name")
                             .required(true)
                             .help("The name of the key to delete"))
                        .after_help("\
This command deletes the named key from the key store.

It is not legal to delete the last key from the key store. A key also may not \
be deleted if it is the last member of any particular key group, as doing so \
would destroy the group.

Since this operation modifies the key store, a key in the `root` group is \
required. By default, this prompts the terminal, but the `--root` argument \
can be used to use other passphrase methods. Note that you cannot use the \
passphrase of the key being deleted for this, even if that key is in the \
`root` group."))
            .subcommand(SubCommand::with_name("ls")
                        .alias("list")
                        .about("List the keys in the key store")
                        .max_term_width(100)
                        .setting(AppSettings::DontCollapseArgsInUsage)
                        .arg(&config_arg))
            .subcommand(
                SubCommand::with_name("group")
                .setting(AppSettings::SubcommandRequiredElseHelp)
                .about("Manage key groups")
                .max_term_width(100)
                .subcommand(SubCommand::with_name("create")
                            .max_term_width(100)
                            .setting(AppSettings::DontCollapseArgsInUsage)
                            .about("Create key group(s)")
                            .arg(&config_arg)
                            .arg(&alt_key_arg)
                            .arg(&root_key_arg)
                            .arg(Arg::with_name("group")
                                 .required(true)
                                 .multiple(true)
                                 .help("The name(s) of the \
                                        group(s) to create"))
                            .after_help("\
Creates one or more key groups. All groups will initially be granted \
to the key that was derived from the passphrase.

By default, this applies to the passphrase obtained as described by \
the configuration. The `--key` argument can be used to override this.

Since this operation modifies the key store, a key in the `root` group is \
required. If the existing key is in the `root` group, it will be used to \
do this implicitly. Otherwise, a separate key will need to be provided. \
By default, this prompts the terminal, but the `--root` argument can be \
used to use other passphrase methods."))
                .subcommand(SubCommand::with_name("assoc")
                            .max_term_width(100)
                            .setting(AppSettings::DontCollapseArgsInUsage)
                            .about("Associate a key with key group(s)")
                            .arg(&config_arg)
                            .arg(&from_key_arg)
                            .arg(&to_key_arg)
                            .arg(&root_key_arg)
                            .arg(Arg::with_name("group")
                                 .required(true)
                                 .multiple(true)
                                 .help("The name(s) of the group(s) to grant"))
                            .after_help("\
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
can be used to use other passphrase methods."))
                .subcommand(SubCommand::with_name("disassoc")
                            .alias("dissoc")
                            .max_term_width(100)
                            .setting(AppSettings::DontCollapseArgsInUsage)
                            .about("Disassociate a key from key group(s)")
                            .arg(&config_arg)
                            .arg(&root_key_arg)
                            .arg(Arg::with_name("key")
                                 .required(true)
                                 .help("The key from which to remove the \
                                        listed group(s)"))
                            .arg(Arg::with_name("group")
                                 .required(true)
                                 .multiple(true)
                                 .help("The name(s) of the group(s) to \
                                        remove"))
                            .after_help("\
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
even after this operation has been performed."))
                .subcommand(SubCommand::with_name("destroy")
                            .max_term_width(100)
                            .setting(AppSettings::DontCollapseArgsInUsage)
                            .about("Destroy key group(s)")
                            .arg(&config_arg)
                            .arg(&root_key_arg)
                            .arg(Arg::with_name("yes")
                                 .required(false)
                                 .takes_value(false)
                                 .short("y")
                                 .long("yes")
                                 .help("Don't prompt for confirmation"))
                            .arg(Arg::with_name("group")
                                 .required(true)
                                 .multiple(true)
                                 .help("The name(s) of the group(s) to \
                                        destroy"))
                            .after_help("\
Destroys the listed groups, implicitly disassociating them from any and all \
keys.

The `root` and `everyone` key groups may not be destroyed.

WARNING: This is an irreversible operation which will render any data \
protected by the destroyed key groups irrecoverable.

Since this operation modifies the key store, a key in the `root` group is \
required. By default, this prompts the terminal, but the `--root` argument \
can be used to use other passphrase methods."))))
        .subcommand(SubCommand::with_name("ls")
                    .alias("dir")
                    .about("List directory contents on server")
                    .max_term_width(100)
                    .setting(AppSettings::DontCollapseArgsInUsage)
                    .arg(&config_arg)
                    .arg(Arg::with_name("path")
                         .multiple(true)
                         .required(true)
                         .help("The path(s) to list"))
                    .arg(Arg::with_name("human-readable")
                         .required(false)
                         .takes_value(false)
                         .short("h")
                         .help("Display sizes in human-readable format"))
                    .after_help("\
Lists the content of each given directory (or individual file) on the server.

If <path> does not start with `/`, it is relative to the `server_root` value \
in the configuration. Otherwise, it starts from the physical root of the \
server."))
        .subcommand(SubCommand::with_name("mkdir")
                    .alias("md")
                    .about("Directly create a directory on the server")
                    .max_term_width(100)
                    .setting(AppSettings::DontCollapseArgsInUsage)
                    .arg(&config_arg)
                    .arg(Arg::with_name("mode")
                         .long("mode")
                         .short("m")
                         .takes_value(true)
                         .required(false)
                         .default_value("0700")
                         .help("UNIX permissions for the new directory"))
                    .arg(Arg::with_name("path")
                         .multiple(true)
                         .required(true)
                         .help("The path(s) to create"))
                    .after_help("\
Creates a directory entry on the server.

If <path> does not start with `/`, it is relative to `server_root` value \
in the configuration. Otherwise, it starts from the physical root of the \
server.

The main use of this is to manually create logical sync roots. For example, \
the below would be used to create the `myroot` logical root (i.e., \
`server_root = \"myroot\"` in the configuration):

        ensync mkdir /path/to/config /myroot

The UNIX permissions do not control access to content on the server; they \
are only used when a server directory is propagated to the local filesystem."))
        .subcommand(SubCommand::with_name("rmdir")
                    .alias("rd")
                    .about("Remove an empty directory on the server")
                    .max_term_width(100)
                    .setting(AppSettings::DontCollapseArgsInUsage)
                    .arg(&config_arg)
                    .arg(Arg::with_name("path")
                         .multiple(true)
                         .required(true)
                         .help("The path(s) to remove"))
                    .after_help("\
Removes the listed directory(ies), which must be empty. To remove non-empty \
directories, use `rm` with the `-r` flag.

If <path> does not start with `/`, it is relative to `server_root` value \
in the configuration. Otherwise, it starts from the physical root of the \
server."))
        .subcommand(SubCommand::with_name("cat")
                    .alias("type")
                    .alias("dump")
                    .about("Dump file contents to standard output")
                    .max_term_width(100)
                    .setting(AppSettings::DontCollapseArgsInUsage)
                    .arg(&config_arg)
                    .arg(Arg::with_name("path")
                         .multiple(true)
                         .required(true)
                         .help("The path(s) to dump"))
                    .after_help("\
Dumps the raw content of all listed files to standard output, without any \
other information. Every path must be a regular file (symlinks are not \
permitted).

If <path> does not start with `/`, it is relative to `server_root` value \
in the configuration. Otherwise, it starts from the physical root of the \
server."))
        .subcommand(SubCommand::with_name("get")
                    .about("Directly fetch files from server")
                    .max_term_width(100)
                    .setting(AppSettings::DontCollapseArgsInUsage)
                    .arg(&config_arg)
                    .arg(Arg::with_name("src")
                         .required(true)
                         .help("Path of the file to fetch from the server"))
                    .arg(Arg::with_name("dst")
                         .required(false)
                         .default_value(".")
                         .help("Location to which to copy the files"))
                    .arg(Arg::with_name("force")
                         .required(false)
                         .takes_value(false)
                         .short("f")
                         .long("force")
                         .help("Allow overwriting existing files"))
                    .arg(&simple_verbose_arg)
                    .after_help("\
Fetches the named file or directory tree from the server, copying it to the \
local filesystem.

This behaves roughly similarly to `cp -a`, in that it will copy files \
recursively with attributes, and doesn't treat symlinks specially.

If <src> does not start with `/`, it is relative to `server_root` value \
in the configuration. Otherwise, it starts from the physical root of the \
server."))
        .subcommand(SubCommand::with_name("put")
                    .about("Directly upload files to server")
                    .max_term_width(100)
                    .setting(AppSettings::DontCollapseArgsInUsage)
                    .arg(&config_arg)
                    .arg(Arg::with_name("src")
                         .required(true)
                         .help("Path of the file to upload to the server"))
                    .arg(Arg::with_name("dst")
                         .required(false)
                         .help("Path on the server to which to upload \
                                the file. If omitted, upload to a file \
                                of the same name in the logical root \
                                named in the configuration."))
                    .arg(Arg::with_name("force")
                         .required(false)
                         .takes_value(false)
                         .short("f")
                         .long("force")
                         .help("Allow overwriting existing files"))
                    .arg(&simple_verbose_arg)
                    .after_help("\
Copies/uploads the named file or directory from the local filesystem to the \
server.

This behaves roughly similarly to `cp -a`, in that it will copy files \
recursively with attributes, and doesn't treat symlinks specially.

If <dst> does not start with `/`, it is relative to `server_root` value \
in the configuration. Otherwise, it starts from the physical root of the \
server."))
        .subcommand(SubCommand::with_name("rm")
                    .alias("del")
                    .about("Directly delete files or directory trees \
                            on the server")
                    .max_term_width(100)
                    .setting(AppSettings::DontCollapseArgsInUsage)
                    .arg(&config_arg)
                    .arg(Arg::with_name("path")
                         .required(true)
                         .multiple(true)
                         .help("The path(s) to delete"))
                    .arg(Arg::with_name("recursive")
                         .required(false)
                         .takes_value(false)
                         .short("r")
                         .long("recursive")
                         .help("Delete directories recursively"))
                    .arg(&simple_verbose_arg)
                    .after_help("\
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

        ensync rm -r /path/to/config /myroot"))
        .subcommand(SubCommand::with_name("server")
                    .about("Run the server-side component")
                    .max_term_width(100)
                    .setting(AppSettings::DontCollapseArgsInUsage)
                    .arg(Arg::with_name("path")
                         .help("Path to server-side storage")
                         .required(true))
                    .after_help("\
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
pipe."))
        .get_matches();

    fn create_storage(matches: &ArgMatches, config: &cli::config::Config)
                      -> errors::Result<std::sync::Arc<dyn server::Storage>> {
        let storage = cli::open_server::open_server_storage(
            &config.server,
            matches.occurrences_of("quiet") <=
                matches.occurrences_of("verbose"))?;
        fs::create_dir_all(&config.private_root).chain_err(
            || format!("Failed to create ensync private directory '{}'",
                       config.private_root.display()))?;
        Ok(storage)
    }

    macro_rules! set_up {
        ($matches:ident, $config:ident) => {
            let $config = cli::config::Config::read(
                $matches.value_of("config").unwrap())
                .chain_err(|| "Could not read configuration")?;
        };

        ($matches:ident, $config:ident, $storage:ident) => {
            set_up!($matches, $config);
            let $storage = create_storage(&$matches, &$config)?;
        };

        ($matches:ident, $config:ident, $storage:ident, $replica:ident) => {
            set_up!($matches, $config, $storage);
            let $replica = cli::open_server::open_server_replica(
                &$config, $storage.clone(), None)?;
        };
    }

    macro_rules! passphrase_or_config {
        ($matches:expr, $config:expr, $key:expr) => {
            if let Some(c) = $matches.value_of($key) {
                c.parse::<PassphraseConfig>()?
            } else {
                $config.passphrase.clone()
            }
        };
    }
    macro_rules! passphrase_or_prompt {
        ($matches:expr, $key:expr) => {
            if let Some(c) = $matches.value_of($key) {
                c.parse::<PassphraseConfig>()?
            } else {
                PassphraseConfig::Prompt
            }
        };
    }

    if let Some(matches) = matches.subcommand_matches("server") {
        cli::cmd_server::run(matches.value_of("path").unwrap())
    } else if let Some(matches) = matches.subcommand_matches("key") {
        if let Some(matches) = matches.subcommand_matches("init") {
            set_up!(matches, config, storage);
            cli::cmd_keymgmt::init_keys(&config, &*storage,
                                        matches.value_of("key-name"))
        } else if let Some(matches) = matches.subcommand_matches("add") {
            set_up!(matches, config, storage);
            let old = passphrase_or_config!(matches, config, "old-key");
            let new = passphrase_or_prompt!(matches, "new-key");
            let root = passphrase_or_prompt!(matches, "root-key");
            cli::cmd_keymgmt::add_key(&*storage, &old, &new, &root,
                                      matches.value_of("key-name").unwrap())
        } else if let Some(matches) = matches.subcommand_matches("ls") {
            set_up!(matches, config, storage);
            cli::cmd_keymgmt::list_keys(&*storage)
        } else if let Some(matches) = matches.subcommand_matches("change") {
            set_up!(matches, config, storage);
            let old = passphrase_or_config!(matches, config, "old-key");
            let new = passphrase_or_prompt!(matches, "new-key");
            let root = passphrase_or_prompt!(matches, "root-key");
            cli::cmd_keymgmt::change_key(&config, &*storage, &old, &new, &root,
                                         matches.value_of("key-name"),
                                         matches.is_present("force"))
        } else if let Some(matches) = matches.subcommand_matches("rm") {
            set_up!(matches, config, storage);
            let root = passphrase_or_prompt!(matches, "root-key");
            cli::cmd_keymgmt::del_key(
                &*storage, matches.value_of("key-name").unwrap(), &root)
        } else if let Some(matches) = matches.subcommand_matches("group") {
            if let Some(matches) = matches.subcommand_matches("create") {
                set_up!(matches, config, storage);
                let key = passphrase_or_config!(matches, config, "key");
                let root = passphrase_or_prompt!(matches, "root-key");
                cli::cmd_keymgmt::create_group(
                    &*storage, &key, &root, matches.values_of("group").unwrap())
            } else if let Some(matches) = matches.subcommand_matches("assoc") {
                set_up!(matches, config, storage);
                let from = passphrase_or_config!(matches, config, "from-key");
                let to = passphrase_or_prompt!(matches, "to-key");
                let root = passphrase_or_prompt!(matches, "root-key");
                cli::cmd_keymgmt::assoc_group(
                    &*storage, &from, &to, &root,
                    matches.values_of("group").unwrap())
            } else if let Some(matches) = matches.subcommand_matches(
                "disassoc")
            {
                set_up!(matches, config, storage);
                let root = passphrase_or_prompt!(matches, "root-key");
                cli::cmd_keymgmt::disassoc_group(
                    &*storage, matches.value_of("key").unwrap(), &root,
                    matches.values_of("group").unwrap())
            } else if let Some(matches) = matches.subcommand_matches(
                "destroy")
            {
                set_up!(matches, config, storage);
                let root = passphrase_or_prompt!(matches, "root-key");
                cli::cmd_keymgmt::destroy_group(
                    &*storage, matches.is_present("yes"), &root,
                    matches.values_of("group").unwrap())
            } else {
                panic!("Unhandled `key group` subcommand: {}",
                       matches.subcommand_name().unwrap());
            }
        } else {
            panic!("Unhandled `key` subcommand: {}",
                   matches.subcommand_name().unwrap());
        }
    } else if let Some(matches) = matches.subcommand_matches("setup") {
        let passphrase = passphrase_or_prompt!(matches, "key");
        cli::cmd_setup::run(&passphrase,
                            matches.value_of("config").unwrap(),
                            matches.value_of("local-path").unwrap(),
                            matches.value_of("remote-path").unwrap())
    } else if let Some(matches) = matches.subcommand_matches("sync") {
        set_up!(matches, config);

        let num_threads = if let Some(threads) = matches.value_of("threads") {
            let nt = threads.parse::<u32>().chain_err(
                || format!("Value '{}' for --threads is not an integer",
                           threads))?;

            if nt < 1 {
                return Err("Thread count must be at least 1".into());
            }

            nt
        } else {
            num_cpus::get() as u32 + 2
        };

        let reconnect = if let Some(reconnect) = matches.value_of("reconnect") {
            let seconds = reconnect.parse::<u64>().chain_err(
                || format!("Value '{}' for --reconnect is not an integer",
                           reconnect))?;
            Some(seconds)
        } else {
            None
        };

        let override_mode = {
            if let Some(mode) = matches.value_of("override-mode") {
                Some(mode.parse::<rules::SyncMode>().chain_err(
                    || format!("Value '{}' for --override-mode is invalid",
                               mode))?)
            } else {
                None
            }
        };

        fn do_run(matches: &ArgMatches, config: &cli::config::Config,
                  num_threads: u32, override_mode: Option<rules::SyncMode>,
                  key_chain: &mut Option<std::sync::Arc<server::KeyChain>>)
                  -> errors::Result<()> {
            let storage = create_storage(matches, config)?;

            cli::cmd_sync::run(config, storage,
                               matches.occurrences_of("verbose") as i32,
                               matches.occurrences_of("quiet") as i32,
                               matches.is_present("itemise"),
                               matches.is_present("itemise-unchanged"),
                               matches.value_of("colour").unwrap(),
                               matches.value_of("spin").unwrap(),
                               matches.is_present("include-ancestors"),
                               matches.is_present("dry-run"),
                               matches.is_present("watch"),
                               num_threads,
                               matches.value_of("strategy").unwrap(),
                               override_mode,
                               key_chain)
        }

        let mut key_chain = None;

        loop {
            use std::io::{stderr, Write};

            match do_run(&matches, &config, num_threads, override_mode,
                         &mut key_chain) {
                Ok(()) => return Ok(()),
                Err(e) => if let Some(seconds) = reconnect {
                    interrupt::clear_notify();

                    let _ = writeln!(
                        stderr(),
                        "Connection terminated due to error: {}\n\
                         Attempting reconnect in {} seconds...",
                        e, seconds);
                    ::std::thread::sleep(::std::time::Duration::new(
                        seconds, 0));
                    // Continue loop
                } else {
                    return Err(e);
                }
            }
        }
    } else if let Some(matches) = matches.subcommand_matches("ls") {
        set_up!(matches, config, storage, replica);
        cli::cmd_manual::ls(&replica, matches.values_of_os("path").unwrap(),
                            matches.occurrences_of("path") > 1,
                            matches.is_present("human-readable"))
    } else if let Some(matches) = matches.subcommand_matches("mkdir") {
        set_up!(matches, config, storage, replica);
        cli::cmd_manual::mkdir(&replica, matches.values_of_os("path").unwrap(),
                               parse_mode(matches.value_of("mode").unwrap())?)
    } else if let Some(matches) = matches.subcommand_matches("rmdir") {
        set_up!(matches, config, storage, replica);
        cli::cmd_manual::rmdir(&replica, matches.values_of_os("path").unwrap())
    } else if let Some(matches) = matches.subcommand_matches("cat") {
        set_up!(matches, config, storage, replica);
        cli::cmd_manual::cat(&replica, matches.values_of_os("path").unwrap())
    } else if let Some(matches) = matches.subcommand_matches("get") {
        set_up!(matches, config, storage, replica);
        cli::cmd_manual::get(&replica, matches.value_of_os("src").unwrap(),
                             matches.value_of_os("dst").unwrap(),
                             matches.is_present("force"),
                             matches.is_present("verbose"))
    } else if let Some(matches) = matches.subcommand_matches("put") {
        set_up!(matches, config, storage, replica);
        cli::cmd_manual::put(&replica, matches.value_of_os("src").unwrap(),
                             matches.value_of_os("dst").unwrap_or(
                                 ::std::ffi::OsStr::new("")),
                             matches.is_present("force"),
                             matches.is_present("verbose"))
    } else if let Some(matches) = matches.subcommand_matches("rm") {
        set_up!(matches, config, storage, replica);
        cli::cmd_manual::rm(&replica, matches.values_of_os("path").unwrap(),
                            matches.is_present("recursive"),
                            matches.is_present("verbose"))
    } else {
        panic!("Unhandled subcommand: {}", matches.subcommand_name().unwrap());
    }
}

fn parse_mode(s: &str) -> Result<defs::FileMode> {
    u32::from_str_radix(s, 8).map(|m| m & 0o7777).map_err(
        |_| format!("Invalid mode '{}'", s).into())
}
