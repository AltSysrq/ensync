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

use defs::HashId;
use errors::Result;

pub type Tx = u64;

/// Defines the underlying storage model for the server.
///
/// The replica data itself is completely opaque to the server. Thus, the
/// server recognises only two types of objects:
///
/// - "Directories", which are identified by surrogate random ids. These hold
/// all the information needed for `Replica::list`. The server permits fetching
/// a prefix from every directory, and appending new bytes to the end of the
/// directory. Concurrent modifications on a directory are handled by testing
/// the length of the directory as well as a "version id" regenerated each time
/// the directory is rewritten. Note that the version id has additional
/// semantics at the higher layer to detect revert attacks. There is also a
/// "secret version id" which is specified when the directory is created; write
/// operations to a directory specify the secret version, whereas read
/// operations only return the normal version.
///
/// - "Objects", which hold blocks produced by `block_xfer`, identified by
/// HMAC. The server has no way to determine itself whether a block is in use;
/// instead, the client works with the server to maintain a reference count.
pub trait Storage : Send + Sync {
    /// Returns whether the storage system is in a fatal state.
    fn is_fatal(&self) -> bool { false }

    /// Fetches the full content of the directory with the given surrogate id.
    /// If such a directory currently exists, returns the version id and the
    /// committed binary content of the directory. Otherwise, returns
    /// `Ok(None)`.
    fn getdir(&self, id: &HashId) -> Result<Option<(HashId, Vec<u8>)>>;
    /// Returns the object with the given hash id if it exists.
    fn getobj(&self, id: &HashId) -> Result<Option<Vec<u8>>>;

    /// Check whether a directory with the given id, version, and length exists.
    ///
    /// If such a directory does exist, do nothing. If no such directory
    /// exists, place the id into an internal buffer.
    fn check_dir_dirty(&self, id: &HashId, ver: &HashId, len: u32)
                       -> Result<()>;

    /// For each directory id buffered by calls to `check_dir_dirty`, invoke
    /// `f`. The dirty directory buffer is then cleared.
    fn for_dirty_dir(&self, f: &mut FnMut (&HashId) -> Result<()>)
                     -> Result<()>;

    /// Begins a write transaction private to this session.
    ///
    /// `tx` is an arbitrary integer not already used for a transaction id this
    /// session.
    ///
    /// A session can have any number of transactions open at once.
    /// Transactions work similar to in Redis (vs, eg, SQL): Edits are simply
    /// buffered within the transaction without returning results, and then
    /// committing either applies everything or nothing. There is no locking;
    /// if a condition implied by the tranaction's contents is not met, the
    /// transaction is not applied.
    fn start_tx(&self, tx: Tx) -> Result<()>;
    /// Commits a transaction. The changes in the transaction are committed if
    /// and only if all conditions implied by its contents are met.
    ///
    /// Commits are linearisable with other commits and read methods.
    ///
    /// Returns whether the transaction was committed or rejected. If rejected,
    /// the caller should refetch needed data and try again with a new commit.
    fn commit(&self, tx: Tx) -> Result<bool>;
    /// Aborts a transaction.
    fn abort(&self, tx: Tx) -> Result<()>;

    /// Schedules a directory to be created with the id `id`, version `v`,
    /// secret version `sv`, and content `data`.
    ///
    /// The transaction will fail if a directory with the given id exists,
    /// regardless of version or content. The transaction will also fail if a
    /// directory with that id and version exists anywhere globally, including
    /// outside the transaction or possibly in uncommitted transactions.
    fn mkdir(&self, tx: Tx, id: &HashId, v: &HashId, sv: &HashId, data: &[u8])
             -> Result<()>;
    /// Schedules `append` to be appended to the directory with id `id`,
    /// secret version `sv`, and length in bytes `old_len`.
    ///
    /// The transaction will fail if no directory with that id exists, or if it
    /// exists but has a differing version or length.
    ///
    /// The length of the directory is increased by `append.len()` when the
    /// transaction successfully commits.
    fn updir(&self, tx: Tx, id: &HashId, sv: &HashId,
             old_len: u32, append: &[u8])
             -> Result<()>;
    /// Schedules the directory identified by `id` to be removed if it still
    /// has secret version `sv` and length in bytes `old_len`.
    ///
    /// The transaction will fail if no directory with that id exists, or if it
    /// exists but has a differing version or length.
    ///
    /// An `rmdir` followed by a `mkdir` of the same directory in one
    /// transaction is fully atomic.
    fn rmdir(&self, tx: Tx, id: &HashId, sv: &HashId, old_len: u32)
             -> Result<()>;
    /// Adds a link to an object.
    ///
    /// When the transaction is committed, `linkid` will be added to the
    /// object's reference accumulator. `linkid` should be a unique,
    /// randomly-chosen byte array, as "reference counting" is handled by
    /// XORing all `linkid`s together and considering the object unreferenced
    /// if this results in a 0 value. (This means one cannot determine the
    /// exact reference count of an object on the server.)
    ///
    /// If an object identified by `id` exists at the time of this call, the
    /// object is arranged to be available for the transaction to commit even
    /// if its reference count becomes zero externally before the transaction
    /// commits, and `true` is returned.
    ///
    /// If no such object exists at the time of the call, this call has no
    /// effect and the caller must use `putobj` to upload the data for the
    /// object and establish the link.
    fn linkobj(&self, tx: Tx, id: &HashId, linkid: &HashId) -> Result<bool>;
    /// Schedules `data` to be installed to the object with id `id` when the
    /// transaction commits.
    ///
    /// On success, this additionally has the effect of calling `linkobj` with
    /// the same parameters.
    ///
    /// This may have created the object with the specfied data even if it
    /// returns failure. When retrying the transaction, the caller should first
    /// try `linkobj` again to see if this happened or if another process
    /// meanwhile created the object.
    ///
    /// The object may become available to other readers before the transaction
    /// commits.
    fn putobj(&self, tx: Tx, id: &HashId, linkid: &HashId,
              data: &[u8]) -> Result<()>;
    /// Schedules `linkid` to be subtracted from the reference accumulator of
    /// the object identified by `id` when the transaction commits.
    fn unlinkobj(&self, tx: Tx, id: &HashId, linkid: &HashId)
                 -> Result<()>;

    /// Performs cleanup of orphaned data on a best-effort basis.
    ///
    /// Any errors are silently ignored.
    fn clean_up(&self);
}
