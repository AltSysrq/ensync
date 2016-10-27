//-
// Copyright (c) 2016, Jason Lingle
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
extern crate regex;
extern crate serde;
extern crate serde_cbor;
extern crate sqlite;
extern crate tempfile;
extern crate tiny_keccak as keccak;
extern crate toml;
#[macro_use] extern crate error_chain;
#[macro_use] extern crate quick_error;

#[cfg(test)] extern crate libc;
#[cfg(test)] extern crate quickcheck;
#[cfg(test)] extern crate rand;
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
#[cfg(test)]
mod memory_replica;
mod reconcile;
mod block_xfer;
mod ancestor;
mod posix;
mod server;

fn main() {
    println!("hello world");
}
