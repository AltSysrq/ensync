//-
// Copyright (c) 2016, 2017, 2021, Jason Lingle
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

use std::borrow::Cow;
use std::ffi::{OsStr, OsString};
use std::sync::{Arc, Mutex};

use sqlite;

use super::dao::*;
use crate::defs::*;
use crate::errors::*;
use crate::replica::*;
use crate::sql::{AsNBytes, AsNStr};

const T_REGULAR: i64 = 0;
const T_DIRECTORY: i64 = 1;
const T_SYMLINK: i64 = 2;

/// The ancestor replica used in production contexts.
///
/// As described in the module documentation, the replica is stored entirely
/// within a single SQLite database in the local filesystem. Since it does not
/// actually store file data, it supports the `NullTransfer` extension; it also
/// has logic to support `Condemn`.
pub struct AncestorReplica {
    dao: Mutex<Dao>,
}

/// A handle on a directory in an `AncestorReplica`.
///
/// This does not support the `ReplicaDirectory::full_path()` method
/// meaningfully.
#[derive(Clone, Debug)]
pub enum DirHandle {
    /// A real directory, identified simply by its database id.
    Real(RealDir),
    /// A synthetic directory.
    Synth(Arc<Mutex<SynthDir>>),
}

/// Newtype which hides the content of `DirHandle::Real`.
#[derive(Clone, Copy, Debug)]
pub struct RealDir(i64);
/// Represents a possibly-uncreated synthetic directory.
#[derive(Clone, Debug)]
pub struct SynthDir {
    /// The parent directory.
    parent: DirHandle,
    /// If Some, the directory has been created and has the given id.
    /// Otherwise, the directory does not exist.
    id: Option<i64>,
    /// The name of the directory to create as-needed.
    name: OsString,
    /// The mode of the directory to create should we need to.
    mode: FileMode,
}

impl DirHandle {
    /// Gets the numeric handle for this directory.
    ///
    /// If this is a synthetic directory that has not yet been created, returns
    /// Error::NotFound.
    fn get_h(&self) -> Result<i64> {
        match *self {
            DirHandle::Real(RealDir(v)) => Ok(v),
            DirHandle::Synth(ref sd) => {
                if let Some(v) = sd.lock().unwrap().id {
                    Ok(v)
                } else {
                    Err(ErrorKind::NotFound.into())
                }
            }
        }
    }

    /// Gets the numeric handle for this directory.
    ///
    /// If this is a synthetic directory that has not yet been created, it is
    /// created now.
    fn mk_h(&self, dao: &Dao) -> Result<i64> {
        match *self {
            DirHandle::Real(RealDir(v)) => Ok(v),
            DirHandle::Synth(ref sd) => {
                let mut synth = sd.lock().unwrap();
                if let Some(v) = synth.id {
                    Ok(v)
                } else {
                    let parent = synth.parent.mk_h(dao)?;
                    let created = {
                        let fd = FileData::Directory(synth.mode);
                        let file = File(&synth.name, &fd);
                        dao.create(&file.as_entry(parent))?
                    };
                    if let Some(id) = created {
                        synth.id = Some(id);
                        Ok(id)
                    } else {
                        // Something was created where we wanted to place the
                        // synthetic directory. This is very unexpected, so
                        // don't try to handle gracefully.
                        Err(ErrorKind::CreateExists.into())
                    }
                }
            }
        }
    }

    /// Creates a new synthetic subdirectory with this directory as its parent,
    /// and with the given subdirectory name and mode.
    fn push_synth(&self, name: &OsStr, mode: FileMode) -> Self {
        DirHandle::Synth(Arc::new(Mutex::new(SynthDir {
            parent: self.clone(),
            id: None,
            name: name.to_owned(),
            mode: mode,
        })))
    }

    /// Returns whether this directory is synthetic and has not yet been
    /// materialised.
    fn is_deferred(&self) -> bool {
        match *self {
            DirHandle::Real(_) => false,
            DirHandle::Synth(ref sd) => sd.lock().unwrap().id.is_none(),
        }
    }
}

impl ReplicaDirectory for DirHandle {
    fn full_path(&self) -> &OsStr {
        OsStr::new("")
    }
}

impl AncestorReplica {
    /// Creates or opens an ancestor replica on the given path.
    ///
    /// The path is passed directly to SQLite, so special path names apply;
    /// particularly, `":memory:"` can be used to create a temporary in-memory
    /// ancestor replica.
    pub fn open(path: &str) -> sqlite::Result<Self> {
        Ok(AncestorReplica {
            dao: Mutex::new(Dao::open(path)?),
        })
    }
}

trait AsEntry {
    fn as_entry(&self, dir: i64) -> FileEntry;
}

impl AsEntry for FileData {
    fn as_entry(&self, dir: i64) -> FileEntry {
        match *self {
            FileData::Regular(mode, _, time, ref hash) => FileEntry {
                id: -1,
                parent: dir,
                name: Cow::Borrowed(b""),
                typ: T_REGULAR,
                mode: mode as i64,
                mtime: time as i64,
                content: Cow::Borrowed(hash),
            },
            FileData::Directory(mode) => FileEntry {
                id: -1,
                parent: dir,
                name: Cow::Borrowed(b""),
                typ: T_DIRECTORY,
                mode: mode as i64,
                mtime: 0,
                content: Cow::Borrowed(b""),
            },
            FileData::Symlink(ref target) => FileEntry {
                id: -1,
                parent: dir,
                name: Cow::Borrowed(b""),
                typ: T_SYMLINK,
                mode: 0,
                mtime: 0,
                content: Cow::Borrowed(target.as_nbytes()),
            },
            FileData::Special => {
                panic!("Attempt to store Special file in ancestor replica")
            }
        }
    }
}

impl<'a> AsEntry for File<'a> {
    fn as_entry(&self, dir: i64) -> FileEntry {
        let mut e = self.1.as_entry(dir);
        e.name = Cow::Borrowed(self.0.as_nbytes());
        e
    }
}

impl Replica for AncestorReplica {
    type Directory = DirHandle;
    type TransferIn = FileData;
    type TransferOut = ();

    fn is_dir_dirty(&self, _: &DirHandle) -> bool {
        false
    }
    fn set_dir_clean(&self, _: &DirHandle) -> Result<bool> {
        Ok(true)
    }

    fn root(&self) -> Result<DirHandle> {
        Ok(DirHandle::Real(RealDir(0)))
    }

    fn list(&self, dir: &mut DirHandle) -> Result<Vec<(OsString, FileData)>> {
        let mut ret = Vec::new();

        if dir.is_deferred() {
            return Ok(ret);
        }

        let mut nul_error = None;
        let mut invalid_type = None;
        let mut hash_error = false;

        let h = dir.get_h()?;
        let exists = self.dao.lock().unwrap().list(true, h, |e| {
            match e.name.as_nstr() {
                Ok(name) => {
                    if let Some(d) = match e.typ {
                        T_REGULAR => {
                            if 32 != e.content.len() {
                                hash_error = true;
                                None
                            } else {
                                let mut hash = [0; 32];
                                hash.copy_from_slice(&*e.content);
                                Some(FileData::Regular(
                                    e.mode as FileMode,
                                    0,
                                    e.mtime as FileTime,
                                    hash,
                                ))
                            }
                        }
                        T_DIRECTORY => {
                            Some(FileData::Directory(e.mode as FileMode))
                        }
                        T_SYMLINK => match e.content.as_nstr() {
                            Ok(target) => {
                                Some(FileData::Symlink(target.to_owned()))
                            }
                            Err(ne) => {
                                nul_error = Some(ne);
                                None
                            }
                        },
                        t => {
                            invalid_type = Some(t);
                            None
                        }
                    }
                    /* then */
                    {
                        ret.push((name.to_owned(), d));
                    }
                }
                Err(ne) => {
                    nul_error = Some(ne);
                }
            }
        })?;

        if let Some(it) = invalid_type {
            Err(ErrorKind::InvalidAncestorFileType(it).into())
        } else if hash_error {
            Err(ErrorKind::InvalidHash.into())
        } else if let Some(nul_error) = nul_error {
            Err(nul_error.into())
        } else if !exists {
            Err(ErrorKind::NotFound.into())
        } else {
            Ok(ret)
        }
    }

    fn rename(
        &self,
        dir: &mut DirHandle,
        old: &OsStr,
        new: &OsStr,
    ) -> Result<()> {
        let h = dir.get_h()?;
        match self.dao.lock().unwrap().rename(
            h,
            old.as_nbytes(),
            new.as_nbytes(),
        )? {
            RenameStatus::Ok => Ok(()),
            RenameStatus::SourceNotFound => Err(ErrorKind::NotFound.into()),
            RenameStatus::DestExists => Err(ErrorKind::RenameDestExists.into()),
        }
    }

    fn remove(&self, dir: &mut DirHandle, target: File) -> Result<()> {
        let h = dir.get_h()?;
        match self.dao.lock().unwrap().delete(&target.as_entry(h))? {
            DeleteStatus::Ok => Ok(()),
            DeleteStatus::NotFound => Err(ErrorKind::NotFound.into()),
            DeleteStatus::NotMatched => {
                Err(ErrorKind::ExpectationNotMatched.into())
            }
            DeleteStatus::DirNotEmpty => Err(ErrorKind::DirNotEmpty.into()),
        }
    }

    fn create(
        &self,
        dir: &mut DirHandle,
        source: File,
        xfer: FileData,
    ) -> Result<FileData> {
        let dao = self.dao.lock().unwrap();
        let h = dir.mk_h(&*dao)?;
        if dao.create(&File(source.0, &xfer).as_entry(h))?.is_some() {
            Ok(xfer)
        } else {
            Err(ErrorKind::CreateExists.into())
        }
    }

    fn update(
        &self,
        dir: &mut DirHandle,
        name: &OsStr,
        old: &FileData,
        new_nonxfer: &FileData,
        xfer: FileData,
    ) -> Result<FileData> {
        if is_dir(Some(old)) && !is_dir(Some(&xfer)) {
            // Instead of updating, remove old then create new.
            // This gets us an implicit emptyness check and clearing of the
            // condemnation list for free, and breaks any stale handles to the
            // now-non-directory since the new entry will have a different id.
            self.remove(dir, File(name, old))?;
            return self.create(dir, File(name, new_nonxfer), xfer);
        }

        let h = dir.get_h()?;
        match self.dao.lock().unwrap().update(
            &File(name, old).as_entry(h),
            &File(name, &xfer).as_entry(h),
        )? {
            UpdateStatus::Ok => Ok(xfer),
            UpdateStatus::NotFound => Err(ErrorKind::NotFound.into()),
            UpdateStatus::NotMatched => {
                Err(ErrorKind::ExpectationNotMatched.into())
            }
        }
    }

    fn chdir(&self, dir: &DirHandle, subdir: &OsStr) -> Result<DirHandle> {
        let h = dir.get_h()?;
        if let Some(f) = self
            .dao
            .lock()
            .unwrap()
            .get_by_name(h, subdir.as_nbytes())?
        {
            if T_DIRECTORY == f.typ {
                Ok(DirHandle::Real(RealDir(f.id)))
            } else {
                Err(ErrorKind::NotADirectory.into())
            }
        } else {
            Err(ErrorKind::NotFound.into())
        }
    }

    fn synthdir(
        &self,
        dir: &mut DirHandle,
        subdir: &OsStr,
        mode: FileMode,
    ) -> DirHandle {
        dir.push_synth(subdir, mode)
    }

    fn rmdir(&self, dir: &mut DirHandle) -> Result<()> {
        if dir.is_deferred() {
            // If the directory is synthetic and hasn't been created, there's
            // nothing to do.
            return Ok(());
        }

        let h = dir.get_h()?;
        match self.dao.lock().unwrap().delete_raw(h)? {
            DeleteStatus::Ok | DeleteStatus::NotFound => Ok(()),
            DeleteStatus::DirNotEmpty => Err(ErrorKind::DirNotEmpty.into()),
            DeleteStatus::NotMatched => {
                panic!("Got NotMatched from delete_raw()")
            }
        }
    }

    fn transfer(&self, _: &DirHandle, _: File) -> Result<()> {
        Ok(())
    }
}

impl NullTransfer for AncestorReplica {
    fn null_transfer(file: &FileData) -> FileData {
        file.clone()
    }
}

impl Condemn for AncestorReplica {
    fn condemn(&self, dir: &mut DirHandle, name: &OsStr) -> Result<()> {
        let h = dir.get_h()?;
        Ok(self.dao.lock().unwrap().condemn(h, name.as_nbytes())?)
    }

    fn uncondemn(&self, dir: &mut DirHandle, name: &OsStr) -> Result<()> {
        let h = dir.get_h()?;
        Ok(self.dao.lock().unwrap().uncondemn(h, name.as_nbytes())?)
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::defs::test_helpers::*;

    fn new() -> (AncestorReplica, DirHandle) {
        let replica = AncestorReplica::open(":memory:").unwrap();
        let root = replica.root().unwrap();
        (replica, root)
    }

    fn mkreg(
        replica: &AncestorReplica,
        dir: &mut DirHandle,
        name: &str,
        mode: FileMode,
        mtime: FileTime,
        h: u8,
    ) -> Result<FileData> {
        let data = FileData::Regular(mode, 0, mtime, [h; 32]);
        replica.create(dir, File(&oss(name), &data), data.clone())
    }

    fn mkdir(
        replica: &AncestorReplica,
        dir: &mut DirHandle,
        name: &str,
        mode: FileMode,
    ) -> Result<FileData> {
        let data = FileData::Directory(mode);
        replica.create(dir, File(&oss(name), &data), data.clone())
    }

    fn mksym(
        replica: &AncestorReplica,
        dir: &mut DirHandle,
        name: &str,
        target: &str,
    ) -> Result<FileData> {
        let data = FileData::Symlink(oss(target));
        replica.create(dir, File(&oss(name), &data), data.clone())
    }

    #[test]
    fn empty() {
        let (replica, mut root) = new();
        assert!(replica.list(&mut root).unwrap().is_empty());
        assert!(replica.rename(&mut root, &oss("foo"), &oss("bar")).is_err());
        assert!(replica
            .remove(&mut root, File(&oss("foo"), &FileData::Directory(0o666)))
            .is_err());
        assert!(replica
            .update(
                &mut root,
                &oss("foo"),
                &FileData::Directory(0o666),
                &FileData::Directory(0o777),
                FileData::Directory(0o777)
            )
            .is_err());
        assert!(replica.chdir(&root, &oss("foo")).is_err());
    }

    #[test]
    fn create_and_list() {
        let (replica, mut root) = new();

        mkreg(&replica, &mut root, "foo", 0o666, 42, 1).unwrap();
        mkdir(&replica, &mut root, "bar", 0o777).unwrap();
        mksym(&replica, &mut root, "xyzzy", "plugh").unwrap();

        let listed = replica.list(&mut root).unwrap();
        assert_eq!(3, listed.len());
        for (name, data) in listed {
            if oss("foo") == name {
                if let FileData::Regular(mode, _, mtime, hash) = data {
                    assert_eq!(0o666, mode);
                    assert_eq!(42, mtime);
                    assert_eq!([1; 32], hash);
                } else {
                    panic!("`foo` was returned as a non-file: {:?}", data);
                }
            } else if oss("bar") == name {
                if let FileData::Directory(mode) = data {
                    assert_eq!(0o777, mode);
                } else {
                    panic!("`bar` was returned as a non-directory: {:?}", data);
                }
            } else if oss("xyzzy") == name {
                if let FileData::Symlink(target) = data {
                    assert_eq!(oss("plugh"), target);
                } else {
                    panic!("`xyzzy` was returned as a non-link: {:?}", data);
                }
            } else {
                panic!("Unexpected filename returned: {:?}", name);
            }
        }
    }

    #[test]
    fn list_nx() {
        let (replica, mut root) = new();

        mkdir(&replica, &mut root, "foo", 0o666).unwrap();

        let mut subdir = replica.chdir(&root, &oss("foo")).unwrap();
        replica
            .remove(&mut root, File(&oss("foo"), &FileData::Directory(0o666)))
            .unwrap();

        assert!(replica.list(&mut subdir).is_err());
    }

    #[test]
    fn create_alread_exists() {
        let (replica, mut root) = new();

        mkdir(&replica, &mut root, "foo", 0o666).unwrap();
        assert!(mkdir(&replica, &mut root, "foo", 0o666).is_err());
    }

    #[test]
    fn remove_not_matched() {
        let (replica, mut root) = new();

        mkdir(&replica, &mut root, "foo", 0o666).unwrap();
        assert!(replica
            .remove(&mut root, File(&oss("foo"), &FileData::Directory(0o777)))
            .is_err());
    }

    #[test]
    fn remove_dir_not_empty() {
        let (replica, mut root) = new();

        mkdir(&replica, &mut root, "foo", 0o666).unwrap();
        let mut subdir = replica.chdir(&root, &oss("foo")).unwrap();
        mkdir(&replica, &mut subdir, "bar", 0o777).unwrap();

        assert!(replica
            .remove(&mut root, File(&oss("foo"), &FileData::Directory(0o666)))
            .is_err());
    }

    #[test]
    fn update_not_matched() {
        let (replica, mut root) = new();

        mkdir(&replica, &mut root, "foo", 0o666).unwrap();
        assert!(replica
            .update(
                &mut root,
                &oss("foo"),
                &FileData::Directory(0o777),
                &FileData::Directory(0o111),
                FileData::Directory(0o111)
            )
            .is_err());
    }

    #[test]
    fn update_dir_to_dir_doesnt_invalidate() {
        let (replica, mut root) = new();

        mkdir(&replica, &mut root, "foo", 0o666).unwrap();
        let mut subdir = replica.chdir(&root, &oss("foo")).unwrap();
        replica
            .update(
                &mut root,
                &oss("foo"),
                &FileData::Directory(0o666),
                &FileData::Directory(0o111),
                FileData::Directory(0o111),
            )
            .unwrap();

        replica.list(&mut subdir).unwrap();
    }

    #[test]
    fn update_dir_to_nondir_invalidates_handles() {
        let (replica, mut root) = new();

        mkdir(&replica, &mut root, "foo", 0o666).unwrap();
        let mut subdir = replica.chdir(&root, &oss("foo")).unwrap();
        replica
            .update(
                &mut root,
                &oss("foo"),
                &FileData::Directory(0o666),
                &FileData::Symlink(oss("bar")),
                FileData::Symlink(oss("bar")),
            )
            .unwrap();

        assert!(replica.list(&mut subdir).is_err());
    }

    #[test]
    fn update_dir_to_nondir_fails_if_not_empty() {
        let (replica, mut root) = new();

        mkdir(&replica, &mut root, "foo", 0o666).unwrap();
        let mut subdir = replica.chdir(&root, &oss("foo")).unwrap();
        mksym(&replica, &mut subdir, "foo", "bar").unwrap();

        assert!(replica
            .update(
                &mut root,
                &oss("foo"),
                &FileData::Directory(0o666),
                &FileData::Symlink(oss("bar")),
                FileData::Symlink(oss("bar"))
            )
            .is_err());
    }

    #[test]
    fn chdir_into_non_dir() {
        let (replica, mut root) = new();

        mksym(&replica, &mut root, "foo", "bar").unwrap();
        assert!(replica.chdir(&root, &oss("foo")).is_err());
    }

    #[test]
    fn synth_dirs() {
        let (replica, mut root) = new();

        let mut da = replica.synthdir(&mut root, &oss("a"), 0o666);
        let mut db = replica.synthdir(&mut da, &oss("b"), 0o777);
        assert_eq!(0, replica.list(&mut db).unwrap().len());
        assert_eq!(0, replica.list(&mut root).unwrap().len());

        mksym(&replica, &mut db, "foo", "bar").unwrap();
        mksym(&replica, &mut db, "xyzzy", "plugh").unwrap();

        assert_eq!(2, replica.list(&mut db).unwrap().len());

        let l = replica.list(&mut root).unwrap();
        assert_eq!(1, l.len());
        assert_eq!(&oss("a"), &l[0].0);
        if let FileData::Directory(mode) = l[0].1 {
            assert_eq!(0o666, mode);
        } else {
            panic!("File created by synthdir not a directory: {:?}", l[0].1);
        }

        let l = replica.list(&mut da).unwrap();
        assert_eq!(1, l.len());
        assert_eq!(&oss("b"), &l[0].0);
        if let FileData::Directory(mode) = l[0].1 {
            assert_eq!(0o777, mode);
        } else {
            panic!("File created by synthdir not a directory: {:?}", l[0].1);
        }
    }

    #[test]
    fn rmdir_success() {
        let (replica, mut root) = new();

        mkdir(&replica, &mut root, "foo", 0o666).unwrap();
        let mut subdir = replica.chdir(&root, &oss("foo")).unwrap();

        replica.rmdir(&mut subdir).unwrap();
        // Should succeeed even if the directory doesn't exist.
        replica.rmdir(&mut subdir).unwrap();

        assert_eq!(0, replica.list(&mut root).unwrap().len());
    }

    #[test]
    fn rmdir_not_empty() {
        let (replica, mut root) = new();

        mkdir(&replica, &mut root, "foo", 0o666).unwrap();
        let mut subdir = replica.chdir(&root, &oss("foo")).unwrap();
        mksym(&replica, &mut subdir, "foo", "bar").unwrap();

        assert!(replica.rmdir(&mut subdir).is_err());
    }

    #[test]
    fn rmdir_deferred_synthetic() {
        let (replica, mut root) = new();

        let mut subdir = replica.synthdir(&mut root, &oss("foo"), 0o666);
        replica.rmdir(&mut subdir).unwrap();
    }

    #[test]
    fn condemnation() {
        let (replica, mut root) = new();

        mksym(&replica, &mut root, "foo", "plugh").unwrap();
        mksym(&replica, &mut root, "bar", "xyzzy").unwrap();
        replica.condemn(&mut root, &oss("foo")).unwrap();
        replica.condemn(&mut root, &oss("bar")).unwrap();
        replica.uncondemn(&mut root, &oss("foo")).unwrap();

        let l = replica.list(&mut root).unwrap();
        assert_eq!(1, l.len());
        assert_eq!(&oss("foo"), &l[0].0);
    }
}
