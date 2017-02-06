//-
// Copyright (c) 2016, 2017, Jason Lingle
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

extern crate chrono;
// Not sure why the `rust_` prefix gets stripped by cargo
extern crate crypto as rust_crypto;
extern crate flate2;
extern crate libc;
extern crate num_cpus;
extern crate rand;
extern crate regex;
#[cfg(feature = "passphrase-prompt")] extern crate rpassword;
extern crate sqlite;
extern crate tempfile;
extern crate tiny_keccak as keccak;
extern crate toml;
#[macro_use] extern crate clap;
#[macro_use] extern crate fourleaf;
#[macro_use] extern crate error_chain;
#[macro_use] extern crate lazy_static;
#[macro_use] extern crate quick_error;

#[cfg(test)] extern crate quickcheck;
#[cfg(test)] extern crate os_pipe;
#[cfg(test)] extern crate tempdir;

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

use errors::{Result, ResultExt};

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
    use std::fs;

    use clap::*;

    use cli::config::PassphraseConfig;

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
        .setting(AppSettings::DontCollapseArgsInUsage)
        .setting(AppSettings::DeriveDisplayOrder)
        .setting(AppSettings::SubcommandRequiredElseHelp)
        .setting(AppSettings::UnifiedHelpMessage)
        .setting(AppSettings::VersionlessSubcommands)
        .max_term_width(120)
        .subcommand(SubCommand::with_name("setup")
                    .about("Wizard to set up simple ensync configurations")
                    .setting(AppSettings::DontCollapseArgsInUsage)
                    .arg(&config_arg)
                    .arg(Arg::with_name("key")
                         .required(false)
                         .takes_value(true)
                         .short("k")
                         .long("key")
                         .default_value("prompt")
                         .help("\
How to get the passphrase. Defaults to `prompt` to read it interactively. Use
`string:xxx` to use a fixed value, `file:/some/path` to read it from a file,
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
                                `user@host:/path/to/files`).")))
        .subcommand(SubCommand::with_name("sync")
                    .about("Sync files according to configuration")
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
                         .help("Don't actually make any changes")))
        .subcommand(
            SubCommand::with_name("key")
            .about("Perform key management operations")
            .subcommand(SubCommand::with_name("init")
                        .about("Initialise the key store")
                        .setting(AppSettings::DontCollapseArgsInUsage)
                        .arg(&config_arg)
                        .arg(Arg::with_name("key-name")
                             .help("Name for the first key in the key store")
                             .required(false))
                        .after_help("\
This command is used to initialise the key store in the server. It generates
a new internal key set and associates one user key withthem, corresponding
to the passphrase defined in the configuration. You should specify thekey
name if you plan on using multiple keys.This cannot be used after the key
store hasbeen initialised; see `key add` for that instead."))
            .subcommand(SubCommand::with_name("add")
                        .about("Add a new key to the key store")
                        .setting(AppSettings::DontCollapseArgsInUsage)
                        .arg(&config_arg)
                        .arg(&old_key_arg)
                        .arg(&new_key_arg)
                        .arg(&root_key_arg)
                        .arg(Arg::with_name("key-name")
                             .required(true)
                             .help("The name of the key to add")))
            .subcommand(SubCommand::with_name("change")
                        .about("Change a key in the key store")
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
                                    that key")))
            .subcommand(SubCommand::with_name("rm")
                        .alias("del")
                        .about("Delete a key from the key store")
                        .setting(AppSettings::DontCollapseArgsInUsage)
                        .arg(&config_arg)
                        .arg(&root_key_arg)
                        .arg(Arg::with_name("key-name")
                             .required(true)
                             .help("The name of the key to delete"))
                        .after_help("The named key/passphrase is removed from \
                                     the key store. The `root` key used to \
                                     write to the key store may not be the \
                                     key being removed."))
            .subcommand(SubCommand::with_name("list")
                        .about("List the keys in the key store")
                        .setting(AppSettings::DontCollapseArgsInUsage)
                        .arg(&config_arg))
            .subcommand(
                SubCommand::with_name("group")
                .about("Manage key groups")
                .subcommand(SubCommand::with_name("create")
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
                            .after_help(
                                "Creates one or more key groups. All \
                                 groups will initially be granted to \
                                 the key that was derived from the \
                                 passphrase."))
                .subcommand(SubCommand::with_name("assoc")
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
                            .after_help(
                                "Associates one or more key groups to a \
                                 key. This involves two passphrases; the \
                                 first must be a member of all the listed \
                                 groups and is used to derive the \
                                 information needed to grant them. The \
                                 second is the one to which the groups \
                                 are to be granted."))
                .subcommand(SubCommand::with_name("disassoc")
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
                            .after_help(
                                "Disassociates one or more key groups from \
                                 a key identified by name, essentially \
                                 undoing the effect of `key group assoc`. \
                                 This cannot be used to remove a group from \
                                 the last key associated; use `key group \
                                 destroy` for that instead."))
                .subcommand(SubCommand::with_name("destroy")
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
                            .after_help(
                                "Destroys the listed groups, implicitly \
                                 disassociating them from any and all \
                                 keys. WARNING: This is an irreversible \
                                 operation which will render any data \
                                 protected by the destroyed key groups \
                                 irrecoverable."))))
        .subcommand(SubCommand::with_name("ls")
                    .alias("dir")
                    .about("List directory contents on server")
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
                    .after_help("List the content of each given directory \
                                 (or individual file)."))
        .subcommand(SubCommand::with_name("mkdir")
                    .alias("md")
                    .about("Directly create a directory on the server")
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
                         .help("The path(s) to create")))
        .subcommand(SubCommand::with_name("rmdir")
                    .alias("rd")
                    .about("Remove an empty directory on the server")
                    .setting(AppSettings::DontCollapseArgsInUsage)
                    .arg(&config_arg)
                    .arg(Arg::with_name("path")
                         .multiple(true)
                         .required(true)
                         .help("The path(s) to remove"))
                    .after_help("Removes the listed directory(ies), which \
                                 must be empty. To remove non-empty \
                                 directories, use `rm` with the `-r` flag."))
        .subcommand(SubCommand::with_name("cat")
                    .alias("type")
                    .alias("dump")
                    .about("Dump file contents to standard output")
                    .setting(AppSettings::DontCollapseArgsInUsage)
                    .arg(&config_arg)
                    .arg(Arg::with_name("path")
                         .multiple(true)
                         .required(true)
                         .help("The path(s) to dump"))
                    .after_help("Dumps the raw content of all listed files \
                                 to standard output, without any other \
                                 information. Every path must be a regular \
                                 file (symlinks are not permitted)."))
        .subcommand(SubCommand::with_name("get")
                    .about("Directly fetch files from server")
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
                    .after_help("This behaves roughly similarly to `cp -a`, \
                                 in that it will copy files recursively with \
                                 attributes, and doesn't treat symlinks \
                                 specially."))
        .subcommand(SubCommand::with_name("put")
                    .about("Directly upload files to server")
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
                    .after_help("This behaves roughly similarly to `cp -a`, \
                                 in that it will copy files recursively with \
                                 attributes, and doesn't treat symlinks \
                                 specially."))
        .subcommand(SubCommand::with_name("rm")
                    .alias("del")
                    .about("Directly delete files or directory trees \
                            on the server")
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
                    .arg(&simple_verbose_arg))
        .subcommand(SubCommand::with_name("server")
                    .about("Run the server-side component")
                    .setting(AppSettings::DontCollapseArgsInUsage)
                    .arg(Arg::with_name("path")
                         .help("Path to server-side storage")
                         .required(true)))
        .get_matches();

    macro_rules! set_up {
        ($matches:ident, $config:ident) => {
            let $config = cli::config::Config::read(
                $matches.value_of("config").unwrap())
                .chain_err(|| "Could not read configuration")?;
        };

        ($matches:ident, $config:ident, $storage:ident) => {
            set_up!($matches, $config);
            let $storage = cli::open_server::open_server_storage(
                &$config.server)?;
            fs::create_dir_all(&$config.private_root).chain_err(
                || format!("Failed to create ensync private directory '{}'",
                           $config.private_root.display()))?;
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
        } else if let Some(matches) = matches.subcommand_matches("list") {
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
        set_up!(matches, config, storage);

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

        cli::cmd_sync::run(&config, storage,
                           matches.occurrences_of("verbose") as i32,
                           matches.occurrences_of("quiet") as i32,
                           matches.is_present("itemise"),
                           matches.is_present("itemise-unchanged"),
                           matches.value_of("colour").unwrap(),
                           matches.value_of("spin").unwrap(),
                           matches.is_present("include-ancestors"),
                           matches.is_present("dry-run"),
                           num_threads)
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
