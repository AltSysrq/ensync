//-
// Copyright (c) 2017, Jason Lingle
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

use std::collections::{HashMap, HashSet};
use std::ffi::{OsStr, OsString};

use defs::*;
use errors::*;
use replica::*;

#[derive(Debug, Clone)]
pub struct DryRunDirectory<T> {
    path: OsString,
    /// The directory handle of the underlying replica, if any.
    delegate: Option<T>,
    /// The set of directory names created in this directory. This is used so
    /// that `chdir` can emulate the effect of `create()` having created a new
    /// directory.
    new_subdirs: HashSet<OsString>,
    /// Renames performed in this directory. Since the reconciler will never
    /// rename a file more than once, we don't need to handle more complex
    /// things here.
    renames: HashMap<OsString, OsString>,
}

impl<T : ReplicaDirectory> ReplicaDirectory for DryRunDirectory<T> {
    fn full_path(&self) -> &OsStr {
        &self.path
    }
}

/// Wraps a `Replica` to prevent any writes from happening to the underlying
/// replica.
///
/// This is somewhat coupled to the way the reconciler works. For example, its
/// rename emulation is rather limited, and committed writes would not be
/// reflected if a directory were listed more than once. Its write methods also
/// always succeed regardless of the state of the underlying directory.
#[derive(Debug)]
pub struct DryRunReplica<T>(pub T);

impl<T : Replica> Replica for DryRunReplica<T> {
    type Directory = DryRunDirectory<T::Directory>;
    type TransferIn = ();
    type TransferOut = ();

    fn is_fatal(&self) -> bool {
        self.0.is_fatal()
    }

    fn is_dir_dirty(&self, dir: &Self::Directory) -> bool {
        dir.delegate.as_ref().map_or(true, |d| self.0.is_dir_dirty(d))
    }

    fn set_dir_clean(&self, _: &Self::Directory) -> Result<bool> {
        Ok(false)
    }

    fn root(&self) -> Result<Self::Directory> {
        let delegate = self.0.root()?;
        Ok(DryRunDirectory {
            path: delegate.full_path().to_owned(),
            delegate: Some(delegate),
            new_subdirs: HashSet::new(),
            renames: HashMap::new(),
        })
    }

    fn list(&self, dir: &mut Self::Directory)
            -> Result<Vec<(OsString, FileData)>> {
        dir.delegate.as_mut().map_or_else(
            || Ok(Vec::new()), |d| self.0.list(d))
    }

    fn rename(&self, dir: &mut Self::Directory, old: &OsStr, new: &OsStr)
              -> Result<()> {
        dir.renames.insert(new.to_owned(), old.to_owned());
        Ok(())
    }

    fn remove(&self, _: &mut Self::Directory, _: File) -> Result<()> {
        Ok(())
    }

    fn create(&self, dir: &mut Self::Directory, source: File,
              _: ()) -> Result<FileData> {
        if let FileData::Directory(..) = *source.1 {
            dir.new_subdirs.insert(source.0.to_owned());
        }

        Ok(source.1.to_owned())
    }

    fn update(&self, _: &mut Self::Directory, _: &OsStr,
              _: &FileData, new: &FileData, _: ()) -> Result<FileData> {
        Ok(new.to_owned())
    }

    fn chdir(&self, dir: &Self::Directory, name: &OsStr)
             -> Result<Self::Directory> {
        let mut sub_path = dir.full_path().to_owned();
        sub_path.push("/");
        sub_path.push(name);

        let name: &OsStr = dir.renames.get(name).map(|s| &**s).unwrap_or(name);

        if let (Some(d), false) = (dir.delegate.as_ref(),
                                   dir.new_subdirs.contains(name)) {
            Ok(DryRunDirectory {
                path: sub_path,
                delegate: Some(self.0.chdir(d, name)?),
                new_subdirs: HashSet::new(),
                renames: HashMap::new(),
            })
        } else {
            Ok(DryRunDirectory {
                path: sub_path,
                delegate: None,
                new_subdirs: HashSet::new(),
                renames: HashMap::new(),
            })
        }
    }

    fn synthdir(&self, dir: &mut Self::Directory, name: &OsStr, _: FileMode)
                -> Self::Directory {
        let mut sub_path = dir.full_path().to_owned();
        sub_path.push("/");
        sub_path.push(name);

        DryRunDirectory {
            path: sub_path,
            delegate: None,
            new_subdirs: HashSet::new(),
            renames: HashMap::new(),
        }
    }

    fn rmdir(&self, _: &mut Self::Directory) -> Result<()> {
        Ok(())
    }

    fn transfer(&self, _: &Self::Directory, _: File) -> Result<()> {
        Ok(())
    }

    fn prepare(&self, typ: PrepareType) -> Result<()> {
        self.0.prepare(typ)
    }

    fn clean_up(&self) -> Result<()> {
        self.0.clean_up()
    }
}

impl<T : NullTransfer> NullTransfer for DryRunReplica<T> {
    fn null_transfer(_: &FileData) -> () { () }
}

impl<T : Condemn> Condemn for DryRunReplica<T> {
    fn condemn(&self, _: &mut Self::Directory, _: &OsStr) -> Result<()> {
        Ok(())
    }

    fn uncondemn(&self, _: &mut Self::Directory, _: &OsStr) -> Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod test {
    use defs::*;
    use defs::test_helpers::*;
    use replica::*;
    use memory_replica::*;
    use super::DryRunReplica;

    #[test]
    fn mundane_requests_proxied_directly() {
        let mr = MemoryReplica::empty();
        {
            let mut root = mr.root().unwrap();
            let fd = FileData::Directory(0o600);
            mr.create(&mut root, File(&oss("subdir"), &fd),
                      MemoryReplica::null_transfer(&fd)).unwrap();

            let mut subdir = mr.chdir(&root, &oss("subdir")).unwrap();
            let fd = FileData::Symlink(oss("plugh"));
            mr.create(&mut subdir, File(&oss("sym"), &fd),
                      MemoryReplica::null_transfer(&fd)).unwrap();
        }

        let rep = DryRunReplica(mr);
        let mut root = rep.root().unwrap();
        let list = rep.list(&mut root).unwrap();
        assert_eq!(1, list.len());
        assert_eq!(oss("subdir"), list[0].0);

        let mut subdir = rep.chdir(&root, &oss("subdir")).unwrap();
        let list = rep.list(&mut subdir).unwrap();
        assert_eq!(1, list.len());
        assert_eq!(oss("sym"), list[0].0);

        assert!(rep.chdir(&root, &oss("nx")).is_err());
    }

    #[test]
    fn mkdir_emulated() {
        let rep = DryRunReplica(MemoryReplica::empty());
        let mut root = rep.root().unwrap();
        rep.create(&mut root, File(&oss("subdir"),
                                   &FileData::Directory(0o600)), ()).unwrap();

        let mut subdir = rep.chdir(&root, &oss("subdir")).unwrap();
        let list = rep.list(&mut subdir).unwrap();
        assert!(list.is_empty());
    }

    #[test]
    fn dir_rename_emulated() {
        let mr = MemoryReplica::empty();
        {
            let mut root = mr.root().unwrap();
            let fd = FileData::Directory(0o600);
            mr.create(&mut root, File(&oss("subdir"), &fd),
                      MemoryReplica::null_transfer(&fd)).unwrap();

            let mut subdir = mr.chdir(&root, &oss("subdir")).unwrap();
            let fd = FileData::Symlink(oss("plugh"));
            mr.create(&mut subdir, File(&oss("sym"), &fd),
                      MemoryReplica::null_transfer(&fd)).unwrap();
        }

        let rep = DryRunReplica(mr);
        let mut root = rep.root().unwrap();
        let list = rep.list(&mut root).unwrap();
        assert_eq!(1, list.len());
        assert_eq!(oss("subdir"), list[0].0);
        rep.rename(&mut root, &oss("subdir"), &oss("xyzzy")).unwrap();

        let mut subdir = rep.chdir(&root, &oss("xyzzy")).unwrap();
        let list = rep.list(&mut subdir).unwrap();
        assert_eq!(1, list.len());
        assert_eq!(oss("sym"), list[0].0);
    }
}
