//-
// Copyright (c) 2017, Jason Lingle
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

use libc::isatty;
use std::env;
use std::fs;
use std::io::{stdin, stdout, Write};
use std::path::Path;

use crate::errors::*;
use crate::server::{rpc, LocalStorage};

pub const SHELL_IDENTITY: &'static str = "I am ensync shell";

pub fn run<P: AsRef<Path>>(path: P) -> Result<()> {
    if 1 == unsafe { isatty(0) } || 1 == unsafe { isatty(1) } {
        return Err("The `server` subcommand is not for interactive use".into());
    }

    let path = path.as_ref();

    let storage = LocalStorage::open(path).chain_err(|| {
        format!("Failed to set up storage at '{}'", path.display())
    })?;
    rpc::run_server_rpc(storage, stdin(), stdout())
        .chain_err(|| "Server-side RPC handler failed")
}

pub fn shell() -> Result<()> {
    if 1 == unsafe { isatty(0) } || 1 == unsafe { isatty(1) } {
        return Err("Ensync shell is not for interactive use".into());
    }

    // If we have been given any arguments, print a string that lets `ensync
    // setup` recognise us.
    if env::args().count() > 1 {
        print!("{}\x00", SHELL_IDENTITY);
        stdout().flush()?;
    }

    let path = fs::read_link("ensync-server-dir").chain_err(|| {
        "Failed to read symlink '~/ensync-server-dir' (needs to be a \
            symlink to where `ensync server` should place files)"
    })?;
    run(path)
}
