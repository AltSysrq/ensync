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

#![allow(dead_code)]

use std::ffi::{OsStr, OsString};
use std::sync::{Arc, Mutex};

use sqlite;

use block_xfer::*;
use defs::*;
use errors::*;
use replica::*;
use sql::{SendConnection, StatementEx};
use super::crypt::{MasterKey, decrypt_dir_ver};
use super::dir::*;
use super::storage::*;

impl<S : Storage + 'static> ReplicaDirectory for Arc<Dir<S>> {
    fn full_path(&self) -> &OsStr {
        (**self).full_path()
    }
}

pub struct ServerReplica<S : Storage + 'static> {
    db: Arc<Mutex<SendConnection>>,
    key: Arc<MasterKey>,
    storage: Arc<S>,
    pseudo_root: Arc<Dir<S>>,
    root_name: OsString,
    block_size: usize,
}

impl<S : Storage + 'static> ServerReplica<S> {
    pub fn new(path: &str, key: Arc<MasterKey>,
               storage: Arc<S>, root_name: &str, block_size: usize)
               -> Result<Self> {
        let db = sqlite::Connection::open(path)?;
        db.execute(include_str!("client-schema.sql"))?;
        let db = Arc::new(Mutex::new(SendConnection(db)));

        let pseudo_root = Arc::new(Dir::root(
            db.clone(), key.clone(), storage.clone(), block_size)?);

        Ok(ServerReplica {
            db: db,
            key: key.clone(),
            storage: storage,
            pseudo_root: pseudo_root,
            root_name: root_name.to_owned().into(),
            block_size: block_size,
        })
    }
}

impl<S : Storage + 'static> Replica for ServerReplica<S> {
    type Directory = Arc<Dir<S>>;
    type TransferIn = Option<Box<StreamSource>>;
    type TransferOut = Option<ContentAddressableSource>;

    fn is_dir_dirty(&self, dir: &Arc<Dir<S>>) -> bool {
        let db = self.db.lock().unwrap();
        let clean = db.prepare("SELECT 1 FROM `clean_dir` WHERE `id` = ?1")
            .binding(1, &dir.id[..])
            .exists().unwrap_or(false);
        !clean
    }

    fn set_dir_clean(&self, dir: &Arc<Dir<S>>) -> Result<bool> {
        let (ver, len) = dir.ver_and_len();
        let parent = if let Some(ref parent) = dir.parent {
            sqlite::Value::Binary(parent.id.to_vec())
        } else {
            sqlite::Value::Null
        };

        let db = self.db.lock().unwrap();
        db.prepare("INSERT OR REPLACE INTO `clean_dir` (\
                       `id`, `parent`, `ver`, `len` \
                     ) VALUES ( \
                       ?1, ?2, ?3, ?4 \
                     )")
            .binding(1, &dir.id[..])
            .binding(2, &parent)
            .binding(3, ver as i64)
            .binding(4, len as i64)
            .run()?;
        Ok(true)
    }

    fn root(&self) -> Result<Arc<Dir<S>>> {
        Dir::subdir(self.pseudo_root.clone(), &self.root_name).map(Arc::new)
    }

    fn list(&self, dir: &mut Arc<Dir<S>>) -> Result<Vec<(OsString, FileData)>> {
        dir.list()
    }

    fn rename(&self, dir: &mut Arc<Dir<S>>, old: &OsStr, new: &OsStr)
              -> Result<()> {
        dir.rename(old, new)
    }

    fn remove(&self, dir: &mut Arc<Dir<S>>, target: File) -> Result<()> {
        if target.1.is_dir() {
            let removed = dir.remove_subdir(
                |name, mode, _| if name == target.0 {
                    if FileData::Directory(mode).matches(target.1) {
                        Ok(true)
                    } else {
                        Err(ErrorKind::ExpectationNotMatched.into())
                    }
                } else {
                    Ok(false)
                })?;
            if removed {
                Ok(())
            } else {
                Err(ErrorKind::NotFound.into())
            }
        } else {
            dir.edit(target.0, None, None, |old| match old {
                None => Err(ErrorKind::NotFound.into()),
                Some(old_file) => if old_file.matches(target.1) {
                    Ok(())
                } else {
                    Err(ErrorKind::ExpectationNotMatched.into())
                },
            })?;
            Ok(())
        }
    }

    fn create(&self, dir: &mut Arc<Dir<S>>, source: File,
              xfer: Self::TransferIn) -> Result<FileData> {
        dir.edit(source.0, Some(source.1), xfer, |existing| match existing {
            None => Ok(()),
            Some(_) => Err(ErrorKind::CreateExists.into()),
        }).map(|r| r.expect("Created non-existent file?"))
    }

    fn update(&self, dir: &mut Arc<Dir<S>>, name: &OsStr,
              old: &FileData, new: &FileData, xfer: Self::TransferIn)
              -> Result<FileData> {
        dir.edit(name, Some(new), xfer, |v| match v {
            None => Err(ErrorKind::ExpectationNotMatched.into()),
            Some(existing) => if old.matches(existing) {
                Ok(())
            } else {
                Err(ErrorKind::ExpectationNotMatched.into())
            },
        }).map(|r| r.expect("Updated to non-existent file?"))
    }

    fn chdir(&self, dir: &Arc<Dir<S>>, subdir: &OsStr)
             -> Result<Arc<Dir<S>>> {
        Dir::subdir(dir.clone(), subdir).map(Arc::new)
    }

    fn synthdir(&self, dir: &mut Arc<Dir<S>>, subdir: &OsStr,
                mode: FileMode) -> Arc<Dir<S>> {
        Arc::new(Dir::synthdir(dir.clone(), subdir, mode))
    }

    fn rmdir(&self, dir: &mut Arc<Dir<S>>) -> Result<()> {
        let removed = dir.parent.as_ref().expect("rmdir() on pseudo-root?")
            .remove_subdir(|_, _, id| Ok(*id == dir.id))?;
        if removed {
            Ok(())
        } else {
            Err(ErrorKind::NotFound.into())
        }
    }

    fn transfer(&self, dir: &Arc<Dir<S>>, file: File)
                -> Result<Self::TransferOut> {
        match *file.1 {
            FileData::Regular(_, _, _, ref content) =>
                dir.transfer(file.0, content).map(Some),
            _ => Ok(None),
        }
    }

    fn prepare(&self) -> Result<()> {
        self.storage.for_dir(|id, crypt_ver, length| {
            let ver = decrypt_dir_ver(id, crypt_ver, &self.key);

            let db = self.db.lock().unwrap();
            if !db.prepare("SELECT 1 FROM `clean_dir` \
                            WHERE `id` = ?1 AND (\
                              `ver` != ?2 OR `len` != ?3)")
                .binding(1, &id[..])
                .binding(2, ver as i64)
                .binding(3, length as i64)
                .exists()?
            {
                // Either the directory is still clean (ver and len match), or
                // we don't have any entry for it at all.
                return Ok(());
            }

            // Remove the clean entries for this directory and all its parents.
            let mut next_target = Some(id.to_vec());
            while let Some(target) = next_target {
                next_target = db.prepare("SELECT `parent` FROM `clean_dir` \
                                          WHERE `id` = ?1 AND `parent` IS NOT NULL")
                    .binding(1, &target[..])
                    .first(|s| s.read::<Vec<u8>>(0))?;

                db.prepare("DELETE FROM `clean_dir` WHERE `id` = ?1")
                    .binding(1, &target[..])
                    .run()?;
            }

            Ok(())
        })
    }

    fn clean_up(&self) -> Result<()> {
        self.storage.clean_up();
        Ok(())
    }
}
