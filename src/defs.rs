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
use std::ffi::OsString;
use libc;

/// Type for content hashes of regular files and for blob identifiers on the
/// server.
///
/// In practise, this is a 256-bit SHA-3 sum.
pub type HashId = [u8;32];
/// The sentinal hash value indicating an uncomputed hash.
///
/// One does not compare hashes against this, since the hashes on files can be
/// out-of-date anyway and must be computed when the file is uploaded in any
/// case.
pub const UNKNOWN_HASH : HashId = [0;32];

// Maybe someday we can move to rust's platform-independent stuff, but time
// support is missing from 1.7.0 and the permissions model is sufficiently
// awkward that it's not really comfortable to trust for file syncing.
pub type FileMode = libc::mode_t;
pub type FileSize = libc::off_t;
pub type FileTime = libc::time_t;

/// Shallow data about a file in the sync process, excluding its name.
#[derive(Clone,Debug,PartialEq,Eq)]
pub enum FileData {
    /// A directory. The only immediate data is its mode. In a file stream, the
    /// receiver must either push the new directory or request it to be
    /// discarded.
    Directory(FileMode),
    /// A regular file. Data is mode, size in bytes, last modified, content
    /// hash. Note that the content hash may be incorrect, and always will be
    /// for files freshly streamed off the client filesystem.
    Regular(FileMode, FileSize, FileTime, HashId),
    /// A symbolic link. The only data is its actual content.
    Symlink(OsString),
    /// Any other type of non-regular file.
    Special,
}

impl FileData {
    /// Returns whether this `FileData` and another one represent the same
    /// content.
    ///
    /// This is slightly less strict than a full equality test, ignoring some
    /// of the fields for regular fiels.
    pub fn matches(&self, that: &FileData) -> bool {
        use self::FileData::*;

        match (self, that) {
            (&Directory(m1), &Directory(m2)) => m1 == m2,
            (&Regular(m1, _, _, ref h1), &Regular(m2, _, _, ref h2)) =>
                m1 == m2 && *h1 == *h2,
            (&Symlink(ref t1), &Symlink(ref t2)) => *t1 == *t2,
            (&Special, &Special) => true,
            _ => false,
        }
    }
}

/// A file, both its name and shallow data.
#[derive(Clone,Debug,PartialEq,Eq)]
pub struct File (pub OsString, pub FileData);

impl File {
    pub fn is_dir(&self) -> bool {
        match self {
            &File (_, FileData::Directory(_)) => true,
            _ => false,
        }
    }
}

/// A single field of a sync mode.
#[derive(Clone,Copy,PartialEq,Eq,Debug,PartialOrd,Ord)]
pub enum SyncModeSetting {
    /// This type of propagation shall not happen.
    Off,
    /// This type of propagation shall happen. Handle conflicts regarding it
    /// conservatively.
    On,
    /// This type of propagation shall happen. In case of conflict, "force" the
    /// resolution to be this particular propagation.
    Force,
}

impl SyncModeSetting {
    /// Returns whether the given setting is in any enabled state.
    pub fn on(self) -> bool {
        self >= SyncModeSetting::On
    }

    /// Returns whether the given setting is in any forced state.
    pub fn force(self) -> bool {
        self >= SyncModeSetting::Force
    }

    fn ch(self, when_on: char, when_force: char) -> char {
        use self::SyncModeSetting::*;

        match self {
            Off => '-',
            On => when_on,
            Force => when_force,
        }
    }
}

/// The sync settings for one direction of propagation.
#[derive(Clone,Copy,PartialEq,Eq,Debug)]
pub struct HalfSyncMode {
    /// Whether creates should happen in this direction.
    pub create: SyncModeSetting,
    /// Whether updates should happen in this direction.
    pub update: SyncModeSetting,
    /// Whether deletes should happen in this direction.
    pub delete: SyncModeSetting,
}

impl fmt::Display for HalfSyncMode {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}{}{}", self.create.ch('c','C'),
               self.update.ch('u','U'), self.delete.ch('d','D'))
    }
}

/// A full description of a sync mode.
#[derive(Clone,Copy,PartialEq,Eq,Debug)]
pub struct SyncMode {
    /// Whether particular types of changes should propagate from server to
    /// client.
    pub inbound: HalfSyncMode,
    /// Whether particular types of changes should propagate from client to
    /// server.
    pub outbound: HalfSyncMode,
}

impl fmt::Display for SyncMode {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}/{}", self.inbound, self.outbound)
    }
}
