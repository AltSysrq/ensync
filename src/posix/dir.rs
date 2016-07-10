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

//! Manages the in-memory state of a POSIX replica directory.
//!
//! This specifically concerns the handling of synthetic directories and
//! computing directory hashes.

#![allow(dead_code)]

use std::ffi::{OsStr,OsString};
use std::os::unix::ffi::OsStrExt;
use std::sync::{Arc,Mutex};

use keccak::Keccak;

use defs::*;
use replica::{Replica,ReplicaDirectory,Result};

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
    /// The fully-quallified path to this directory, including trailing slash.
    path: OsString,
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
#[derive(Debug,Clone)]
pub struct DirHandle(Arc<DirContent>);

impl ReplicaDirectory for DirHandle {
    fn full_path(&self) -> &OsStr {
        &self.0.path
    }
}

fn kc_update_mode(kc: &mut Keccak, mode: FileMode) {
    let m = mode as u32;
    kc.update(&[m as u8, (m >> 8) as u8,
                (m >> 16) as u8, (m >> 24) as u8])
}

static NUL: &'static [u8] = &[0u8];

impl DirHandle {
    pub fn root(path: OsString) -> Self {
        DirHandle(Arc::new(DirContent {
            path: path,
            name: OsString::new(),
            parent: None,
            mcontent: Mutex::new(DirContentMut {
                synth_mode: None,
                hash: [0;32],
            }),
        }))
    }

    pub fn hash(&self) -> HashId {
        self.0.mcontent.lock().unwrap().hash
    }

    /// Resets the hash of this directory to the hash of the empty directory.
    pub fn reset_hash(&self) {
        self.0.mcontent.lock().unwrap().hash = [0;32]
    }

    /// Updates the hash of this directory to account for the presence or
    /// absence of the given file.
    ///
    /// If `file` has not yet been accumulated to the hash, the hash is updated
    /// to reflect a directory containing `file`. Otherwise, the hash is
    /// updated to reflect a directory not containing `file`.
    pub fn toggle_file(&self, file: &File) {
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
