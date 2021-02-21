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

use std::ffi::{OsStr, OsString};
use std::sync::{Condvar, Mutex, Weak};

use crate::defs::*;
use crate::errors::Result;
use crate::interrupt;

/// Controls the behaviour of `Replica::prepare`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum PrepareType {
    /// Only check for the aftermath of events reported to the `WatchHandle`
    /// associated with `Watch::watch()`.
    Watched,
    /// The ordinary prepare mode. The replica may assume things marked clean
    /// are still clean provided their conditions are met.
    Fast,
    /// The replica must consider all directories dirty after `prepare()`
    /// completes.
    Clean,
    /// Like `Clean`, but additionally discard all caches that cannot be
    /// validated perfectly.
    Scrub,
}

/// Trait for the `Replica::Directory` type.
pub trait ReplicaDirectory: Send {
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
pub trait Replica: Sync + Send {
    /// Type representing an operating directory.
    type Directory: ReplicaDirectory + 'static;
    /// Type which this replica uses to transfer data in from the other
    /// replica.
    type TransferIn;
    /// Type which this replica uses to transfer data out to the other replica.
    type TransferOut;

    /// Returns whether any fatal errors have occurred.
    fn is_fatal(&self) -> bool {
        false
    }

    /// Returns whether the given directory has been marked dirty for this
    /// session.
    ///
    /// Returns true if the directory cannot be accessed (including if the
    /// directory is synthetic).
    fn is_dir_dirty(&self, _: &Self::Directory) -> bool;
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
    fn set_dir_clean(&self, _: &Self::Directory) -> Result<bool>;
    /// Returns the root directory for this replica.
    fn root(&self) -> Result<Self::Directory>;
    /// Reads the contents of the given directory.
    ///
    /// On success, the full conents of the directory (excluding "." and ".."
    /// if returned by the underlying system) after transform/filtering are
    /// returned, in no particular order.
    fn list(
        &self,
        _: &mut Self::Directory,
    ) -> Result<Vec<(OsString, FileData)>>;
    /// Renames a file within a directory.
    ///
    /// The file of any type named by `old` is renamed to `new`. A best effort
    /// is made to prevent renaming onto an existing file.
    fn rename(
        &self,
        _: &mut Self::Directory,
        old: &OsStr,
        new: &OsStr,
    ) -> Result<()>;
    /// Deletes the file within a directory.
    ///
    /// The file identified by `target` is removed from the directory. A best
    /// effort is made to prevent deleting the file if it does not match
    /// `target`.
    ///
    /// If `target` is a directory, this call must fail if the directory is not
    /// actually empty.
    fn remove(&self, _: &mut Self::Directory, target: File) -> Result<()>;
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
    fn create(
        &self,
        _: &mut Self::Directory,
        source: File,
        xfer: Self::TransferIn,
    ) -> Result<FileData>;
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
    fn update(
        &self,
        _: &mut Self::Directory,
        name: &OsStr,
        old: &FileData,
        new: &FileData,
        xfer: Self::TransferIn,
    ) -> Result<FileData>;
    /// Creates a new context within the subdirectory identified by `subdir`.
    fn chdir(
        &self,
        _: &Self::Directory,
        subdir: &OsStr,
    ) -> Result<Self::Directory>;
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
    fn synthdir(
        &self,
        _: &mut Self::Directory,
        subdir: &OsStr,
        mode: FileMode,
    ) -> Self::Directory;
    /// Deletes the directory identified by the given handle, if it is empty.
    ///
    /// If the directory already does not exist (regardless of whether the
    /// handle is synthetic), this call succeeds.
    ///
    /// If the path indicated by the directory exists but is not actually a
    /// directory, a best effort should be made to not remove that object;
    /// whether the call succeeds in this case is unspecified.
    fn rmdir(&self, _: &mut Self::Directory) -> Result<()>;

    /// Returns an object which can be used to transfer `file` out of this
    /// replica.
    fn transfer(
        &self,
        _: &Self::Directory,
        file: File,
    ) -> Result<Self::TransferOut>;

    /// Performs any initial setup of this replica.
    ///
    /// This is generally a scan for dirty directories, sanity checks, etc.
    ///
    /// The default is a noop.
    #[allow(unused_variables)]
    fn prepare(&self, typ: PrepareType) -> Result<()> {
        Ok(())
    }

    /// Performs any final cleanup on this replica.
    ///
    /// This should not affect the actual content of the replica; instead, it
    /// gives it a chance to remove temporary data, clean up orphaned files,
    /// etc.
    ///
    /// The default is a noop.
    fn clean_up(&self) -> Result<()> {
        Ok(())
    }
}

/// A Replica which supports a "null transfer" input.
pub trait NullTransfer: Replica {
    /// Produces a `TransferIn` that can be used to use `Replica::create()` or
    /// `Replica::update()` with the given `FileData`.
    ///
    /// The hash on the `FileData` is assumed correct.
    fn null_transfer(file: &FileData) -> Self::TransferIn;
}

/// Trait for replicas which also support the "condemn" operation.
pub trait Condemn: Replica {
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

#[derive(Debug, Clone, Copy)]
struct WatchStatus {
    dirty: bool,
    context_lost: bool,
}

impl Default for WatchStatus {
    fn default() -> WatchStatus {
        WatchStatus {
            dirty: false,
            context_lost: false,
        }
    }
}

/// Handle passed to `Replica::watch` to allow notifying higher-level code
/// about changes within the replica.
///
/// Initially, the `Watch` is marked both `dirty` and `context_lost`.
pub struct WatchHandle {
    cond: Condvar,
    status: Mutex<WatchStatus>,
}

impl Default for WatchHandle {
    fn default() -> Self {
        WatchHandle {
            cond: Condvar::new(),
            status: Mutex::new(WatchStatus::default()),
        }
    }
}

impl WatchHandle {
    /// Checks whether this `WatchHandle` has the `dirty` flag set, indicating
    /// that changes have been detected. The `dirty` flag is then cleared.
    pub fn check_dirty(&self) -> bool {
        let mut lock = self.status.lock().unwrap();
        let dirty = lock.dirty;
        lock.dirty = false;
        dirty
    }

    /// Checks whether this `WatchHandle` has the `context_lost` flag set,
    /// indicating that a full `Replica::prepare` call must be made. The
    /// `context_lost` flag is then cleared.
    pub fn check_context_lost(&self) -> bool {
        let mut lock = self.status.lock().unwrap();
        let context_lost = lock.context_lost;
        lock.context_lost = false;
        context_lost
    }

    /// Notify all waiters without changing state.
    pub fn notify(&self) {
        self.cond.notify_all();
    }

    /// Sets the `dirty` flag on the `WatchHandle` and notifies all waiters.
    pub fn set_dirty(&self) {
        let mut lock = self.status.lock().unwrap();
        lock.dirty = true;
        self.cond.notify_all();
    }

    /// Sets the `dirty` and `context_lost` flags on the `WatchHandle` and
    /// notifies all waiters.
    pub fn set_context_lost(&self) {
        let mut lock = self.status.lock().unwrap();
        lock.dirty = true;
        lock.context_lost = true;
        self.cond.notify_all();
    }

    /// Delays the caller until the `dirty` flag is set or the process has been
    /// interrupted.
    pub fn wait(&self) {
        let mut lock = self.status.lock().unwrap();
        while !lock.dirty && !interrupt::is_interrupted() {
            lock = self.cond.wait(lock).unwrap();
        }
    }
}

/// Trait for replicas which can send notifications when clean directories have
/// been changed.
pub trait Watch: Replica {
    /// Starts watching for changes to any directories in this replica marked
    /// as clean. When such directories are changed, they are marked as dirty
    /// and the watch is notified.
    ///
    /// These notifications continue until the `WatchHandle` or the `Replica`
    /// is dropped.
    fn watch(&mut self, watch: Weak<WatchHandle>) -> Result<()>;
}
