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

//! This module contains the implementation of the POSIX filesystem replica,
//! used for the client side (typically).
//!
//! The bulk of the replica is represented literally on the local filesystem.
//! Some side data (a cache of hashes as well as data to know which directories
//! are still clean) is stored in a SQLite database under a directory owned by
//! the replica. Additionally, the replica uses the directory it owns for
//! temporary files created during the sync process.
//!
//! The private directory should be on the same filesystem as the sync root.
//! This is due to its use as a cache to optimise renames (which can look like
//! a delete followed by a create), which requires being able to rename files
//! to or from their location under the sync root. If the sync crosses a
//! filesystem boundary, this rename optimisation does not occur; renamed files
//! will instead be re-downloaded if the deletion occurs before the create.
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
//!
//! This top-level module also has some miscellaneous functions for working
//! with POSIX filesystems.

use libc::{c_long, futimes, timeval, utimes};
use std::ffi::CString;
use std::io;
use std::os::unix::ffi::OsStringExt;
use std::os::unix::io::AsRawFd;
use std::path::Path;

use crate::defs::FileTime;

mod dao;
mod dir;
mod replica;

pub use self::dir::DirHandle;
pub use self::replica::PosixReplica;

/// Set the atime and mtime of the given file handle to the given time, in
/// seconds.
pub fn set_mtime(file: &impl AsRawFd, mtime: FileTime) -> io::Result<()> {
    let access_modified = [
        timeval {
            tv_sec: mtime as c_long,
            tv_usec: 0,
        },
        timeval {
            tv_sec: mtime as c_long,
            tv_usec: 0,
        },
    ];
    let code = unsafe { futimes(file.as_raw_fd(), &access_modified[0]) };

    if 0 == code {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// Like `set_mtime`, but operates on a path instead.
pub fn set_mtime_path<P: AsRef<Path>>(
    path: P,
    mtime: FileTime,
) -> io::Result<()> {
    let access_modified = [
        timeval {
            tv_sec: mtime as c_long,
            tv_usec: 0,
        },
        timeval {
            tv_sec: mtime as c_long,
            tv_usec: 0,
        },
    ];
    let path =
        CString::new(path.as_ref().to_owned().into_os_string().into_vec())
            .expect("set_mtime_path path argument contains NUL");
    let code = unsafe { utimes(path.as_ptr(), &access_modified[0]) };

    if 0 == code {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}
