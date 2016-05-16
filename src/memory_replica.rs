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

//! An implementation of `Replica` which holds all its contents in memory.
//!
//! This is only used for testing. The contents of regular files are
//! represented simply by their "hash", which itself is simply a 32-byte
//! incrementing counter (starting at 1 so it does not conflict with
//! `UNKNOWN_HASH`).
//!
//! The `MemoryReplica` allows simulating both replicas with a reliable hash
//! for entries (as with the real server) and without (as with the client).
//!
//! Faults can be injected into any operation to test error handling.

use std::collections::HashMap;
use std::ffi::{OsStr,OsString};
use std::sync::{Mutex,MutexGuard};

use defs::*;
use replica::{Replica,Result};

#[derive(Clone,Debug,Hash,PartialEq,Eq)]
pub enum Op {
    ReadRoot,
    List(OsString),
    Rename(OsString,OsString,OsString),
    Remove(OsString,OsString),
    Create(OsString,OsString),
    CreateSynthetic(OsString),
    Update(OsString,OsString),
    Chdir(OsString),
    Transfer(OsString,OsString),
}

#[derive(Clone,Debug)]
pub enum Entry {
    Directory(FileMode),
    Regular(Regular),
    Symlink(OsString),
    Special,
}

#[derive(Clone,Debug,Default)]
pub struct Directory {
    contents: HashMap<OsString, Entry>,
    dirty: bool,
}

#[derive(Clone,Copy,Debug,PartialEq,Eq)]
pub struct Regular {
    mode: FileMode,
    size: FileSize,
    modified: FileTime,
    hash: HashId,
    content: HashId,
}

impl<'a> From<&'a Entry> for FileData {
    fn from(entry: &'a Entry) -> FileData {
        match entry {
            &Entry::Directory(mode) => FileData::Directory(mode),
            &Entry::Regular(reg) => FileData::Regular(
                reg.mode, reg.size, reg.modified, reg.hash),
            &Entry::Symlink(ref target) =>
                FileData::Symlink(target.clone()),
            &Entry::Special => FileData::Special,
        }
    }
}

impl From<Entry> for FileData {
    fn from(entry: Entry) -> FileData {
        (&entry).into()
    }
}

impl<'a> From<&'a mut Entry> for FileData {
    fn from(entry: &'a mut Entry) -> FileData {
        (&*entry).into()
    }
}

impl Entry {
    fn from_file_data(fd: &FileData, xfer: Result<HashId>) -> Result<Entry> {
        match fd {
            &FileData::Directory(mode) => Ok(Entry::Directory(mode)),
            &FileData::Regular(mode, size, modified, _) => {
                let hash = try!(xfer);
                Ok(Entry::Regular(Regular {
                    mode: mode, size: size, modified: modified,
                    hash: hash, content: hash,
                }))
            },
            &FileData::Symlink(ref target) =>
                Ok(Entry::Symlink(target.clone())),
            &FileData::Special => Ok(Entry::Special),
        }
    }
}

pub struct MemoryReplicaImpl {
    faults: HashMap<Op, Box<Fn (&mut MemoryReplicaImpl) -> Result<()>>>,
    dirs: HashMap<OsString, Directory>,
}

impl MemoryReplicaImpl {
    fn test_op(&mut self, op: &Op) -> Result<()> {
        let funo = self.faults.remove(op);
        if let Some(fun) = funo {
            let res = fun(self);
            self.faults.insert(op.clone(), fun);
            res
        } else {
            Ok(())
        }
    }
}

#[derive(Debug,Clone)]
pub struct DirHandle {
    path: OsString,
    synthetics: Vec<(OsString, OsString, FileMode)>,
}

pub struct MemoryReplica {
    data: Mutex<MemoryReplicaImpl>,
}

/// Returns an `Err` `Result` with an arbitrary error value.
pub fn simple_error<T>() -> Result<T> {
    use std::env::VarError;

    Err(Box::new(VarError::NotPresent))
}

fn catpath(a: &OsStr, b: &OsStr) -> OsString {
    let mut cat = a.to_owned();
    cat.push("/");
    cat.push(b);
    cat
}

impl MemoryReplica {
    pub fn data(&self) -> MutexGuard<MemoryReplicaImpl> {
        self.data.lock().unwrap()
    }
}

impl Replica for MemoryReplica {
    type Directory = DirHandle;
    type TransferIn = Result<HashId>;
    type TransferOut = Result<HashId>;

    fn is_dirty_dir(&self, dir: &DirHandle) -> bool {
        self.data().dirs.get(&dir.path).map_or(false, |d| d.dirty)
    }

    fn root(&self) -> Result<DirHandle> {
        let mut d = self.data();
        try!(d.test_op(&Op::ReadRoot));
        Ok(DirHandle { path: "/".to_owned().into(),
                       synthetics: vec![] })
    }

    fn list(&self, dir: &DirHandle) -> Result<Vec<File>> {
        let mut d = self.data();
        try!(d.test_op(&Op::List(dir.path.clone())));
        if let Some(contents) = d.dirs.get(&dir.path) {
            Ok(contents.contents.iter().map(|(name,val)| {
                File(name.clone(), val.into())
            }).collect())
        } else if !dir.synthetics.is_empty() {
            Ok(vec![])
        } else {
            simple_error()
        }
    }

    fn rename(&self, dir: &mut DirHandle, old: &OsStr, new: &OsStr)
              -> Result<()> {
        let mut d = self.data();
        try!(d.test_op(&Op::Rename(dir.path.clone(),
                                   old.to_owned(), new.to_owned())));
        let is_dir = if let Some(contents) = d.dirs.get_mut(&dir.path) {
            if contents.contents.contains_key(new) {
                simple_error()
            } else if let Some(data) = contents.contents.remove(old) {
                let is_dir = match data {
                    Entry::Directory(_) => true,
                    _ => false,
                };
                contents.contents.insert(new.to_owned(), data);
                Ok(is_dir)
            } else {
                simple_error()
            }
        } else {
            simple_error()
        };

        if try!(is_dir) {
            let contents = d.dirs.remove(&catpath(&dir.path, old)).unwrap();
            d.dirs.insert(catpath(&dir.path, new), contents);
        }

        Ok(())
    }

    fn remove(&self, dir: &mut DirHandle, target: &File) -> Result<()> {
        let mut d = self.data();
        try!(d.test_op(&Op::Remove(dir.path.clone(), target.0.clone())));
        let dir_with_mode = if let Some(contents) = d.dirs.get_mut(&dir.path) {
            if contents.contents.get(&target.0).map_or(
                false, |fd| target.1.matches(&fd.into()))
            {
                let removed = contents.contents.remove(&target.0).unwrap();
                match removed {
                    Entry::Directory(mode) => Ok(Some(mode)),
                    _ => Ok(None),
                }
            } else {
                simple_error()
            }
        } else {
            simple_error()
        };

        if let Some(mode) = try!(dir_with_mode) {
            let dt = d.dirs.remove(&catpath(&dir.path, &target.0)).unwrap();
            if !dt.contents.is_empty() {
                // Can't remove it after all
                d.dirs.insert(catpath(&dir.path, &target.0), dt);
                d.dirs.get_mut(&dir.path).unwrap().contents.insert(
                    target.0.clone(), Entry::Directory(mode));
                return simple_error();
            }
        }

        Ok(())
    }

    fn create(&self, dir: &mut DirHandle, source: &File, xfer: Result<HashId>)
              -> Result<FileData> {
        use std::collections::hash_map::Entry::*;

        let mut d = self.data();
        try!(d.test_op(&Op::Create(dir.path.clone(), source.0.clone())));

        for &(ref synth_parent, ref synth_name, synth_mode)
        in &dir.synthetics {
            let full = catpath(synth_parent, synth_name);
            if !d.dirs.contains_key(&full) {
                // Doesn't exist, try to create
                try!(d.test_op(&Op::CreateSynthetic(synth_name.clone())));
                // Make sure the parent exists and there's nothing else there
                // with that name.
                if let Some(parent) = d.dirs.get_mut(synth_parent) {
                    match parent.contents.entry(synth_name.clone()) {
                        Vacant(entry) => {
                            // Ok, create
                            entry.insert(Entry::Directory(synth_mode));
                        },
                        Occupied(_) => {
                            // Can't create, something else is there
                            return simple_error();
                        }
                    }
                }
                // Success, add the directory table
                d.dirs.insert(full, Default::default());
            }
        }

        let res = if let Some(contents) = d.dirs.get_mut(&dir.path) {
            match contents.contents.entry(source.0.clone()) {
                Occupied(_) => simple_error(),
                Vacant(entry) => Ok(entry.insert(try!(Entry::from_file_data(
                    &source.1, xfer))).into()),
            }
        } else {
            simple_error()
        };

        if let (&Ok(_), &FileData::Directory(_)) = (&res, &source.1) {
            d.dirs.insert(catpath(&dir.path, &source.0), Default::default());
        }

        res
    }

    fn update(&self, dir: &mut DirHandle, name: &OsStr,
              old: &FileData, new: &FileData,
              xfer: Result<HashId>) -> Result<FileData> {
        {
            let mut d = self.data();
            try!(d.test_op(&Op::Update(dir.path.clone(), name.to_owned())));

            if let Some(contents) = d.dirs.get_mut(&dir.path) {
                if let Some(entry) = contents.contents.get_mut(name) {
                    if !old.matches(&entry.into()) {
                        return simple_error();
                    } else {
                        match (&*entry, new) {
                            (&Entry::Directory(_), &FileData::Directory(_)) |
                            (&Entry::Regular(_), &FileData::Regular(_,_,_,_)) |
                            (&Entry::Symlink(_), &FileData::Symlink(_)) |
                            (&Entry::Special, &FileData::Special) => {
                                // No type change, we can atomically update the
                                // attributes.
                                *entry = try!(Entry::from_file_data(new, xfer));
                                return Ok(entry.into());
                            },
                            // If the types change, we need to do
                            // rename/create/delete, so fall through
                            _ => (),
                        }
                    }
                } else {
                    return simple_error();
                }
            } else {
                return simple_error();
            }
        };

        let mut tmpname = name.to_owned();
        // Blindly assume this doesn't exist. Good enough for tests.
        tmpname.push("!testtmp");
        try!(self.rename(dir, name, &tmpname));
        let res = try!(self.create(dir, &File(
            name.to_owned(), new.clone()), xfer));
        try!(self.remove(dir, &File(tmpname, old.clone())));
        Ok(res)
    }

    fn chdir(&self, dir: &DirHandle, subdir: &OsStr)
             -> Result<DirHandle> {
        let mut d = self.data();
        let dirname = catpath(&dir.path, subdir);
        try!(d.test_op(&Op::Chdir(dirname.clone())));

        if d.dirs.contains_key(&dirname) {
            Ok(DirHandle {
                synthetics: vec![],
                path: dirname,
            })
        } else {
            simple_error()
        }
    }

    fn synthdir(&self, dir: &DirHandle, subdir: &OsStr, mode: FileMode)
                -> DirHandle {
        let mut sub = dir.clone();
        sub.path.push("/");
        sub.path.push(subdir);
        sub.synthetics.push((dir.path.clone(), subdir.to_owned(), mode));
        sub
    }

    fn transfer(&self, dir: &DirHandle, file: &File) -> Result<HashId> {
        let mut d = self.data();
        try!(d.test_op(&Op::Transfer(dir.path.clone(), file.0.clone())));

        if let Some(contents) = d.dirs.get(&dir.path) {
            if let Some(&Entry::Regular(Regular { content, .. })) =
                contents.contents.get(&file.0)
            {
                Ok(content)
            } else {
                simple_error()
            }
        } else {
            simple_error()
        }
    }
}
