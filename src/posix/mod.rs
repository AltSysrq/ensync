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

//! This module contains the implementation of the POSIX filesystem replica,
//! used for the client side (typically).
//!
//! The bulk of the replica is represented literally on the local filesystem.
//! Some side data (a cache of hashes as well as data to know which directories
//! are still clean) is stored in a SQLite database under a directory owned by
//! the replica. Additionally, the replica uses the directory it owns for
//! temporary files created during the sync process.
//!
//! The private directory must be on the same filesystem as the sync root. This
//! is due to its use as a staging area for new files and as a cache to
//! optimise renames (which can look like a delete followed by a create), both
//! of which require being able to rename files to or from their location under
//! the sync root. Attempts to access a directory in a different filesystem
//! return an error.
//!
//! Not everything in this module is POSIX-specific. The SQLite layer and some
//! of the higher-level replica logic could probably be abstracted and reused
//! for supporting a non-POSIX operating system.
//!
//! - The `dao` submodule manages the SQLite database.
//!
//! - The `replica` submodule implements a `Replica` atop the other submodules.
//!
//! Originally, the intent here was to use the raw POSIX API for interacting
//! with the filesystem due to the Rust API needing us to convert our
//! `OsString`s into `OsStr`s just so it can convert them back into new
//! `OsString`s. However, the fact that Linux *still* uses 32-bit off_t on
//! 32-bit platforms and requires separate 64-suffixed functions and types to
//! deal with that was sufficient inconvenience to use the API provided by
//! Rust.

mod dao;
mod dir;
mod replica;

pub use self::dir::DirHandle;
pub use self::replica::PosixReplica;
