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

//! Manages the in-memory state of a POSIX replica directory.
//!
//! This specifically concerns the handling of synthetic directories and
//! computing directory hashes.

use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use keccak::Keccak;

use defs::*;
use errors::*;
use replica::{Replica, ReplicaDirectory};

#[derive(Debug,Clone)]
struct DirContentMut {
    /// If `Some`, this is an as-yet-uncreated synthetic directory which should
    /// be created when needed with the given mode, at which point this field
    /// is updated to `None`.
    synth_mode: Option<FileMode>,
    /// The cumulative hash of this directory's contents.
    ///
    /// This is the XOR of the SHA-3 of each `File` in the directory. This
    /// particular design means that we do not need to worry about ordering or
    /// remember the exact state of every file.
    ///
    /// Note that there is no requirement for this to be cryptograhpically
    /// secure, since it's only used to determine whether a directory needs to
    /// be reexamined.
    hash: HashId,
}

#[derive(Debug)]
struct DirContent {
    /// The fully-quallified path to this directory.
    path: PathBuf,
    /// The name of this directory within its parent.
    name: OsString,
    /// If a parent directory handle exists, that parent. This is needed so
    /// that directories created implicitly via the synthdir mechanism
    /// correctly update the parent's hash.
    parent: Option<DirHandle>,
    /// The shared mutable state of this handle
    mcontent: Mutex<DirContentMut>,
}

/// A (lightweight) handle on a POSIX directory.
///
/// Note that this does not involve an actual native file descriptor or other
/// kernel handle.
///
/// The full path of a directory handle always includes a trailing slash, so it
/// can be used directly with the DAO methods and child paths can be created by
/// simply appending the base name of the child to that of the directory.
#[derive(Debug,Clone)]
pub struct DirHandle(Arc<DirContent>);

impl ReplicaDirectory for DirHandle {
    fn full_path(&self) -> &OsStr {
        &self.0.path.as_os_str()
    }
}

fn kc_update_mode(kc: &mut Keccak, mode: FileMode) {
    let m = mode as u32;
    kc.update(&[m as u8, (m >> 8) as u8,
                (m >> 16) as u8, (m >> 24) as u8])
}

static NUL: &'static [u8] = &[0u8];
const INIT_HASH: HashId = [0;32];

impl DirHandle {
    pub fn root(path: PathBuf) -> Self {
        DirHandle(Arc::new(DirContent {
            path: path,
            name: OsString::new(),
            parent: None,
            mcontent: Mutex::new(DirContentMut {
                synth_mode: None,
                hash: INIT_HASH,
            }),
        }))
    }

    pub fn subdir(&self, name: &OsStr, synth: Option<FileMode>)
                  -> DirHandle {
        DirHandle(Arc::new(DirContent {
            path: self.child(name),
            name: name.to_owned(),
            parent: Some(self.clone()),
            mcontent: Mutex::new(DirContentMut {
                synth_mode: synth,
                hash: INIT_HASH,
            }),
        }))
    }

    pub fn path(&self) -> &Path {
        &self.0.path
    }

    pub fn name(&self) -> &OsStr {
        &self.0.name
    }

    pub fn hash(&self) -> HashId {
        self.0.mcontent.lock().unwrap().hash
    }

    /// Resets the hash of this directory to the hash of the empty directory.
    pub fn reset_hash(&self) {
        self.0.mcontent.lock().unwrap().hash = INIT_HASH
    }

    pub fn parent(&self) -> Option<&DirHandle> {
        self.0.parent.as_ref()
    }

    pub fn child<P : AsRef<Path>>(&self, name: P) -> PathBuf {
        self.0.path.join(name)
    }

    pub fn is_synth(&self) -> bool {
        let locked = self.0.mcontent.lock().unwrap();
        locked.synth_mode.is_some()
    }

    pub fn full_path_with_trailing_slash(&self) -> OsString {
        let mut s = self.0.path.as_os_str().to_owned();
        if OsStr::new("s") != &s {
            s.push("/");
        }
        s
    }

    /// Updates the hash of this directory to account for the presence or
    /// absence of the given file.
    ///
    /// If `file` has not yet been accumulated to the hash, the hash is updated
    /// to reflect a directory containing `file`. Otherwise, the hash is
    /// updated to reflect a directory not containing `file`.
    pub fn toggle_file(&self, file: File) {
        let hash = {
            let mut kc = Keccak::new_sha3_256();
            kc.update(file.0.as_bytes());
            kc.update(NUL);
            match *file.1 {
                FileData::Regular(mode, _, _, ref h) => {
                    kc.update(&[0u8]);
                    kc_update_mode(&mut kc, mode);
                    kc.update(&h[..]);
                },
                FileData::Directory(mode) => {
                    kc.update(&[1u8]);
                    kc_update_mode(&mut kc, mode);
                },
                FileData::Symlink(ref target) => {
                    kc.update(&[2u8]);
                    kc.update(target.as_bytes());
                    kc.update(NUL);
                },
                FileData::Special => {
                    kc.update(&[3u8]);
                },
            }

            let mut hash = [0;32];
            kc.finalize(&mut hash);
            hash
        };

        let mut locked = self.0.mcontent.lock().unwrap();
        for i in 0..32 {
            locked.hash[i] ^= hash[i];
        }
    }

    /// If this directory is an as-yet uncreated synthetic directory, ensures
    /// the parent directory exists and then creates itself using `replica`.
    ///
    /// `xfer` is needed to pass to `Replica::create()`. It should be whatever
    /// the replica uses for "not a file", since this call will only attempt to
    /// create directories.
    pub fn create_if_needed<R : Replica<Directory = Self>>(
        &self, replica: &R, xfer: R::TransferIn) -> Result<()>
        where R::TransferIn : Clone
    {
        let mut locked = self.0.mcontent.lock().unwrap();
        if let Some(mode) = locked.synth_mode {
            let mut parent = self.0.parent.clone().unwrap();
            try!(parent.create_if_needed(replica, xfer.clone()));
            try!(replica.create(&mut parent,
                                File(&self.0.name, &FileData::Directory(mode)),
                                xfer));
            locked.synth_mode = None;
        }
        Ok(())
    }
}
