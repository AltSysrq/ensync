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

use std::ffi::{OsStr,OsString};
use std::error::Error;
use std::result::Result as StdResult;

use defs::*;

pub type Result<T> = StdResult<T, Box<Error>>;

/// Trait for the `Replica::Directory` type.
pub trait ReplicaDirectory {
    /// Returns the full path of this directory, suitable for display to the
    /// user.
    fn full_path(&self) -> &OsStr;
}

/// Represents one of the two replicas; ie, the client filesystem or the
/// encrypted mirror on the server.
///
/// Mutating functions guarantee that if they return success, the full result
/// will be visible on a subsequent call to `Replica::list()` for the same
/// directory, even if the client or server process terminates gracelessly.
///
/// Replicas are intended to use interior mutability if they must keep mutable
/// state. All methods on `Replica` therefore take an immutable `self`
/// reference. This is a consequence of there currently being no way to express
/// something like `fn chdir<'a>(&'a self, &OsStr) -> Self::Directory<'a>` to
/// allow the mutable state to be moved into the directory objects instead.
pub trait Replica {
    /// Type representing an operating directory.
    type Directory : ReplicaDirectory + 'static;
    /// Type which this replica uses to transfer data in from the other
    /// replica.
    type TransferIn;
    /// Type which this replica uses to transfer data out to the other replica.
    type TransferOut;

    /// Returns whether the given directory has been marked dirty for this
    /// session.
    ///
    /// Returns true if the directory cannot be accessed (including if the
    /// directory is synthetic).
    fn is_dir_dirty(&self, &Self::Directory) -> bool;
    /// Marks the given directory as clean, if no modifications to its contents
    /// have occurred since the last call to list() except through the
    /// directory handle itself.
    ///
    /// Returns true if the directory is now clean; false if it is not clean
    /// because there were concurrent modifications; error if checking whether
    /// the directory is clean, or marking it clean, failed.
    ///
    /// This call is not required to test explicitly for concurrent
    /// modifications; it is permitted to return `true` even if the directory
    /// is actually still dirty as long as the next run will return true from
    /// `is_dir_dirty()`.
    ///
    /// If the directory is synthetic and does not exist in any concrete form,
    /// this call succeeds and returns true.
    fn set_dir_clean(&self, &Self::Directory) -> Result<bool>;
    /// Returns the root directory for this replica.
    fn root(&self) -> Result<Self::Directory>;
    /// Reads the contents of the given directory.
    ///
    /// On success, the full conents of the directory (excluding "." and ".."
    /// if returned by the underlying system) after transform/filtering are
    /// returned, in no particular order.
    fn list(&self, &mut Self::Directory) -> Result<Vec<(OsString,FileData)>>;
    /// Renames a file within a directory.
    ///
    /// The file of any type named by `old` is renamed to `new`. A best effort
    /// is made to prevent renaming onto an existing file.
    fn rename(&self, &mut Self::Directory, old: &OsStr, new: &OsStr)
              -> Result<()>;
    /// Deletes the file within a directory.
    ///
    /// The file identified by `target` is removed from the directory. A best
    /// effort is made to prevent deleting the file if it does not match
    /// `target`.
    ///
    /// If `target` is a directory, this call must fail if the directory is not
    /// actually empty.
    fn remove(&self, &mut Self::Directory, target: File) -> Result<()>;
    /// Creates a file within a directory.
    ///
    /// A file in the given directory is created conforming to `source`. If
    /// `source` is a regular file and no object with that hash is available on
    /// this replica, `xfer` is used to copy it from the other replica.
    ///
    /// The replica must guarantee that at no point will an intermediate state
    /// be visible; `source` must atomically go from not existing to existing
    /// with the correct contents.
    ///
    /// A best effort is made to avoid creating a file on top of an existing
    /// one.
    ///
    /// Returns the actual file version resulting from this creation. This may
    /// be different from `source` if the hash on `source` is incorrect.
    fn create(&self, &mut Self::Directory, source: File,
              xfer: Self::TransferIn) -> Result<FileData>;
    /// Updates a file within a directory.
    ///
    /// A file in the given directory identified by `name` is changed from
    /// `old` to `new`. If `new` is a regular file and no object with that hash
    /// is available within the replica, `xfer` is used to transfer the data
    /// from the other replica.
    ///
    /// The replica must guarantee that at no point will an intermediate state
    /// be visible, except that the old file may be temporarily renamed if a
    /// rename/create/delete sequence is needed to perform the operation.
    ///
    /// A best effort is made to avoid replacing a file which does not match
    /// `old`. If `old` is a directory and `new` is not, the directory must be
    /// empty. This is a special case; if the underlying system cannot
    /// atomically check that the directory is empty, remove it, and replace it
    /// with the new item, it should remove the directory _first_, so that the
    /// failure mode is to either take no action at all, or to fail halfway,
    /// resulting in the loss of the directory (which isn't generally a big
    /// deal), rather than possibly renaming a whole directory tree.
    ///
    /// Returns the actual file version resulting from this update. This may be
    /// different from `new` if the hash on `new` was incorrect.
    fn update(&self, &mut Self::Directory, name: &OsStr,
              old: &FileData, new: &FileData,
              xfer: Self::TransferIn) -> Result<FileData>;
    /// Creates a new context within the subdirectory identified by `subdir`.
    fn chdir(&self, &Self::Directory, subdir: &OsStr)
             -> Result<Self::Directory>;
    /// Creates a "synthetic" subdirectory and returns a context that can be
    /// used to manipulate it.
    ///
    /// A synthetic directory is not immediately created; rather, the directory
    /// object simply stores the name and mode of that directory (and any
    /// parent synthetic directories) and creates the hierarchy if any files
    /// are to be created within the synthetic directory.
    ///
    /// `Replica::ls()` always returns the empty vector for synthetic
    /// directories that have not yet materialised.
    fn synthdir(&self, &mut Self::Directory, subdir: &OsStr, mode: FileMode)
                -> Self::Directory;
    /// Deletes the directory identified by the given handle, if it is empty.
    ///
    /// If the directory already does not exist (regardless of whether the
    /// handle is synthetic), this call succeeds.
    ///
    /// If the path indicated by the directory exists but is not actually a
    /// directory, a best effort should be made to not remove that object;
    /// whether the call succeeds in this case is unspecified.
    fn rmdir(&self, &mut Self::Directory) -> Result<()>;

    /// Returns an object which can be used to transfer `file` out of this
    /// replica.
    fn transfer(&self, &Self::Directory, file: File)
                -> Self::TransferOut;

    /// Performs any initial setup of this replica.
    ///
    /// This is generally a scan for dirty directories, sanity checks, etc.
    ///
    /// The default is a noop.
    fn prepare(&self) -> Result<()> { Ok(()) }

    /// Performs any final cleanup on this replica.
    ///
    /// This should not affect the actual content of the replica; instead, it
    /// gives it a chance to remove temporary data, clean up orphaned files,
    /// etc.
    ///
    /// The default is a noop.
    fn clean_up(&self) -> Result<()> { Ok(()) }
}

/// A Replica which supports a "null transfer" input.
pub trait NullTransfer : Replica {
    /// Produces a `TransferIn` that can be used to use `Replica::create()` or
    /// `Replica::update()` with the given `FileData`.
    ///
    /// The hash on the `FileData` is assumed correct.
    fn null_transfer(file: &FileData) -> Self::TransferIn;
}

/// Trait for replicas which also support the "condemn" operation.
pub trait Condemn : Replica {
    /// Marks a filename within a directory as condemned.
    ///
    /// The name given by `file` within `dir` is marked as condemned. When
    /// `Replica::list()` is invoked on a directory with condemned filenames,
    /// any matching files are deleted, recursively as needed, and all
    /// condemned names are cleared.
    ///
    /// Note that it is specifically the *names*, not the *files* that are
    /// condemned. It is permissible to condemn a name which is not bound to
    /// any file; renaming a file under a condemned name to a different name
    /// does not bring the condemnation with it (ie, the original name remains
    /// condemned).
    ///
    /// When this call returns, the condemnation must be committed and
    /// persisted, such that it will take effect even if the process exits
    /// gracelessly.
    ///
    /// Condemning a name which is already condemned has no effect.
    fn condemn(&self, dir: &mut Self::Directory, file: &OsStr) -> Result<()>;
    /// Reverses a call to `condemn()`.
    ///
    /// Uncondemning a name which is not condemned has no effect.
    fn uncondemn(&self, dir: &mut Self::Directory, file: &OsStr) -> Result<()>;
}
