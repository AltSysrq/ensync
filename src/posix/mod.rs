//-
// Copyright (c) 2016, Jason Lingle
//
// Permission to  use, copy,  modify, and/or distribute  this software  for any
// purpose  with or  without fee  is hereby  granted, provided  that the  above
// copyright notice and this permission notice appear in all copies.
//
// THE SOFTWARE  IS PROVIDED "AS  IS" AND  THE AUTHOR DISCLAIMS  ALL WARRANTIES
// WITH  REGARD   TO  THIS  SOFTWARE   INCLUDING  ALL  IMPLIED   WARRANTIES  OF
// MERCHANTABILITY AND FITNESS. IN NO EVENT  SHALL THE AUTHOR BE LIABLE FOR ANY
// SPECIAL,  DIRECT,   INDIRECT,  OR  CONSEQUENTIAL  DAMAGES   OR  ANY  DAMAGES
// WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS, WHETHER IN AN ACTION
// OF  CONTRACT, NEGLIGENCE  OR OTHER  TORTIOUS ACTION,  ARISING OUT  OF OR  IN
// CONNECTION WITH THE USE OR PERFORMANCE OF THIS SOFTWARE.

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
//! - The `posix` submodule presents a simplified front-end to the raw POSIX
//! system calls provided by `libc`.
//!
//! - The `replica` submodule implements a `Replica` atop the other submodules.

mod dao;
