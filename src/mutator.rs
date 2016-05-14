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

use std::fmt;
use std::error::Error;
use std::result::Result as StdResult;
use std::iter::Iterator;

use defs::*;

/// Error type from the various `Mutator` methods.
#[derive(Debug)]
pub enum MutationError {
    /// The mutation failed because the target object or one of its parents
    /// either ceased to exist or has been modified in a way that does not
    /// permit the operation to proceed.
    Modified,
    /// The mutation failed due to an unrecoverable error (eg, IO error, server
    /// is gone).
    Failure(Box<Error>),
}

impl fmt::Display for MutationError {
    fn fmt(&self, f: &mut fmt::Formatter) -> StdResult<(), fmt::Error> {
        match *self {
            MutationError::Modified =>
                write!(f, "Concurrent modification"),
            MutationError::Failure(ref e) =>
                write!(f, "Error: {}", *e),
        }
    }
}

impl Error for MutationError {
    fn description(&self) -> &str {
        match *self {
            MutationError::Modified => "concurrent modification",
            MutationError::Failure(ref e) => e.description(),
        }
    }
}

pub type Result<T> = StdResult<T,MutationError>;

/// Represents the "context" of a single directory.
pub trait Context {
    /// Returns the sync mode in effect for this directory.
    fn sync_mode(&self) -> &SyncMode;
}

/// The interface between the reconciler and the client/server transfer system
/// and ancestor store.
pub trait Mutator {
    /// The context of the current location, essentially representing a
    /// directory and sync flags.
    type Context : Context;

    /// Constructs the root context for this sync, including determining the id
    /// of the root directory on the server.
    fn root(&self) -> Result<Self::Context>;
    /// Cancels all pending ancestor changes.
    fn cancel_ancestors(&self);
    /// Finishes all mutations. The root on the server is updated to match
    /// root, and any changes to the ancestors are flushed atomically.
    fn finish(&self, root: &HashId) -> Result<()>;
    /// Reads the current directory in from the client filesystem. HashIds is
    /// the listing may be incorrect due to using stale data or the actual
    /// hashes being unknown.
    fn ls_client(&self, &Self::Context) -> Result<Vec<DirPtr>>;
    /// Reads the ancestor state of the current directory. This is always
    /// either an empty vector or a value written by `put_dir_ancestor()`.
    fn ls_ancestor(&self, &Self::Context) -> Result<Vec<DirPtr>>;
    /// Reads the server state of the current directory. This is always either
    /// an empty vector or a value written by `put_dir_server()`.
    fn ls_server(&self, &Self::Context) -> Result<Vec<DirPtr>>;
    /// Transfers the file in the current directory identified by `src` to the
    /// server. Only the name and type of `src` are meaningful.
    ///
    /// Returns the HashId of the resulting blob.
    fn upload(&self, &Self::Context, src: &DirPtr) -> Result<HashId>;
    /// Transfers the file identified by `src` to the current directory.
    ///
    /// The `HashId` identifies the exact file to transfer, and the file will
    /// have the same name and mode as dictated by `src`. The named file is
    /// created or replaced atomically.
    ///
    /// If `if_match` is None, the call will fail with `Modified` if such a
    /// file already exists. Otherwise, the call will fail with `Modified` if
    /// the file content no longer has the specified hash. In this case, the
    /// new content will have replaced that file anyway, but the old content
    /// will exist under a new name.
    fn download(&self, &Self::Context, src: &DirPtr,
                if_match: Option<HashId>) -> Result<()>;
    /// Deletes the file in the current directory identified by `target`.
    ///
    /// If `target` is a directory, it must be empty, and the `HashId` is
    /// ignored.
    ///
    /// Otherwise, the given object is removed. A best effort is made to check
    /// that it still has the `HashId` given by `target`; if it does not, it
    /// returns `Modified`.
    fn rm_client(&self, &Self::Context, target: &DirPtr) -> Result<()>;
    /// Rewrites the ancestor state for the current directory to match `data`.
    ///
    /// This change is not committed until the call to `finish()`.
    fn put_dir_ancestor<'a, I: Iterator<Item = &'a DirPtr>>(
        &self, &Self::Context, data: I) -> Result<()>;
    /// Rewrites the server state for the current directory to match `data`.
    ///
    /// Returns the `HashId` of the resulting blob, which must either be
    /// propagated into the parent directory or passed into `finish()` if this
    /// is the root directory.
    fn put_dir_server<'a, I: Iterator<Item = &'a DirPtr>>(
        &self, &Self::Context, data: I) -> Result<HashId>;
    /// Steps down into the subdirectory identified by `subdir`, returning a
    /// new context representing that directory.
    fn chdir(&self, &Self::Context, subdir: &DirPtr)
             -> Result<Self::Context>;
}
