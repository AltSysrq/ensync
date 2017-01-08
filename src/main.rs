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
extern crate libc;
extern crate rand;
extern crate regex;
extern crate rpassword;
extern crate serde;
extern crate serde_cbor;
extern crate sqlite;
extern crate tempfile;
extern crate tiny_keccak as keccak;
extern crate toml;
#[macro_use] extern crate clap;
#[macro_use] extern crate error_chain;
#[macro_use] extern crate quick_error;

#[cfg(test)] extern crate quickcheck;
#[cfg(test)] extern crate os_pipe;
#[cfg(test)] extern crate tempdir;

mod defs;
mod errors;
mod serde_types {
    include!(concat!(env!("OUT_DIR"), "/serde_types.rs"));
}
mod sql;
mod work_stack;

mod rules;
mod log;
mod replica;
#[cfg(test)] mod memory_replica;
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
    let new_key_arg = Arg::with_name("new-key")
        .required(false)
        .takes_value(true)
        .short("n")
        .long("new")
        .help("Specify how to get the new passphrase, like \
               with the `passphrase` line in the config")
        .default_value("prompt");

    let matches = App::new("ensync")
        .author(crate_authors!("\n"))
        .about("Encrypted bidirectional file synchroniser")
        .version(crate_version!())
        .setting(AppSettings::DontCollapseArgsInUsage)
        .setting(AppSettings::DeriveDisplayOrder)
        .setting(AppSettings::SubcommandRequiredElseHelp)
        .setting(AppSettings::UnifiedHelpMessage)
        .setting(AppSettings::VersionlessSubcommands)
        .set_term_width(80)
        .max_term_width(160)
        .subcommand(SubCommand::with_name("init-keys")
                    .about("Initialise the key store")
                    .setting(AppSettings::DontCollapseArgsInUsage)
                    .arg(&config_arg)
                    .arg(Arg::with_name("key-name")
                         .help("Name for the first key in the key store")
                         .required(false))
                    .after_help("This command is used to initialise the key \
                                 store in the server. It generates a new \
                                 master key and associates one user key with \
                                 it, corresponding to the passphrase defined \
                                 in the configuration. You should specify the \
                                 key name if you plan on using multiple keys. \
                                 This cannot be used after the key store has \
                                 been initialised; see `add-key` for that \
                                 instead."))
        .subcommand(SubCommand::with_name("add-key")
                    .about("Add a new key to the key store")
                    .setting(AppSettings::DontCollapseArgsInUsage)
                    .arg(&config_arg)
                    .arg(&old_key_arg)
                    .arg(&new_key_arg)
                    .arg(Arg::with_name("key-name")
                         .required(true)
                         .help("The name of the key to add")))
        .subcommand(SubCommand::with_name("change-key")
                    .about("Change a key in the key store")
                    .setting(AppSettings::DontCollapseArgsInUsage)
                    .arg(&config_arg)
                    .arg(&old_key_arg)
                    .arg(&new_key_arg)
                    .arg(Arg::with_name("key-name")
                         .required(false)
                         .help("The name of the key to edit"))
                    .arg(Arg::with_name("force")
                         .long("force")
                         .takes_value(false)
                         .required(false)
                         .help("Change <key-name> even if the old passphrase \
                                does not correspond to that key")))
        .subcommand(SubCommand::with_name("del-key")
                    .about("Delete a key from the key store")
                    .setting(AppSettings::DontCollapseArgsInUsage)
                    .arg(&config_arg)
                    .arg(Arg::with_name("key-name")
                         .required(true)
                         .help("The name of the key to delete")))
        .subcommand(SubCommand::with_name("list-keys")
                    .about("List the keys in the key store")
                    .setting(AppSettings::DontCollapseArgsInUsage)
                    .arg(&config_arg))
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
        ($matches:expr, $config:expr, $key:expr) => {
            if let Some(c) = $matches.value_of($key) {
                c.parse::<PassphraseConfig>()?
            } else {
                PassphraseConfig::Prompt
            }
        };
    }

    if let Some(matches) = matches.subcommand_matches("server") {
        cli::cmd_server::run(matches.value_of("path").unwrap())
    } else if let Some(matches) = matches.subcommand_matches("init-keys") {
        set_up!(matches, config, storage);
        cli::cmd_keymgmt::init_keys(&config, &*storage,
                                    matches.value_of("key-name"))
    } else if let Some(matches) = matches.subcommand_matches("add-key") {
        set_up!(matches, config, storage);
        let old = passphrase_or_config!(matches, config, "old-key");
        let new = passphrase_or_prompt!(matches, config, "new-key");
        cli::cmd_keymgmt::add_key(&*storage, &old, &new,
                                  matches.value_of("key-name").unwrap())
    } else if let Some(matches) = matches.subcommand_matches("list-keys") {
        set_up!(matches, config, storage);
        cli::cmd_keymgmt::list_keys(&*storage)
    } else if let Some(matches) = matches.subcommand_matches("change-key") {
        set_up!(matches, config, storage);
        let old = passphrase_or_config!(matches, config, "old-key");
        let new = passphrase_or_prompt!(matches, config, "new-key");
        cli::cmd_keymgmt::change_key(&config, &*storage, &old, &new,
                                     matches.value_of("key-name"),
                                     matches.is_present("force"))
    } else if let Some(matches) = matches.subcommand_matches("del-key") {
        set_up!(matches, config, storage);
        cli::cmd_keymgmt::del_key(
            &*storage, matches.value_of("key-name").unwrap())
    } else {
        panic!("Unhandled subcommand: {}", matches.subcommand_name().unwrap());
    }
}
