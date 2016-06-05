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
//!
//! For the most part, it behaves like an idealised POSIX filesystem, except
//! that the root directory is addressed as "" rather than "/".
//!
//! `set_dir_clean()` is only partially implemented; it does not check for
//! concurrent modifications of any kind. Any tests which simulate these should
//! simply clear the clean flag again.

use std::collections::{HashMap,HashSet};
use std::ffi::{CStr,CString};
use std::sync::{Mutex,MutexGuard};

use defs::*;
use replica::{Replica,ReplicaDirectory,Result,NullTransfer,Condemn};

/// Identifies an operation for purposes of mapping to a fault.
#[derive(Clone,Debug,Hash,PartialEq,Eq)]
pub enum Op {
    /// Matched when `Replica::root()` is called.
    ReadRoot,
    /// Matched when `Replica::list()` is called. The string is the full path
    /// to the directory, eg, "/foo/bar".
    List(CString),
    /// Matched when `Replica::rename()` is called or when an operation
    /// performs a rename internally. The fields are: path, old name.
    Rename(CString,CString),
    /// Matched when `Replica::remove()` is called or when an operation
    /// performs a removal internally. The fields are: path, filename.
    Remove(CString,CString),
    /// Matched when `Replica::create()` is called or when an operation
    /// performs a creation internally.
    Create(CString,CString),
    /// Matched when a directory with the given path is created implicitly due
    /// to creating a file within a synthetic directory.
    CreateSynthetic(CString),
    /// Matched when a file is updated. Fields are: path, filename.
    Update(CString,CString),
    /// Matched when `Replica::chdir()` is used to descend into the given full
    /// path.
    Chdir(CString),
    /// Matched when `Replica::transfer()` is called for the given (path,file).
    /// If the fault fails, this is reflected if the transfer value is actually
    /// used.
    Transfer(CString,CString),
}

/// An entry in a `MemoryReplica` directory.
///
/// This is essentially the same as `FileData`, with slightly different
/// semantics for regular files.
#[derive(Clone,Debug)]
pub enum Entry {
    /// Indicates a subdirectory. The actual contents of the directory can be
    /// found by building its full path and looking in
    /// `MemoryReplicaImpl.dirs`.
    Directory(FileMode),
    Regular(Regular),
    Symlink(CString),
    Special,
}

/// The contents of a directory in a `MemoryReplica`.
#[derive(Clone,Debug,Default)]
pub struct Directory {
    /// The files in this directory. Defaults to empty.
    pub contents: HashMap<CString, Entry>,
    /// The names which have been condemned in this directory.
    pub condemned: HashSet<CString>,
    /// Whether this directory is marked clean. Defaults to false.
    pub clean: bool,
}

/// A regular file as represented within a `MemoryReplica`.
#[derive(Clone,Copy,Debug,PartialEq,Eq)]
pub struct Regular {
    pub mode: FileMode,
    pub size: FileSize,
    pub modified: FileTime,
    /// The reported hash of the file.
    pub hash: HashId,
    /// The _actual_ hash of the file, produced when the file is transferred.
    pub content: HashId,
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
    /// Allows tests to inject errors or concurrent modifications into the
    /// replica.
    ///
    /// Whenever an operation matching the key is about to be performed, the
    /// contained function is invoked. The function may modify the
    /// `MemoryReplicaImpl` however it wants, except that it cannot usefully
    /// manipulate the key of the operation it matched. If the fault function
    /// returns Err, the operation does not take place and that error is
    /// returned from the method.
    pub faults: HashMap<Op, Box<Fn (&mut MemoryReplicaImpl) -> Result<()>>>,
    /// Tables for existing directories.
    ///
    /// The root directory is "". Subdirectories follow UNIX convention,
    /// notated as the full path of the parent directory followed by a "/" and
    /// the name of the subdirectory.
    pub dirs: HashMap<CString, Directory>,
}

impl MemoryReplicaImpl {
    fn empty() -> Self {
        let mut root = HashMap::new();
        root.insert(CString::new("").unwrap(),
                    Default::default());

        MemoryReplicaImpl {
            faults: HashMap::new(),
            dirs: root,
        }
    }

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
    path: CString,
    synthetics: Vec<(CString, CString, FileMode)>,
}

impl ReplicaDirectory for DirHandle {
    fn full_path(&self) -> &CStr {
        &self.path
    }
}

pub struct MemoryReplica {
    data: Mutex<MemoryReplicaImpl>,
    next_hash: u32,
}

/// Returns an `Err` `Result` with an arbitrary error value.
pub fn simple_error<T>() -> Result<T> {
    use std::env::VarError;

    Err(Box::new(VarError::NotPresent))
}

pub fn catpath(a: &CStr, b: &CStr) -> CString {
    let mut cat: Vec<u8> = a.to_bytes().iter().cloned().collect();
    cat.push('/' as u8);
    cat.extend(b.to_bytes().iter());
    CString::new(cat).unwrap()
}

impl MemoryReplica {
    /// Returns a guard providing mutable access to the data underlying the
    /// replica.
    pub fn data(&self) -> MutexGuard<MemoryReplicaImpl> {
        self.data.lock().unwrap()
    }

    pub fn empty() -> Self {
        MemoryReplica {
            data: Mutex::new(MemoryReplicaImpl::empty()),
            next_hash: 1,
        }
    }

    pub fn gen_hash(&mut self) -> HashId {
        let mut h = self.next_hash;
        self.next_hash += 1;

        let mut hash = [0;32];
        hash[0] = (h & 0xFF) as u8; h >>= 8;
        hash[1] = (h & 0xFF) as u8; h >>= 8;
        hash[2] = (h & 0xFF) as u8; h >>= 8;
        hash[3] = (h & 0xFF) as u8;

        hash
    }

    pub fn set_hashes_unknown(&self, dir: &mut DirHandle) {
        let mut d = self.data();
        let contents = d.dirs.get_mut(&dir.path).unwrap();
        for (_, val) in &mut contents.contents {
            if let &mut Entry::Regular(Regular { ref mut hash, .. }) = val {
                *hash = UNKNOWN_HASH;
            }
        };
    }
}

impl Replica for MemoryReplica {
    type Directory = DirHandle;
    type TransferIn = Result<HashId>;
    type TransferOut = Result<HashId>;

    fn is_dir_dirty(&self, dir: &DirHandle) -> bool {
        self.data().dirs.get(&dir.path).map_or(false, |d| !d.clean)
    }

    fn set_dir_clean(&self, dir: &DirHandle) -> Result<bool> {
        let mut d = self.data();
        if let Some(contents) = d.dirs.get_mut(&dir.path) {
            contents.clean = true;
            Ok(true)
        } else {
            simple_error()
        }
    }

    fn root(&self) -> Result<DirHandle> {
        let mut d = self.data();
        try!(d.test_op(&Op::ReadRoot));
        Ok(DirHandle { path: CString::new("").unwrap(),
                       synthetics: vec![] })
    }

    fn list(&self, dir: &mut DirHandle) -> Result<Vec<(CString,FileData)>> {
        let mut d = self.data();
        try!(d.test_op(&Op::List(dir.path.clone())));

        let mut prefices_to_remove = vec![];

        let result = if let Some(contents) = d.dirs.get_mut(&dir.path) {
            // Remove any condemned entries. Note that we need to save such
            // directories until after we exit this if-let since we're
            // borrowing d.dirs.
            for condemned in contents.condemned.drain() {
                if let Some(Entry::Directory(_)) =
                    contents.contents.remove(&condemned)
                {
                    prefices_to_remove.push(catpath(&dir.path, &condemned));
                }
            }

            Ok(contents.contents.iter().map(|(name,val)| {
                (name.clone(), val.into())
            }).collect())
        } else if !dir.synthetics.is_empty() {
            Ok(vec![])
        } else {
            simple_error()
        };

        // Find any subdirectories which would be "recursively" removed via
        // condemnation above. Since `HashMap` doesn't currently have a
        // `filter_keys` or similar method, we need to make another pass to
        // first find what keys to remove, and another to remove them.
        let mut to_remove = vec![];
        for prefix in prefices_to_remove {
            fn is_under(c_big: &CStr, c_prefix: &CStr) -> bool {
                let big = c_big.to_bytes();
                let prefix = c_prefix.to_bytes();

                prefix == big || (
                    prefix.len() < big.len() &&
                    prefix == &big[0..prefix.len()] &&
                    '/' as u8 == big[prefix.len()])
            }

            for tr in d.dirs.keys().filter(|k| is_under(k, &prefix)) {
                to_remove.push(tr.to_owned());
            }
        }

        for tr in to_remove {
            d.dirs.remove(&tr);
        }

        result
    }

    fn rename(&self, dir: &mut DirHandle, old: &CStr, new: &CStr)
              -> Result<()> {
        let mut d = self.data();
        try!(d.test_op(&Op::Rename(dir.path.clone(), old.to_owned())));
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

    fn remove(&self, dir: &mut DirHandle, target: File) -> Result<()> {
        let mut d = self.data();
        try!(d.test_op(&Op::Remove(dir.path.clone(), target.0.to_owned())));
        let dir_with_mode = if let Some(contents) = d.dirs.get_mut(&dir.path) {
            if contents.contents.get(target.0).map_or(
                false, |fd| target.1.matches(&fd.into()))
            {
                let removed = contents.contents.remove(target.0).unwrap();
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
            let dt = d.dirs.remove(&catpath(&dir.path, target.0)).unwrap();
            if !dt.contents.is_empty() {
                // Can't remove it after all
                d.dirs.insert(catpath(&dir.path, target.0), dt);
                d.dirs.get_mut(&dir.path).unwrap().contents.insert(
                    target.0.to_owned(), Entry::Directory(mode));
                return simple_error();
            }
        }

        Ok(())
    }

    fn create(&self, dir: &mut DirHandle, source: File, xfer: Result<HashId>)
              -> Result<FileData> {
        use std::collections::hash_map::Entry::*;

        let mut d = self.data();
        try!(d.test_op(&Op::Create(dir.path.clone(), source.0.to_owned())));

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
            match contents.contents.entry(source.0.to_owned()) {
                Occupied(_) => simple_error(),
                Vacant(entry) => Ok(entry.insert(try!(Entry::from_file_data(
                    &source.1, xfer))).into()),
            }
        } else {
            simple_error()
        };

        if let (&Ok(_), &FileData::Directory(_)) = (&res, source.1) {
            d.dirs.insert(catpath(&dir.path, source.0), Default::default());
        }

        res
    }

    fn update(&self, dir: &mut DirHandle, name: &CStr,
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

        // If we fell through to this point, we need to change the type of the
        // file with non-atomic means.

        // Special case for old=directory (see `Replica::update()`).
        if let &FileData::Directory(mode) = old {
            try!(self.remove(dir, File(name,
                                       &FileData::Directory(mode))));
            return self.create(dir, File(name, new), xfer);
        }

        let mut tmpname_v: Vec<u8> = name.to_bytes().iter().cloned().collect();
        // Blindly assume this doesn't exist. Good enough for tests.
        tmpname_v.extend("!testtmp".bytes());
        let tmpname = CString::new(tmpname_v).unwrap();
        try!(self.rename(dir, name, &tmpname));
        let res = try!(self.create(dir, File(name, new), xfer));
        try!(self.remove(dir, File(&tmpname, old)));
        Ok(res)
    }

    fn chdir(&self, dir: &DirHandle, subdir: &CStr)
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

    fn synthdir(&self, dir: &mut DirHandle, subdir: &CStr, mode: FileMode)
                -> DirHandle {
        let mut sub_path: Vec<u8> =
            dir.path.to_bytes().iter().cloned().collect();
        sub_path.push('/' as u8);
        sub_path.extend(subdir.to_bytes().iter());

        let mut sub = dir.clone();
        sub.path = CString::new(sub_path).unwrap();
        sub.synthetics.push((dir.path.clone(), subdir.to_owned(), mode));
        sub
    }

    fn rmdir(&self, dir: &mut DirHandle) -> Result<()> {
        let mut split = dir.path.to_bytes().rsplitn(2, |u| b'/' == *u);
        let name = CString::new(split.next().unwrap()).unwrap();
        let path = CString::new(split.next().unwrap()).unwrap();
        assert!(split.next().is_none());

        let mut parent = DirHandle {
            path: path,
            synthetics: Default::default(),
        };
        {
            let data = self.data();
            if !data.dirs.contains_key(&dir.path) ||
               !data.dirs.contains_key(&parent.path) {
                return Ok(());
            }
        }

        let parent_data = try!(self.list(&mut parent))
            .into_iter().filter(|&(ref n, _)| n == &name)
            .map(|(_, d)| d).next().unwrap();
        self.remove(&mut parent, File(&name, &parent_data))
    }

    fn transfer(&self, dir: &DirHandle, file: File) -> Result<HashId> {
        let mut d = self.data();
        try!(d.test_op(&Op::Transfer(dir.path.clone(), file.0.to_owned())));

        if let Some(contents) = d.dirs.get(&dir.path) {
            if let Some(&Entry::Regular(Regular { content, .. })) =
                contents.contents.get(file.0)
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

impl NullTransfer for MemoryReplica {
    fn null_transfer(file: &FileData) -> Result<HashId> {
        match file {
            &FileData::Regular(_,_,_,h) => Ok(h),
            _ => simple_error(),
        }
    }
}

impl Condemn for MemoryReplica {
    fn condemn(&self, dir: &mut Self::Directory, file: &CStr) -> Result<()> {
        let mut d = self.data();

        if let Some(contents) = d.dirs.get_mut(&dir.path) {
            contents.condemned.insert(file.to_owned());
            Ok(())
        } else {
            simple_error()
        }
    }

    fn uncondemn(&self, dir: &mut Self::Directory, file: &CStr) -> Result<()> {
        let mut d = self.data();

        if let Some(contents) = d.dirs.get_mut(&dir.path) {
            contents.condemned.remove(file);
            Ok(())
        } else {
            simple_error()
        }
    }

    fn is_condemned(&self, dir: &Self::Directory, file: &CStr)
                    -> Result<bool> {
        let d = self.data();

        if let Some(contents) = d.dirs.get(&dir.path) {
            Ok(contents.condemned.contains(file))
        } else {
            simple_error()
        }
    }
}

#[cfg(test)]
mod test {
    use std::ffi::CString;

    use defs::*;
    use replica::*;
    use super::*;

    fn oss(s: &str) -> CString {
        CString::new(s).unwrap()
    }

    fn init() -> (MemoryReplica, DirHandle) {
        let replica = MemoryReplica::empty();
        let root = replica.root().unwrap();
        (replica, root)
    }

    #[test]
    fn empty() {
        let (replica, mut root) = init();
        assert!(replica.is_dir_dirty(&root));
        assert!(replica.list(&mut root).unwrap().is_empty());
        assert!(replica.rename(&mut root, &oss("foo"), &oss("bar")).is_err());
        assert!(replica.remove(
            &mut root, File(&oss("foo"), &FileData::Special)).is_err());
        assert!(replica.update(&mut root, &oss("foo"),
                               &FileData::Special, &FileData::Directory(0o666),
                               Ok(UNKNOWN_HASH)).is_err());
    }

    #[test]
    fn synthetic_dir_looks_empty() {
        let (replica, mut root) = init();
        let mut synth = replica.synthdir(&mut root, &oss("foo"), 0o777);
        let list = replica.list(&mut synth).unwrap();
        assert!(list.is_empty());
    }

    #[test]
    fn create_regular_file() {
        let (mut replica, mut root) = init();
        let returned = mkreg(&mut replica, &mut root, "foo", 0o777).unwrap();
        match returned {
            FileData::Regular(mode, _, _, hash) => {
                assert!(UNKNOWN_HASH != hash);
                assert_eq!(0o777, mode);
            },
            unexpected => panic!("Unexpected file data: {:?}", unexpected),
        }

        let list = replica.list(&mut root).unwrap();
        assert_eq!(1, list.len());
        let created = &list[0];
        assert_eq!(oss("foo"), created.0);
        match created.1 {
            FileData::Regular(mode, _, _, hash) => {
                assert!(UNKNOWN_HASH != hash);
                assert_eq!(0o777, mode);
            },
            ref unexpected => panic!("Unexpected file data: {:?}", unexpected),
        }
    }

    fn mkreg(replica: &mut MemoryReplica, dir: &mut DirHandle,
             name: &str, mode: FileMode) -> Result<FileData> {
        let hash = replica.gen_hash();
        replica.create(dir, File(&oss(name), &FileData::Regular(
            mode, 1, 0, UNKNOWN_HASH)), Ok(hash))
    }

    #[test]
    fn create_symlink() {
        let (replica, mut root) = init();
        let returned = mksym(&replica, &mut root, "foo", "bar").unwrap();
        match returned {
            FileData::Symlink(target) => assert_eq!(oss("bar"), target),
            unexpected => panic!("Unexpected file data: {:?}", unexpected),
        }

        let list = replica.list(&mut root).unwrap();
        assert_eq!(1, list.len());
        let created = &list[0];
        assert_eq!(oss("foo"), created.0);
        match created.1 {
            FileData::Symlink(ref target) => assert_eq!(oss("bar"), *target),
            ref unexpected => panic!("Unexpected file data: {:?}", unexpected),
        }
    }

    fn mksym(replica: &MemoryReplica, dir: &mut DirHandle,
             name: &str, target: &str) -> Result<FileData> {
        replica.create(dir, File(&oss(name), &FileData::Symlink(
            oss(target))), simple_error())
    }

    #[test]
    fn create_special() {
        let (replica, mut root) = init();
        let returned = mkspec(&replica, &mut root, "foo").unwrap();
        match returned {
            FileData::Special => (),
            unexpected => panic!("Unexpected file data: {:?}", unexpected),
        }

        let list = replica.list(&mut root).unwrap();
        assert_eq!(1, list.len());
        let created = &list[0];
        assert_eq!(oss("foo"), created.0);
        match created.1 {
            FileData::Special => (),
            ref unexpected => panic!("Unexpected file data: {:?}", unexpected),
        }
    }

    fn mkspec(replica: &MemoryReplica, dir: &mut DirHandle,
              name: &str) -> Result<FileData> {
        replica.create(dir, File(&oss(name), &FileData::Special), simple_error())
    }

    #[test]
    fn create_directory() {
        let (replica, mut root) = init();
        let returned = mkdir(&replica, &mut root, "foo", 0o777).unwrap();
        match returned {
            FileData::Directory(mode) => assert_eq!(0o777, mode),
            unexpected => panic!("Unexpected file data: {:?}", unexpected),
        }

        let list = replica.list(&mut root).unwrap();
        assert_eq!(1, list.len());
        let created = &list[0];
        assert_eq!(oss("foo"), created.0);
        match created.1 {
            FileData::Directory(mode) => assert_eq!(0o777, mode),
            ref unexpected => panic!("Unexpected file data: {:?}", unexpected),
        }

        let mut subdir = replica.chdir(&root, &created.0).unwrap();
        assert!(replica.list(&mut subdir).unwrap().is_empty());
    }

    fn mkdir(replica: &MemoryReplica, dir: &mut DirHandle,
             name: &str, mode: FileMode) -> Result<FileData> {
        replica.create(dir, File(&oss(name), &FileData::Directory(mode)),
                       simple_error())
    }

    #[test]
    fn create_directory_already_exists() {
        let (replica, mut root) = init();
        mkdir(&replica, &mut root, "foo", 0o777).unwrap();
        assert!(mkdir(&replica, &mut root, "foo", 0o777).is_err());
    }

    fn hashof(fd: FileData) -> HashId {
        match fd {
            FileData::Regular(_, _, _, hash) => hash,
            _ => panic!("Unexpected file data"),
        }
    }

    #[test]
    fn remove_regular_file() {
        let (mut replica, mut root) = init();
        let hash = hashof(mkreg(&mut replica, &mut root, "foo", 0o777)
                          .unwrap());
        replica.remove(&mut root, File(
            &oss("foo"), &FileData::Regular(0o777, 99, 99, hash))).unwrap();

        assert!(replica.list(&mut root).unwrap().is_empty());
    }

    #[test]
    fn remove_file_wrong_type() {
        let (mut replica, mut root) = init();
        mkreg(&mut replica, &mut root, "foo", 0o777).unwrap();
        assert!(replica.remove(&mut root, File(
            &oss("foo"), &FileData::Special)).is_err());
    }

    #[test]
    fn remove_regular_file_mode_mismatch() {
        let (mut replica, mut root) = init();
        let hash = hashof(mkreg(&mut replica, &mut root, "foo", 0o777)
                          .unwrap());
        assert!(replica.remove(&mut root, File(
            &oss("foo"), &FileData::Regular(0o666, 99, 99, hash))).is_err());
    }

    #[test]
    fn remove_regular_file_hash_mismatch() {
        let (mut replica, mut root) = init();
        mkreg(&mut replica, &mut root, "foo", 0o777).unwrap();
        let hash = replica.gen_hash();
        assert!(replica.remove(&mut root, File(
            &oss("foo"), &FileData::Regular(0o777, 99, 99, hash))).is_err());
    }

    #[test]
    fn remove_symlink() {
        let (replica, mut root) = init();
        mksym(&replica, &mut root, "foo", "bar").unwrap();
        replica.remove(&mut root, File(
            &oss("foo"), &FileData::Symlink(oss("bar")))).unwrap();
        assert!(replica.list(&mut root).unwrap().is_empty());
    }

    #[test]
    fn remove_symlink_target_mismatch() {
        let (replica, mut root) = init();
        mksym(&replica, &mut root, "foo", "xyzzy").unwrap();
        assert!(replica.remove(&mut root, File(
            &oss("foo"), &FileData::Symlink(oss("plugh")))).is_err());
    }

    #[test]
    fn remove_special() {
        let (replica, mut root) = init();
        mkspec(&replica, &mut root, "foo").unwrap();
        replica.remove(&mut root, File(&oss("foo"), &FileData::Special))
            .unwrap();
        assert!(replica.list(&mut root).unwrap().is_empty());
    }

    #[test]
    fn remove_directory() {
        let (replica, mut root) = init();
        mkdir(&replica, &mut root, "foo", 0o777).unwrap();

        let mut subdir = replica.chdir(&root, &oss("foo")).unwrap();
        assert!(replica.list(&mut subdir).unwrap().is_empty());

        replica.remove(&mut root, File(
            &oss("foo"), &FileData::Directory(0o777))).unwrap();
        assert!(replica.list(&mut root).unwrap().is_empty());

        assert!(replica.list(&mut subdir).is_err());
        assert!(replica.chdir(&root, &oss("foo")).is_err());
    }

    #[test]
    fn remove_directory_mode_mismatch() {
        let (replica, mut root) = init();
        mkdir(&replica, &mut root, "foo", 0o777).unwrap();

        let mut subdir = replica.chdir(&root, &oss("foo")).unwrap();
        assert!(replica.list(&mut subdir).unwrap().is_empty());

        assert!(replica.remove(&mut root, File(
            &oss("foo"), &FileData::Directory(0o666))).is_err());
        assert_eq!(1, replica.list(&mut root).unwrap().len());
        assert!(replica.list(&mut subdir).unwrap().is_empty());
        replica.chdir(&root, &oss("foo")).unwrap();
    }

    #[test]
    fn remove_directory_not_empty() {
        let (replica, mut root) = init();
        mkdir(&replica, &mut root, "foo", 0o777).unwrap();

        let mut subdir = replica.chdir(&root, &oss("foo")).unwrap();
        mkdir(&replica, &mut subdir, "bar", 0o777).unwrap();
        assert_eq!(1, replica.list(&mut subdir).unwrap().len());

        assert!(replica.remove(&mut root, File(
            &oss("foo"), &FileData::Directory(0o777))).is_err());
        assert_eq!(1, replica.list(&mut root).unwrap().len());
        assert_eq!(1, replica.list(&mut subdir).unwrap().len());
        replica.chdir(&root, &oss("foo")).unwrap();
        replica.chdir(&subdir, &oss("bar")).unwrap();
    }

    #[test]
    fn rename_regular_file() {
        let (mut replica, mut root) = init();
        let data = mkreg(&mut replica, &mut root, "foo", 0o777)
            .unwrap().clone();
        replica.rename(&mut root, &oss("foo"), &oss("bar")).unwrap();

        let list = replica.list(&mut root).unwrap();
        assert_eq!(1, list.len());
        let file = &list[0];
        assert_eq!(oss("bar"), file.0);
        assert_eq!(data, file.1);
    }

    #[test]
    fn rename_directory() {
        let (replica, mut root) = init();
        mkdir(&replica, &mut root, "foo", 0o777).unwrap();
        let mut subdir_foo = replica.chdir(&root, &oss("foo")).unwrap();
        mkspec(&replica, &mut subdir_foo, "xyzzy").unwrap();

        replica.rename(&mut root, &oss("foo"), &oss("bar")).unwrap();
        assert!(replica.list(&mut subdir_foo).is_err());

        let list = replica.list(&mut root).unwrap();
        assert_eq!(1, list.len());
        let file = &list[0];
        assert_eq!(oss("bar"), file.0);
        assert_eq!(FileData::Directory(0o777), file.1);

        let mut subdir_bar = replica.chdir(&root, &oss("bar")).unwrap();
        assert_eq!(1, replica.list(&mut subdir_bar).unwrap().len());
    }

    #[test]
    fn rename_directory_conflict() {
        let (replica, mut root) = init();
        mkdir(&replica, &mut root, "foo", 0o777).unwrap();
        mkspec(&replica, &mut root, "bar").unwrap();

        let mut subdir = replica.chdir(&root, &oss("foo")).unwrap();
        assert!(replica.list(&mut subdir).unwrap().is_empty());

        assert!(replica.rename(&mut root, &oss("foo"), &oss("bar")).is_err());

        assert!(replica.list(&mut subdir).is_ok());
        assert!(replica.chdir(&root, &oss("foo")).is_ok());
        assert_eq!(2, replica.list(&mut root).unwrap().len());
    }

    #[test]
    fn synthetic_tree_create() {
        let (replica, mut root) = init();
        let mut subdir_foo = replica.synthdir(&mut root, &oss("foo"), 0o777);
        let mut subdir_foo_bar = replica.synthdir(
            &mut subdir_foo, &oss("bar"), 0o666);
        let mut subdir_foo_bar_xyzzy = replica.synthdir(
            &mut subdir_foo_bar, &oss("xyzzy"), 0o444);
        let mut subdir_foo_bar_plugh = replica.synthdir(
            &mut subdir_foo_bar, &oss("plugh"), 0o333);

        mkspec(&replica, &mut subdir_foo_bar_xyzzy, "x").unwrap();
        mkspec(&replica, &mut subdir_foo_bar_plugh, "x").unwrap();

        let root_list = replica.list(&mut root).unwrap();
        assert_eq!(1, root_list.len());
        assert_eq!(oss("foo"), root_list[0].0);
        assert_eq!(FileData::Directory(0o777), root_list[0].1);

        let foo_list = replica.list(&mut subdir_foo).unwrap();
        assert_eq!(1, foo_list.len());
        assert_eq!(oss("bar"), foo_list[0].0);
        assert_eq!(FileData::Directory(0o666), foo_list[0].1);

        let bar_list = replica.list(&mut subdir_foo_bar).unwrap();
        assert_eq!(2, bar_list.len());
        assert_eq!(1, bar_list.iter().filter(|v| oss("xyzzy") == v.0).count());
        assert_eq!(1, bar_list.iter().filter(|v| oss("plugh") == v.0).count());

        let xyzzy_list = replica.list(&mut subdir_foo_bar_xyzzy).unwrap();
        assert_eq!(1, xyzzy_list.len());

        let plugh_list = replica.list(&mut subdir_foo_bar_plugh).unwrap();
        assert_eq!(1, plugh_list.len());
    }

    #[test]
    fn synthetic_create_conflict() {
        let (replica, mut root) = init();
        let mut subdir = replica.synthdir(&mut root, &oss("foo"), 0o777);
        mkspec(&replica, &mut root, "foo").unwrap();
        assert!(mkspec(&replica, &mut subdir, "x").is_err());
    }

    #[test]
    fn update_symlink_target() {
        let (replica, mut root) = init();
        mksym(&replica, &mut root, "foo", "xyzzy").unwrap();
        replica.update(&mut root, &oss("foo"),
                       &FileData::Symlink(oss("xyzzy")),
                       &FileData::Symlink(oss("plugh")),
                       simple_error()).unwrap();

        let list = replica.list(&mut root).unwrap();
        assert_eq!(1, list.len());
        assert_eq!(FileData::Symlink(oss("plugh")), list[0].1);
    }

    #[test]
    fn update_symlink_target_mismatch() {
        let (replica, mut root) = init();
        mksym(&replica, &mut root, "foo", "bar").unwrap();
        assert!(replica.update(&mut root, &oss("foo"),
                               &FileData::Symlink(oss("xyzzy")),
                               &FileData::Symlink(oss("plugh")),
                               simple_error()).is_err());

        let list = replica.list(&mut root).unwrap();
        assert_eq!(1, list.len());
        assert_eq!(FileData::Symlink(oss("bar")), list[0].1);
    }

    #[test]
    fn update_directory_mode() {
        let (replica, mut root) = init();
        mkdir(&replica, &mut root, "foo", 0o777).unwrap();
        let mut subdir = replica.chdir(&root, &oss("foo")).unwrap();
        mkspec(&replica, &mut subdir, "x").unwrap();

        replica.update(&mut root, &oss("foo"),
                       &FileData::Directory(0o777),
                       &FileData::Directory(0o700),
                       simple_error()).unwrap();

        replica.list(&mut subdir).unwrap();

        let list = replica.list(&mut root).unwrap();
        assert_eq!(1, list.len());
        assert_eq!(FileData::Directory(0o700), list[0].1);
    }

    #[test]
    fn update_directory_mode_mismatch() {
        let (replica, mut root) = init();
        mkdir(&replica, &mut root, "foo", 0o777).unwrap();
        let mut subdir = replica.chdir(&root, &oss("foo")).unwrap();
        mkspec(&replica, &mut subdir, "x").unwrap();

        assert!(replica.update(&mut root, &oss("foo"),
                               &FileData::Directory(0o666),
                               &FileData::Directory(0o700),
                               simple_error()).is_err());

        replica.list(&mut subdir).unwrap();

        let list = replica.list(&mut root).unwrap();
        assert_eq!(1, list.len());
        assert_eq!(FileData::Directory(0o777), list[0].1);
    }

    #[test]
    fn update_regular() {
        let (mut replica, mut root) = init();
        let old_hash = replica.gen_hash();
        let new_hash = replica.gen_hash();

        replica.create(
            &mut root, File(
                &oss("foo"), &FileData::Regular(0o666, 0, 0, UNKNOWN_HASH)),
            Ok(old_hash)).unwrap();
        let returned = replica.update(
            &mut root, &oss("foo"),
            &FileData::Regular(0o666, 0, 0, old_hash),
            &FileData::Regular(0o777, 0, 0, UNKNOWN_HASH),
            Ok(new_hash)).unwrap();

        assert_eq!(FileData::Regular(0o777, 0, 0, new_hash), returned);
    }

    #[test]
    fn update_regular_content_mismatch() {
        let (mut replica, mut root) = init();
        let old_hash = replica.gen_hash();
        let new_hash = replica.gen_hash();
        let other_hash = replica.gen_hash();

        replica.create(
            &mut root, File(
                &oss("foo"), &FileData::Regular(0o666, 0, 0, UNKNOWN_HASH)),
            Ok(other_hash)).unwrap();
        assert!(replica.update(
            &mut root, &oss("foo"),
            &FileData::Regular(0o666, 0, 0, old_hash),
            &FileData::Regular(0o777, 0, 0, UNKNOWN_HASH),
            Ok(new_hash)).is_err());
    }

    #[test]
    fn update_regular_mode_mismatch() {
        let (mut replica, mut root) = init();
        let old_hash = replica.gen_hash();
        let new_hash = replica.gen_hash();

        replica.create(
            &mut root, File(
                &oss("foo"), &FileData::Regular(0o666, 0, 0, UNKNOWN_HASH)),
            Ok(old_hash)).unwrap();
        assert!(replica.update(
            &mut root, &oss("foo"),
            &FileData::Regular(0o600, 0, 0, old_hash),
            &FileData::Regular(0o777, 0, 0, UNKNOWN_HASH),
            Ok(new_hash)).is_err());
    }

    #[test]
    fn update_special_into_directory() {
        let (replica, mut root) = init();
        mkspec(&replica, &mut root, "foo").unwrap();
        replica.update(&mut root, &oss("foo"),
                       &FileData::Special,
                       &FileData::Directory(0o777),
                       simple_error()).unwrap();

        let list = replica.list(&mut root).unwrap();
        assert_eq!(1, list.len());
        assert_eq!(FileData::Directory(0o777), list[0].1);

        let mut subdir = replica.chdir(&root, &oss("foo")).unwrap();
        assert!(replica.list(&mut subdir).unwrap().is_empty());
    }

    #[test]
    fn update_directory_into_special() {
        let (replica, mut root) = init();
        mkdir(&replica, &mut root, "foo", 0o777).unwrap();
        let mut subdir = replica.chdir(&root, &oss("foo")).unwrap();
        replica.update(&mut root, &oss("foo"),
                       &FileData::Directory(0o777),
                       &FileData::Special,
                       simple_error()).unwrap();

        let list = replica.list(&mut root).unwrap();
        assert_eq!(1, list.len());
        assert_eq!(FileData::Special, list[0].1);

        assert!(replica.list(&mut subdir).is_err());
        assert!(replica.chdir(&root, &oss("foo")).is_err());
    }

    #[test]
    fn update_nonempty_directory_into_special() {
        let (replica, mut root) = init();
        mkdir(&replica, &mut root, "foo", 0o777).unwrap();
        let mut subdir = replica.chdir(&root, &oss("foo")).unwrap();
        mkspec(&replica, &mut subdir, "bar").unwrap();

        assert!(replica.update(&mut root, &oss("foo"),
                               &FileData::Directory(0o777),
                               &FileData::Special,
                               simple_error()).is_err());

        let list = replica.list(&mut root).unwrap();
        assert_eq!(1, list.len());
        assert_eq!(FileData::Directory(0o777), list[0].1);

        assert_eq!(1, replica.list(&mut subdir).unwrap().len());
        assert!(replica.chdir(&root, &oss("foo")).is_ok());
    }

    #[test]
    fn condemn_single_file() {
        let (replica, mut root) = init();
        mkspec(&replica, &mut root, "foo").unwrap();
        mkspec(&replica, &mut root, "bar").unwrap();

        replica.condemn(&mut root, &oss("foo")).unwrap();
        assert!(replica.is_condemned(&mut root, &oss("foo")).unwrap());

        let condemned_list = replica.list(&mut root).unwrap();
        assert_eq!(1, condemned_list.len());
        assert_eq!(oss("bar"), condemned_list[0].0);

        replica.uncondemn(&mut root, &oss("foo")).unwrap();
        assert!(!replica.is_condemned(&mut root, &oss("foo")).unwrap());

        assert_eq!(1, replica.list(&mut root).unwrap().len());
    }

    #[test]
    fn condemn_dir_tree() {
        let (replica, mut root) = init();
        mkdir(&replica, &mut root, "foo", 0o777).unwrap();
        let mut subdir_foo = replica.chdir(&root, &oss("foo")).unwrap();
        mkdir(&replica, &mut subdir_foo, "bar", 0o777).unwrap();
        let mut subdir_bar = replica.chdir(&subdir_foo, &oss("bar")).unwrap();
        mkspec(&replica, &mut subdir_bar, "xyzzy").unwrap();
        mkdir(&replica, &mut root, "fooo", 0o777).unwrap();

        replica.condemn(&mut root, &oss("foo")).unwrap();
        let condemned_list = replica.list(&mut root).unwrap();
        assert_eq!(1, condemned_list.len());
        assert_eq!(oss("fooo"), condemned_list[0].0);

        assert!(replica.list(&mut subdir_foo).is_err());
        assert!(replica.list(&mut subdir_bar).is_err());
        assert!(replica.list(&mut replica.chdir(&root, &oss("fooo"))
                             .unwrap()).is_ok());
    }
}
