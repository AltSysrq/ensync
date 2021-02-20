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

use std::collections::HashSet;
use std::ffi::{OsStr, OsString};
use std::mem;
use std::path::Path;
use std::sync::{Arc, Mutex, Weak};
use std::u32;

use flate2;
use sqlite;

use crate::block_xfer::*;
use crate::defs::*;
use crate::errors::*;
use crate::replica::*;
use crate::sql::{SendConnection, StatementEx};
use super::crypt::{KeyChain, encrypt_dir_ver};
use super::dir::*;
use super::storage::*;

impl<S : Storage + ?Sized + 'static> ReplicaDirectory for Arc<Dir<S>> {
    fn full_path(&self) -> &OsStr {
        (**self).full_path()
    }
}

#[derive(Default)]
struct WatcherStatus {
    dirty: HashSet<HashId>,
}

pub struct ServerReplica<S : Storage + ?Sized + 'static> {
    db: Arc<Mutex<SendConnection>>,
    key: Arc<KeyChain>,
    pseudo_root: Arc<Dir<S>>,
    root_name: OsString,
    watcher: Option<Arc<Mutex<WatcherStatus>>>,
}

impl<S : Storage + ?Sized + 'static> ServerReplica<S> {
    /// Opens a `ServerReplica` on the given parameters.
    ///
    /// `path` indicates the path to use for the client-side SQLite database.
    /// It is passed directly to SQLite, so things like `:memory:` work.
    ///
    /// `key` is the key chain to use for all encryption.
    ///
    /// `storage` provides the underlying data store.
    ///
    /// `root_name` is the name of a directory under the pseudo-root directory
    /// of the server which is used as the true root of the replica.
    ///
    /// `block_size` indicates the block size to use for all new file blocking
    /// operations.
    pub fn new<P : AsRef<Path>>(
        path: P, key: Arc<KeyChain>,
        storage: Arc<S>, root_name: &str,
        block_size: usize, compression: flate2::Compression)
        -> Result<Self>
    {
        let db = sqlite::Connection::open(path)?;
        db.execute(include_str!("client-schema.sql"))?;
        let db = Arc::new(Mutex::new(SendConnection(db)));

        let pseudo_root = Arc::new(Dir::root(
            db.clone(), key.clone(), storage,
            block_size, compression)?);

        Ok(ServerReplica {
            db: db,
            key: key.clone(),
            pseudo_root: pseudo_root,
            root_name: root_name.to_owned().into(),
            watcher: None,
        })
    }

    /// Create the logical root directory if it does not already exist.
    #[cfg(test)]
    pub fn create_root(&self) -> Result<()> {
        self.pseudo_root.edit(&self.root_name, Some(&FileData::Directory(0)),
                              None, |existing| {
            match existing {
                None => Ok(()),
                Some(&FileData::Directory(_)) => Ok(()),
                _ => Err(ErrorKind::NotADirectory.into()),
            }
        })?;
        Ok(())
    }

    /// Returns the pseudo-root of this replica; i.e., the true root of the
    /// storage system.
    pub fn pseudo_root(&self) -> Arc<Dir<S>> {
        self.pseudo_root.clone()
    }

    /// Returns the key chain being used by this replica.
    pub fn key_chain(&self) -> &Arc<KeyChain> {
        &self.key
    }

    fn storage(&self) -> &Arc<S> {
        &self.pseudo_root.storage
    }
}

impl<S : Storage + ?Sized + 'static> Replica for ServerReplica<S> {
    type Directory = Arc<Dir<S>>;
    type TransferIn = Option<Box<dyn StreamSource>>;
    type TransferOut = Option<ContentAddressableSource>;

    fn is_fatal(&self) -> bool {
        self.storage().is_fatal()
    }

    fn is_dir_dirty(&self, dir: &Arc<Dir<S>>) -> bool {
        let db = self.db.lock().unwrap();
        let clean = db.prepare("SELECT 1 FROM `clean_dir` WHERE `id` = ?1")
            .binding(1, &dir.id[..])
            .exists().unwrap_or(false);
        !clean
    }

    fn set_dir_clean(&self, dir: &Arc<Dir<S>>) -> Result<bool> {
        let (ver, len) = dir.ver_and_len()?;
        if !dir.list_up_to_date() { return Ok(false); }
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
        if self.watcher.is_some() {
            let ever = encrypt_dir_ver(&dir.id, ver, &self.key);
            self.storage().watchdir(&dir.id, &ever, len)?;
        }
        Ok(true)
    }

    fn root(&self) -> Result<Arc<Dir<S>>> {
        Dir::subdir(self.pseudo_root.clone(), &self.root_name).map(Arc::new)
            .chain_err(|| format!("Accessing logical server root '{}'",
                                  self.root_name.to_string_lossy()))
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
        if old.is_dir() && !new.is_dir() {
            return Err(ErrorKind::NotADirectory.into());
        }

        dir.edit(name, Some(new), xfer, |v| match v {
            None => Err(ErrorKind::NotFound.into()),
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
        dir.parent.as_ref()
            .ok_or(ErrorKind::RmdirRoot)?
            .remove_subdir(|_, _, id| Ok(*id == dir.id))?;
        Ok(())
    }

    fn transfer(&self, dir: &Arc<Dir<S>>, file: File)
                -> Result<Self::TransferOut> {
        match *file.1 {
            FileData::Regular(_, _, _, ref content) =>
                dir.transfer(file.0, content).map(Some),
            _ => Ok(None),
        }
    }

    fn prepare(&self, typ: PrepareType) -> Result<()> {
        let dir_filter = if let (true, Some(ws)) =
            (typ <= PrepareType::Watched, self.watcher.as_ref())
        {
            Some(mem::replace(&mut ws.lock().unwrap().dirty, HashSet::new()))
        } else {
            None
        };

        let db = self.db.lock().unwrap();
        if typ >= PrepareType::Clean {
            db.prepare("DELETE FROM `clean_dir`").run()?;
        }

        {
            let mut stmt = db.prepare(
                "SELECT `id`, `ver`, `len` FROM `clean_dir`")?;
            while sqlite::State::Done != stmt.next()? {
                let vid: Vec<u8> = stmt.read(0)?;
                let vver: i64 = stmt.read(1)?;
                let ilen: i64 = stmt.read(2)?;

                let mut id = UNKNOWN_HASH;
                if vid.len() != id.len() ||
                    ilen < 0 || ilen > u32::MAX as i64
                {
                    return Err(ErrorKind::InvalidServerDirEntry.into());
                }
                id.copy_from_slice(&vid);

                let ver = encrypt_dir_ver(&id, vver as u64, &self.key);
                self.storage().check_dir_dirty(&id, &ver, ilen as u32)?;
            }
        }

        self.storage().for_dirty_dir(&mut |id| {
            if dir_filter.as_ref().map_or(false, |d| !d.contains(id)) {
                return Ok(());
            }

            // Remove the clean entries for this directory and all its parents.
            let mut next_target = Some(id.to_vec());
            while let Some(target) = next_target {
                next_target = db.prepare("SELECT `parent` FROM `clean_dir` \
                                          WHERE `id` = ?1 AND `parent` IS NOT NULL")
                    .binding(1, &target[..])
                    .first(|s| s.read::<Vec<u8>>(0))
                    .chain_err(|| "Error finding parent of dirty \
                                   server directory")?;

                db.prepare("DELETE FROM `clean_dir` WHERE `id` = ?1")
                    .binding(1, &target[..])
                    .run()
                    .chain_err(|| "Error marking server directory dirty")?;
            }

            Ok(())
        }).chain_err(|| "Error while iterating server directories")
    }

    fn clean_up(&self) -> Result<()> {
        self.storage().clean_up();
        Ok(())
    }
}

impl<S : Storage + ?Sized> Watch for ServerReplica<S> {
    fn watch(&mut self, watch: Weak<WatchHandle>) -> Result<()> {
        if self.watcher.is_some() {
            return Err(ErrorKind::AlreadyWatching.into());
        }

        let status = Arc::new(Mutex::new(WatcherStatus::default()));
        let status2 = status.clone();
        let mut_pr = Arc::get_mut(&mut self.pseudo_root).expect(
            "ServerReplica::watch called while pseudo-root shared");
        Arc::get_mut(&mut mut_pr.storage).expect(
            "ServerReplica::watch called while storage shared")
            .watch(Box::new(move |id| {
                if let Some(id) = id {
                    status2.lock().unwrap().dirty.insert(*id);
                }
                if let Some(wh) = watch.upgrade() {
                    if id.is_some() {
                        wh.set_dirty();
                    } else {
                        wh.set_context_lost();
                    }
                }
            }))?;
        self.watcher = Some(status);
        Ok(())
    }
}

#[cfg(test)]
mod test {
    use std::io::Cursor;
    use std::sync::Arc;
    use std::thread;
    use std::time::Duration;

    use crate::block_xfer;
    use crate::defs::*;
    use crate::defs::test_helpers::*;
    #[allow(unused_imports)] use crate::errors::*;
    use crate::replica::*;
    use crate::server::crypt::KeyChain;
    use crate::server::local_storage::LocalStorage;
    use super::*;

    macro_rules! init {
        ($replica:ident, $root:ident) => {
            init!($replica, $root, key_chain);
        };

        ($replica:ident, $root:ident, $key_chain:ident) => {
            let dir = tempfile::Builder::new().prefix("storage").tempdir().unwrap();
            let storage = LocalStorage::open(dir.path()).unwrap();
            let $key_chain = Arc::new(KeyChain::generate_new());
            let $replica = ServerReplica::new(
                ":memory:", $key_chain.clone(),
                Arc::new(storage), "r00t", 1024,
                flate2::Compression::fast()).unwrap();
            $replica.create_root().unwrap();
            let mut $root = $replica.root().unwrap();
        };
    }

    macro_rules! assert_err {
        ($kind:pat, $x:expr) => { match $x {
            Ok(_) => panic!("Call did not fail"),
            Err(Error($kind, _)) => { },
            Err(Error(ref k, _)) =>
                panic!("Unexpected error kind: {:?}", k),
        } }
    }

    macro_rules! assert_list_one {
        ($replica:expr, $root:expr, $name:expr, $fd:expr) => {
            let list = $replica.list(&mut $root).unwrap();
            assert_eq!(1, list.len());
            assert_eq!(oss($name), list[0].0);
            assert_eq!($fd, list[0].1);

            let mut root = $replica.root().unwrap();
            let list = $replica.list(&mut root).unwrap();
            assert_eq!(1, list.len());
            assert_eq!(oss($name), list[0].0);
            assert_eq!($fd, list[0].1);
        }
    }

    macro_rules! assert_list_none {
        ($replica:expr, $root:expr) => {
            assert!($replica.list(&mut $root).unwrap().is_empty());
            assert!($replica.list(&mut $replica.root().unwrap())
                    .unwrap().is_empty());
        }
    }

    #[test]
    fn empty() {
        init!(replica, root);
        assert!(replica.list(&mut root).unwrap().is_empty());
    }

    #[test]
    fn create_subdir() {
        init!(replica, root);

        let created = replica.create(
            &mut root, File(&oss("sub"), &FileData::Directory(0o770)),
            None).unwrap();
        assert_eq!(FileData::Directory(0o770), created);

        let list = replica.list(&mut root).unwrap();
        assert_eq!(1, list.len());
        assert_eq!(oss("sub"), list[0].0);
        assert_eq!(FileData::Directory(0o770), list[0].1);

        let mut root = replica.root().unwrap();
        let list = replica.list(&mut root).unwrap();
        assert_eq!(1, list.len());
        assert_eq!(oss("sub"), list[0].0);
        assert_eq!(FileData::Directory(0o770), list[0].1);

        let mut sub = replica.chdir(&root, &oss("sub")).unwrap();
        assert!(replica.list(&mut sub).unwrap().is_empty());
    }

    #[test]
    #[allow(unused_mut)]
    fn nx_chdir() {
        init!(replica, root);

        assert_err!(ErrorKind::NotFound,
                    replica.chdir(&root, &oss("sub")));
    }

    #[test]
    fn create_symlink() {
        init!(replica, root);

        let sym = FileData::Symlink(oss("target"));
        let created = replica.create(
            &mut root, File(&oss("sym"), &sym), None).unwrap();
        assert_eq!(sym, created);

        assert_list_one!(replica, root, "sym", sym);
    }

    /// Generates a file of the given length (at least 2).
    ///
    /// The content is simply the Fibonacci sequence modulo 256.
    fn gen_file(len: usize) -> Vec<u8> {
        use std::num::Wrapping;

        let mut v = Vec::new();
        v.push(0u8);
        v.push(1u8);
        while v.len() < len {
            let fib = (Wrapping(v[v.len() - 2]) + Wrapping(v[v.len() - 1])).0;
            v.push(fib);
        }

        v
    }

    impl block_xfer::StreamSource for Cursor<Vec<u8>> {
        fn reset(&mut self) -> Result<()> {
            self.set_position(0);
            Ok(())
        }

        fn finish(&mut self, _: &block_xfer::BlockList) -> Result<()> {
            Ok(())
        }
    }

    #[test]
    fn create_file() {
        init!(replica, root, key_chain);

        let file_data = gen_file(65536);
        let created = replica.create(&mut root, File(
            &oss("fib"), &FileData::Regular(0o660, 65536, 0, UNKNOWN_HASH)),
            Some(Box::new(Cursor::new(file_data.clone())))).unwrap();

        match created {
            FileData::Regular(0o660, 65536, 0, hash) =>
                assert!(hash != UNKNOWN_HASH),

            ref fd => panic!("Unexpected created result: {:?}", fd),
        }

        assert_list_one!(replica, root, "fib", created);

        let xfer = replica.transfer(
            &root, File(&oss("fib"), &created)).unwrap().unwrap();
        let mut actual_data = Vec::<u8>::new();
        block_xfer::blocks_to_stream(
            &xfer.blocks, &mut actual_data,
            key_chain.obj_hmac_secret().unwrap(),
            |h| xfer.fetch.fetch(h)).unwrap();

        assert_eq!(file_data, actual_data);
    }

    #[test]
    fn create_already_exists() {
        init!(replica, root);

        // For conflicts here and below, we have 3 cases:
        //
        // - Conflict via the same directory handle we used to set the conflict
        // up (i.e., up-to-date cache).
        //
        // - Conflict via a fresh directory handle (i.e., nothing cached).
        //
        // - Conflict via a directory handle which has already loaded data
        // before the conflict was set up (i.e., out-of-date cache).
        let mut root2 = replica.root().unwrap();
        replica.list(&mut root2).unwrap();

        let sym = FileData::Symlink(oss("target"));
        let created = replica.create(
            &mut root, File(&oss("sym"), &sym), None).unwrap();
        assert_eq!(sym, created);

        assert_err!(ErrorKind::CreateExists,
                    replica.create(&mut root, File(&oss("sym"), &sym), None));

        let mut root = replica.root().unwrap();
        assert_err!(ErrorKind::CreateExists,
                    replica.create(&mut root, File(&oss("sym"), &sym), None));

        assert_err!(ErrorKind::CreateExists,
                    replica.create(&mut root2, File(&oss("sym"), &sym), None));
    }

    #[test]
    fn update_dir_chmod() {
        init!(replica, root);

        replica.create(
            &mut root, File(&oss("subdir"), &FileData::Directory(0o700)),
            None).unwrap();

        // Create something in the subdirectory to ensure that the
        // implementation doesn't create another new directory when we edit it.
        let mut subdir = replica.chdir(&root, &oss("subdir")).unwrap();
        let subdir_file = FileData::Symlink(oss("target"));
        replica.create(&mut subdir, File(&oss("sym"), &subdir_file), None)
            .unwrap();

        let updated = replica.update(
            &mut root, &oss("subdir"), &FileData::Directory(0o700),
            &FileData::Directory(0o770), None).unwrap();
        assert_eq!(FileData::Directory(0o770), updated);

        assert_list_one!(replica, root, "subdir",
                         FileData::Directory(0o770));

        let mut subdir = replica.chdir(&root, &oss("subdir")).unwrap();
        let list = replica.list(&mut subdir).unwrap();
        assert_eq!(1, list.len());
        assert_eq!(subdir_file, list[0].1);
    }

    #[test]
    fn update_dir_nx() {
        init!(replica, root);

        assert_err!(ErrorKind::NotFound,
                    replica.update(
                        &mut root, &oss("subdir"),
                        &FileData::Directory(0o700),
                        &FileData::Directory(0o770), None));
    }

    #[test]
    fn update_dir_not_matched() {
        init!(replica, root);

        replica.create(
            &mut root, File(&oss("subdir"), &FileData::Directory(0o700)),
            None).unwrap();

        let mut root2 = replica.root().unwrap();
        replica.list(&mut root2).unwrap();

        let updated = replica.update(
            &mut root, &oss("subdir"), &FileData::Directory(0o700),
            &FileData::Directory(0o770), None).unwrap();
        assert_eq!(FileData::Directory(0o770), updated);

        assert_err!(ErrorKind::ExpectationNotMatched,
                    replica.update(
                        &mut root, &oss("subdir"),
                        &FileData::Directory(0o666),
                        &FileData::Directory(0o777), None));

        let mut root = replica.root().unwrap();
        assert_err!(ErrorKind::ExpectationNotMatched,
                    replica.update(
                        &mut root, &oss("subdir"),
                        &FileData::Directory(0o666),
                        &FileData::Directory(0o777), None));

        assert_err!(ErrorKind::ExpectationNotMatched,
                    replica.update(
                        &mut root2, &oss("subdir"),
                        &FileData::Directory(0o666),
                        &FileData::Directory(0o777), None));
    }

    #[test]
    fn update_symlink() {
        init!(replica, root);

        replica.create(
            &mut root, File(&oss("sym"), &FileData::Symlink(oss("target"))),
            None).unwrap();

        let updated = replica.update(
            &mut root, &oss("sym"),
            &FileData::Symlink(oss("target")),
            &FileData::Symlink(oss("tegrat")), None).unwrap();
        assert_eq!(FileData::Symlink(oss("tegrat")), updated);


        assert_list_one!(replica, root, "sym",
                         FileData::Symlink(oss("tegrat")));
    }

    #[test]
    fn update_regular() {
        init!(replica, root, key_chain);

        let file_data_a = gen_file(1024);
        let created = replica.create(&mut root, File(
            &oss("fib"), &FileData::Regular(0o660, 1024, 0, UNKNOWN_HASH)),
            Some(Box::new(Cursor::new(file_data_a.clone())))).unwrap();

        match created {
            FileData::Regular(0o660, 1024, 0, hash) =>
                assert!(hash != UNKNOWN_HASH),

            ref fd => panic!("Unexpected created result: {:?}", fd),
        }

        let file_data_b = gen_file(2048);
        let updated = replica.update(
            &mut root, &oss("fib"),
            &created, &FileData::Regular(0o666, 2048, 1, UNKNOWN_HASH),
            Some(Box::new(Cursor::new(file_data_b.clone())))).unwrap();

        match updated {
            FileData::Regular(0o666, 2048, 1, hash) =>
                assert!(hash != UNKNOWN_HASH),

            ref fd => panic!("Unexpected updated result: {:?}", fd),
        }

        assert_list_one!(replica, root, "fib", updated);

        // Clean up in case doing so causes the shared part of the file to be
        // lost.
        replica.clean_up().unwrap();

        let xfer = replica.transfer(
            &root, File(&oss("fib"), &updated)).unwrap().unwrap();
        let mut actual_data = Vec::<u8>::new();
        block_xfer::blocks_to_stream(
            &xfer.blocks, &mut actual_data,
            key_chain.obj_hmac_secret().unwrap(),
            |h| xfer.fetch.fetch(h)).unwrap();

        assert_eq!(file_data_b, actual_data);
    }

    #[test]
    fn remove_symlink() {
        init!(replica, root);

        replica.create(
            &mut root, File(&oss("sym"), &FileData::Symlink(oss("target"))),
            None).unwrap();

        replica.remove(
            &mut root, File(&oss("sym"), &FileData::Symlink(oss("target"))))
            .unwrap();

        assert_list_none!(replica, root);
    }

    #[test]
    fn remove_symlink_not_matched() {
        init!(replica, root);

        replica.create(
            &mut root, File(&oss("sym"), &FileData::Symlink(oss("target"))),
            None).unwrap();

        let mut root2 = replica.root().unwrap();
        replica.list(&mut root2).unwrap();

        assert_err!(
            ErrorKind::ExpectationNotMatched,
            replica.remove(&mut root,
                           File(&oss("sym"), &FileData::Symlink(oss("x")))));

        let mut root = replica.root().unwrap();
        assert_err!(
            ErrorKind::ExpectationNotMatched,
            replica.remove(&mut root,
                           File(&oss("sym"), &FileData::Symlink(oss("x")))));

        assert_err!(
            ErrorKind::ExpectationNotMatched,
            replica.remove(&mut root2,
                           File(&oss("sym"), &FileData::Symlink(oss("x")))));
    }

    #[test]
    fn remove_symlink_nx() {
        init!(replica, root);

        assert_err!(
            ErrorKind::NotFound,
            replica.remove(&mut root,
                           File(&oss("sym"), &FileData::Symlink(oss("x")))));
    }

    #[test]
    fn remove_regular() {
        init!(replica, root);

        let file_data_a = gen_file(1024);
        let created = replica.create(&mut root, File(
            &oss("fib"), &FileData::Regular(0o660, 1024, 0, UNKNOWN_HASH)),
            Some(Box::new(Cursor::new(file_data_a.clone())))).unwrap();

        replica.remove(&mut root, File(&oss("fib"), &created)).unwrap();

        assert_list_none!(replica, root);
    }

    #[test]
    fn remove_subdirectory() {
        init!(replica, root);

        let dir = FileData::Directory(0o700);
        replica.create(&mut root, File(&oss("sub"), &dir), None).unwrap();

        let mut subdir = replica.chdir(&root, &oss("sub")).unwrap();

        replica.remove(&mut root, File(&oss("sub"), &dir)).unwrap();
        assert_list_none!(replica, root);

        assert_err!(ErrorKind::DirectoryMissing, replica.list(&mut subdir));
    }

    #[test]
    fn remove_subdirectory_not_empty() {
        init!(replica, root);

        let dir = FileData::Directory(0o700);
        replica.create(&mut root, File(&oss("sub"), &dir), None).unwrap();

        let mut subdir = replica.chdir(&root, &oss("sub")).unwrap();
        replica.create(&mut subdir, File(&oss("ss"), &dir), None).unwrap();

        assert_err!(ErrorKind::DirNotEmpty,
                    replica.remove(&mut root, File(&oss("sub"), &dir)));

        // Make sure everything is still there
        let mut subdir = replica.chdir(&root, &oss("sub")).unwrap();
        assert_eq!(1, replica.list(&mut subdir).unwrap().len());
    }

    #[test]
    fn remove_subdirectory_not_matched() {
        init!(replica, root);

        let dir = FileData::Directory(0o700);
        replica.create(&mut root, File(&oss("sub"), &dir), None).unwrap();

        assert_err!(ErrorKind::ExpectationNotMatched,
                    replica.remove(&mut root, File(
                        &oss("sub"), &FileData::Directory(0o777))));

        // Make sure everything is still there
        let mut subdir = replica.chdir(&root, &oss("sub")).unwrap();
        assert_eq!(0, replica.list(&mut subdir).unwrap().len());
    }

    #[test]
    fn remove_subdirectory_nx() {
        init!(replica, root);

        let dir = FileData::Directory(0o700);
        replica.create(&mut root, File(&oss("sub"), &dir), None).unwrap();

        assert_err!(ErrorKind::NotFound,
                    replica.remove(&mut root, File(&oss("x"), &dir)));

        // Make sure everything is still there
        let mut subdir = replica.chdir(&root, &oss("sub")).unwrap();
        assert_eq!(0, replica.list(&mut subdir).unwrap().len());
    }

    #[test]
    fn rmdir() {
        init!(replica, root);

        let dir = FileData::Directory(0o700);
        replica.create(&mut root, File(&oss("sub"), &dir), None).unwrap();

        let mut subdir = replica.chdir(&root, &oss("sub")).unwrap();
        replica.rmdir(&mut subdir).unwrap();

        assert_list_none!(replica, root);
    }

    #[test]
    fn rmdir_not_empty() {
        init!(replica, root);

        let dir = FileData::Directory(0o700);
        replica.create(&mut root, File(&oss("sub"), &dir), None).unwrap();

        let mut subdir = replica.chdir(&root, &oss("sub")).unwrap();
        let mut subdir2 = replica.chdir(&root, &oss("sub")).unwrap();
        replica.create(&mut subdir, File(&oss("ss"), &dir), None).unwrap();

        assert_err!(ErrorKind::DirNotEmpty, replica.rmdir(&mut subdir));
        assert_err!(ErrorKind::DirNotEmpty, replica.rmdir(&mut subdir2));

        assert_list_one!(replica, root, "sub", dir);
    }

    #[test]
    fn rmdir_nx() {
        init!(replica, root);

        let dir = FileData::Directory(0o700);
        replica.create(&mut root, File(&oss("sub"), &dir), None).unwrap();

        let mut subdir = replica.chdir(&root, &oss("sub")).unwrap();
        let mut subdir2 = replica.chdir(&root, &oss("sub")).unwrap();
        replica.rmdir(&mut subdir).unwrap();

        replica.rmdir(&mut subdir).unwrap();
        replica.rmdir(&mut subdir2).unwrap();

        assert_list_none!(replica, root);
    }

    #[test]
    fn rename() {
        init!(replica, root);

        let dir = FileData::Directory(0o700);
        replica.create(&mut root, File(&oss("sub"), &dir), None).unwrap();

        replica.rename(&mut root, &oss("sub"), &oss("bus")).unwrap();

        assert_list_one!(replica, root, "bus", dir);
    }

    #[test]
    fn rename_nx() {
        init!(replica, root);

        assert_err!(ErrorKind::NotFound,
                    replica.rename(&mut root, &oss("sub"), &oss("bus")));
    }

    #[test]
    fn rename_exists() {
        init!(replica, root);

        let dir = FileData::Directory(0o700);
        replica.create(&mut root, File(&oss("sub"), &dir), None).unwrap();
        replica.create(&mut root, File(&oss("bus"), &dir), None).unwrap();

        assert_err!(ErrorKind::RenameDestExists,
                    replica.rename(&mut root, &oss("sub"), &oss("bus")));

        assert_eq!(2, replica.list(&mut root).unwrap().len());
    }

    #[test]
    fn directory_rewrite_works() {
        init!(replica, root);

        replica.create(
            &mut root, File(&oss("sub"), &FileData::Directory(0o100)),
            None).unwrap();

        for mode in 0o101..0o1000 {
            let dir = FileData::Directory(mode);
            replica.update(
                &mut root, &oss("sub"),
                &FileData::Directory(mode - 1), &dir, None).unwrap();

            assert_list_one!(replica, root, "sub", dir);
        }
    }

    #[test]
    fn synthdir_into_existing() {
        init!(replica, root);

        replica.create(
            &mut root, File(&oss("sub"), &FileData::Directory(0o700)),
            None).unwrap();

        let mut subdir = replica.chdir(&root, &oss("sub")).unwrap();
        let subdir_file = FileData::Symlink(oss("target"));
        replica.create(&mut subdir, File(&oss("sym"), &subdir_file), None)
            .unwrap();

        let mut root = replica.root().unwrap();
        let mut subdir = replica.synthdir(&mut root, &oss("sub"), 0o600);

        let list = replica.list(&mut subdir).unwrap();
        assert_eq!(1, list.len());
        assert_eq!(subdir_file, list[0].1);

        assert_list_one!(replica, root, "sub", FileData::Directory(0o700));
    }

    #[test]
    fn synthdir_new_single() {
        init!(replica, root);

        let mut subdir = replica.synthdir(&mut root, &oss("sub"), 0o700);
        let subdir_file = FileData::Symlink(oss("target"));
        replica.create(&mut subdir, File(&oss("sym"), &subdir_file), None)
            .unwrap();
        replica.create(&mut subdir, File(&oss("mys"), &subdir_file), None)
            .unwrap();

        assert_list_one!(replica, root, "sub", FileData::Directory(0o700));
        let mut subdir = replica.chdir(&root, &oss("sub")).unwrap();
        let list = replica.list(&mut subdir).unwrap();
        assert_eq!(2, list.len());
        assert_eq!(subdir_file, list[0].1);
        assert_eq!(subdir_file, list[1].1);
    }

    #[test]
    fn synthdir_nested() {
        init!(replica, root);

        let mut subone = replica.synthdir(&mut root, &oss("one"), 0o700);
        let mut subtwo = replica.synthdir(&mut subone, &oss("two"), 0o777);

        let two_file = FileData::Symlink(oss("target"));
        replica.create(&mut subtwo, File(&oss("sym"), &two_file), None)
            .unwrap();
        replica.create(&mut subtwo, File(&oss("mys"), &two_file), None)
            .unwrap();

        assert_list_one!(replica, root, "one", FileData::Directory(0o700));

        let list = replica.list(&mut subone).unwrap();
        assert_eq!(1, list.len());
        assert_eq!(FileData::Directory(0o777), list[0].1);

        let mut subone = replica.chdir(&root, &oss("one")).unwrap();
        let list = replica.list(&mut subone).unwrap();
        assert_eq!(1, list.len());
        assert_eq!(FileData::Directory(0o777), list[0].1);

        let list = replica.list(&mut subtwo).unwrap();
        assert_eq!(2, list.len());
        assert_eq!(two_file, list[0].1);
        assert_eq!(two_file, list[1].1);

        let mut subtwo = replica.chdir(&subone, &oss("two")).unwrap();
        let list = replica.list(&mut subtwo).unwrap();
        assert_eq!(2, list.len());
        assert_eq!(two_file, list[0].1);
        assert_eq!(two_file, list[1].1);
    }

    #[test]
    fn synthdir_with_invalid_config_cannot_materialise() {
        init!(replica, root);

        let mut subone = replica.synthdir(
            &mut root, &oss("one.ensync[invalid]"), 0o700);
        assert!(replica.create(&mut subone,
                               File(&oss("dir"), &FileData::Directory(0o700)),
                               None).is_err());
    }

    #[test]
    fn cleanup_removes_orphaned_blobs() {
        init!(replica, root, key_chain);

        let file_data = gen_file(65536);
        let file_uh = FileData::Regular(0o666, 65536, 0, UNKNOWN_HASH);

        let created = replica.create(
            &mut root, File(&oss("a"), &file_uh),
            Some(Box::new(Cursor::new(file_data.clone())))).unwrap();

        // Instead of inspecting the raw storage layer, we instead hold on to a
        // transfer object even after running cleanup, and check whether we can
        // still retrieve the underlying objects.
        let xfer = replica.transfer(&root, File(&oss("a"), &created))
            .unwrap().unwrap();

        macro_rules! test {
            ($key_chain:expr, $xfer:expr, $which:ident) => { {
                let mut actual_data: Vec<u8> = Vec::new();
                assert!(block_xfer::blocks_to_stream(
                    &xfer.blocks, &mut actual_data,
                    key_chain.obj_hmac_secret().unwrap(),
                    |h| xfer.fetch.fetch(h)).$which());
            } }
        }

        test!(key_chain, xfer, is_ok);
        replica.clean_up().unwrap();
        test!(key_chain, xfer, is_ok);

        // Increase the link count to 2
        replica.create(
            &mut root, File(&oss("b"), &file_uh),
            Some(Box::new(Cursor::new(file_data.clone())))).unwrap();
        test!(key_chain, xfer, is_ok);
        replica.clean_up().unwrap();
        test!(key_chain, xfer, is_ok);

        // Delete a, reducing the link count to 1 (and thus the transfer
        // remains valid).
        replica.remove(&mut root, File(&oss("a"), &created)).unwrap();
        test!(key_chain, xfer, is_ok);
        replica.clean_up().unwrap();
        test!(key_chain, xfer, is_ok);

        // Delete b, reducing the link count to 0. The object lingers until
        // cleanup is run.
        replica.remove(&mut root, File(&oss("b"), &created)).unwrap();
        test!(key_chain, xfer, is_ok);
        replica.clean_up().unwrap();
        test!(key_chain, xfer, is_err);
    }

    #[test]
    fn clean_dirty_tracking() {
        let dir = tempfile::Builder::new().prefix("storage").tempdir().unwrap();
        let key_chain = Arc::new(KeyChain::generate_new());

        let storage1 = LocalStorage::open(dir.path()).unwrap();
        let replica1 = ServerReplica::new(
            ":memory:", key_chain.clone(),
            Arc::new(storage1), "r00t", 1024,
            flate2::Compression::fast()).unwrap();
        replica1.create_root().unwrap();
        let mut root1 = replica1.root().unwrap();

        let storage2 = LocalStorage::open(dir.path()).unwrap();
        let replica2 = ServerReplica::new(
            ":memory:", key_chain.clone(),
            Arc::new(storage2), "r00t", 1024,
            flate2::Compression::fast()).unwrap();
        let root2 = replica1.root().unwrap();

        assert!(replica1.is_dir_dirty(&root1));
        replica1.list(&mut root1).unwrap();
        replica1.create(
            &mut root1, File(&oss("sub"), &FileData::Directory(0o700)),
            None).unwrap();
        let mut subdir1 = replica1.chdir(&root1, &oss("sub")).unwrap();
        assert!(replica1.is_dir_dirty(&subdir1));
        replica1.list(&mut subdir1).unwrap();
        replica1.create(
            &mut subdir1, File(&oss("sym"), &FileData::Symlink(oss("target"))),
            None).unwrap();

        replica1.create(
            &mut root1, File(&oss("other"), &FileData::Directory(0o700)),
            None).unwrap();
        let mut otherdir1 = replica1.chdir(&root1, &oss("other")).unwrap();
        replica1.list(&mut otherdir1).unwrap();

        replica1.set_dir_clean(&root1).unwrap();
        replica1.set_dir_clean(&subdir1).unwrap();
        replica1.set_dir_clean(&otherdir1).unwrap();

        assert!(!replica1.is_dir_dirty(&root1));
        assert!(!replica1.is_dir_dirty(&subdir1));
        assert!(!replica1.is_dir_dirty(&otherdir1));
        replica1.prepare(PrepareType::Fast).unwrap();
        assert!(!replica1.is_dir_dirty(&root1));
        assert!(!replica1.is_dir_dirty(&subdir1));
        assert!(!replica1.is_dir_dirty(&otherdir1));

        assert!(replica2.is_dir_dirty(&root2));
        let mut subdir2 = replica2.chdir(&root2, &oss("sub")).unwrap();
        assert!(replica2.is_dir_dirty(&subdir2));
        replica2.update(
            &mut subdir2, &oss("sym"),
            &FileData::Symlink(oss("target")),
            &FileData::Symlink(oss("tegrat")), None).unwrap();

        replica1.prepare(PrepareType::Fast).unwrap();
        assert!(replica1.is_dir_dirty(&root1));
        assert!(replica1.is_dir_dirty(&subdir1));
        assert!(!replica1.is_dir_dirty(&otherdir1));
    }

    #[test]
    fn setting_dir_clean_is_noop_if_concurrently_modified() {
        let dir = tempfile::Builder::new().prefix("storage").tempdir().unwrap();
        let key_chain = Arc::new(KeyChain::generate_new());

        let storage1 = LocalStorage::open(dir.path()).unwrap();
        let replica1 = ServerReplica::new(
            ":memory:", key_chain.clone(),
            Arc::new(storage1), "r00t", 1024,
            flate2::Compression::fast()).unwrap();
        replica1.create_root().unwrap();
        let mut root1 = replica1.root().unwrap();

        let storage2 = LocalStorage::open(dir.path()).unwrap();
        let replica2 = ServerReplica::new(
            ":memory:", key_chain.clone(),
            Arc::new(storage2), "r00t", 1024,
            flate2::Compression::fast()).unwrap();
        let mut root2 = replica1.root().unwrap();

        // Process 1 gets a snapshot of the root
        replica1.list(&mut root1).unwrap();
        // But then process 2 modifies the root
        replica2.create(&mut root2, File(
            &oss("foo"), &FileData::Directory(0o700)), None).unwrap();
        // And then process 1 tries to mark the root as clean, even though it
        // hasn't seen the change.
        replica1.set_dir_clean(&root1).unwrap();

        // But on the next prepare it discovers that the directory is in fact
        // dirty.
        replica1.prepare(PrepareType::Fast).unwrap();
        let root = replica1.root().unwrap();
        assert!(replica1.is_dir_dirty(&root));
    }

    #[test]
    fn setting_dir_clean_is_noop_if_concurrently_modified_both() {
        let dir = tempfile::Builder::new().prefix("storage").tempdir().unwrap();
        let key_chain = Arc::new(KeyChain::generate_new());

        let storage1 = LocalStorage::open(dir.path()).unwrap();
        let replica1 = ServerReplica::new(
            ":memory:", key_chain.clone(),
            Arc::new(storage1), "r00t", 1024,
            flate2::Compression::fast()).unwrap();
        replica1.create_root().unwrap();
        let mut root1 = replica1.root().unwrap();

        let storage2 = LocalStorage::open(dir.path()).unwrap();
        let replica2 = ServerReplica::new(
            ":memory:", key_chain.clone(),
            Arc::new(storage2), "r00t", 1024,
            flate2::Compression::fast()).unwrap();
        let mut root2 = replica1.root().unwrap();

        // Process 1 gets a snapshot of the root
        replica1.list(&mut root1).unwrap();
        // But then process 2 modifies the root
        replica2.create(&mut root2, File(
            &oss("foo"), &FileData::Directory(0o700)), None).unwrap();
        // Process 1 then also does a modification. This causes the cached
        // directory state to be refreshed to reflect process 2's change, but
        // because this wasn't taken into account in the call to `list()`,
        // marking the dir clean should have no effect.
        replica1.create(&mut root1, File(
            &oss("bar"), &FileData::Directory(0o700)), None).unwrap();
        replica1.set_dir_clean(&root1).unwrap();

        // But on the next prepare it discovers that the directory is in fact
        // dirty.
        replica1.prepare(PrepareType::Fast).unwrap();
        let root = replica1.root().unwrap();
        assert!(replica1.is_dir_dirty(&root));
    }

    #[test]
    fn clean_prepare_sets_all_dirs_dirty() {
        init!(replica, root);

        replica.list(&mut root).unwrap();
        replica.set_dir_clean(&root).unwrap();
        assert!(!replica.is_dir_dirty(&root));

        replica.prepare(PrepareType::Clean).unwrap();
        assert!(replica.is_dir_dirty(&root));
    }

    #[test]
    fn scrub_prepare_sets_all_dirs_dirty() {
        init!(replica, root);

        replica.list(&mut root).unwrap();
        replica.set_dir_clean(&root).unwrap();
        assert!(!replica.is_dir_dirty(&root));

        replica.prepare(PrepareType::Scrub).unwrap();
        assert!(replica.is_dir_dirty(&root));
    }

    #[test]
    fn watch_notifies_of_change_by_other_instance() {
        let dir = tempfile::Builder::new().prefix("storage").tempdir().unwrap();
        let storage_dir = dir.path().join("storage");

        let key_chain = Arc::new(KeyChain::generate_new());

        let mut replica1 = ServerReplica::new(
            ":memory:", key_chain.clone(),
            Arc::new(LocalStorage::open(&storage_dir).unwrap()),
            "r00t", 1024, flate2::Compression::fast()).unwrap();
        let replica2 = ServerReplica::new(
            ":memory:", key_chain.clone(),
            Arc::new(LocalStorage::open(&storage_dir).unwrap()),
            "r00t", 1024, flate2::Compression::fast()).unwrap();
        replica1.create_root().unwrap();

        let watch = Arc::new(WatchHandle::default());
        watch.check_dirty();
        assert!(!watch.check_dirty());
        watch.check_context_lost();
        assert!(!watch.check_context_lost());

        replica1.watch(Arc::downgrade(&watch)).unwrap();

        {
            replica1.prepare(PrepareType::Fast).unwrap();
            let mut root = replica1.root().unwrap();
            replica1.list(&mut root).unwrap();
            replica1.set_dir_clean(&root).unwrap();
            replica1.clean_up().unwrap();
        }

        {
            replica2.prepare(PrepareType::Fast).unwrap();
            let mut root = replica2.root().unwrap();
            replica2.list(&mut root).unwrap();
            replica2.create(&mut root, File(
                &oss("plugh"), &FileData::Symlink(oss("xyzzy"))), None)
                .unwrap();
            replica2.clean_up().unwrap();
        }

        thread::sleep(Duration::new(10, 0));
        assert!(watch.check_dirty());
        assert!(!watch.check_context_lost());

        {
            replica1.prepare(PrepareType::Watched).unwrap();
            let root = replica1.root().unwrap();
            assert!(replica1.is_dir_dirty(&root));
        }
    }

    #[test]
    fn directory_revert_detected() {
        use std::process::Command;

        let dir = tempfile::Builder::new().prefix("storage").tempdir().unwrap();
        // We need a real SQLite file so that we can close and reopen the
        // server replica without losing state.
        let sqlite_file = dir.path().join("state.sqlite");
        let storage_dir = dir.path().join("storage");
        let copy_dir = dir.path().join("copy");

        let key_chain = Arc::new(KeyChain::generate_new());

        let sym1 = FileData::Symlink(oss("target"));
        let sym2 = FileData::Symlink(oss("tegrat"));

        {
            let storage = LocalStorage::open(&storage_dir).unwrap();
            let replica = ServerReplica::new(
                &sqlite_file, key_chain.clone(),
                Arc::new(storage), "r00t", 1024,
                flate2::Compression::fast()).unwrap();
            replica.create_root().unwrap();
            let mut root = replica.root().unwrap();

            replica.create(&mut root, File(&oss("sym"), &sym1), None).unwrap();

            // Copy the current state of the server to another location to
            // mount the later attack.
            // There doesn't seem to be any general-purpose "recursively copy
            // directory" built in to Rust or as a crate. Instead of
            // implementing one here, and since we don't work on non-UNIX right
            // now anyway, just shell out to `cp` for the time being.
            assert!(Command::new("cp").arg("-a")
                    .arg(&storage_dir)
                    .arg(&copy_dir)
                    .status().unwrap().success());

            replica.update(&mut root, &oss("sym"), &sym1, &sym2, None).unwrap();
        }

        {
            // Now open the out-of-date copy we made and try to navigate it.
            let storage = LocalStorage::open(&copy_dir).unwrap();
            let replica = ServerReplica::new(
                &sqlite_file, key_chain.clone(),
                Arc::new(storage), "r00t", 1024,
                flate2::Compression::fast()).unwrap();

            let mut root = replica.root().unwrap();
            // When the out-of-date directory is seen, it fails instead of
            // allowing the sync to continue.
            assert_err!(ErrorKind::DirectoryVersionRecessed(..),
                        replica.list(&mut root));
        }
    }

    fn no_prompt() -> Result<Vec<u8>> {
        panic!("shouldn't prompt for password")
    }

    #[test]
    fn read_protected_dirs_not_readable_by_non_group_members() {
        use crate::server::keymgmt;

        let dir = tempfile::Builder::new().prefix("storage").tempdir().unwrap();
        // We need a real SQLite file since there will be multiple replica
        // instances here.
        let sqlite_file = dir.path().join("state.sqlite");
        let storage_dir = dir.path().join("storage");

        let storage = Arc::new(LocalStorage::open(&storage_dir).unwrap());

        // Set up two keys, putting the first one into a unique group.
        keymgmt::init_keys(&*storage, b"hunter2", "privileged").unwrap();
        keymgmt::add_key(&*storage, b"hunter2", b"hunter3", "restricted",
                         no_prompt).unwrap();
        keymgmt::create_group(&*storage, b"hunter2", ["private"].iter(),
                              no_prompt).unwrap();

        let subdir_name = oss("priv.ensync[r=private]");

        {
            let key_chain = keymgmt::derive_key_chain(&*storage, b"hunter2")
                .unwrap();

            let replica = ServerReplica::new(
                sqlite_file.to_str().unwrap(), Arc::new(key_chain),
                storage.clone(), "r00t", 1024,
                flate2::Compression::fast()).unwrap();
            replica.create_root().unwrap();

            let mut root = replica.root().unwrap();
            replica.create(&mut root, File(&subdir_name,
                                           &FileData::Directory(0o700)),
                           None).unwrap();
            let mut subdir = replica.chdir(&root, &subdir_name).unwrap();
            replica.create(&mut subdir,
                           File(&oss("secret"),
                                &FileData::Symlink(oss("plugh"))),
                           None).unwrap();

            let list = replica.list(&mut subdir).unwrap();
            assert_eq!(1, list.len());
        }

        {
            let key_chain = keymgmt::derive_key_chain(&*storage, b"hunter3")
                .unwrap();

            let replica = ServerReplica::new(
                sqlite_file.to_str().unwrap(), Arc::new(key_chain),
                storage.clone(), "r00t", 1024,
                flate2::Compression::fast()).unwrap();

            let mut root = replica.root().unwrap();
            let list = replica.list(&mut root).unwrap();
            assert_eq!(1, list.len());
            assert_eq!(subdir_name, list[0].0);
            assert_eq!(FileData::Directory(0o700), list[0].1);

            let mut subdir = replica.chdir(&root, &subdir_name).unwrap();
            assert!(replica.list(&mut subdir).is_err());
        }
    }

    #[test]
    fn write_protected_dirs_not_writable_by_non_group_members() {
        use crate::server::keymgmt;

        let dir = tempfile::Builder::new().prefix("storage").tempdir().unwrap();
        // We need a real SQLite file since there will be multiple replica
        // instances here.
        let sqlite_file = dir.path().join("state.sqlite");
        let storage_dir = dir.path().join("storage");

        let storage = Arc::new(LocalStorage::open(&storage_dir).unwrap());

        // Set up two keys, putting the first one into a unique group.
        keymgmt::init_keys(&*storage, b"hunter2", "privileged").unwrap();
        keymgmt::add_key(&*storage, b"hunter2", b"hunter3", "restricted",
                         no_prompt).unwrap();
        keymgmt::create_group(&*storage, b"hunter2", ["private"].iter(),
                              no_prompt).unwrap();

        let subdir_name = oss("priv.ensync[w=private]");

        {
            let key_chain = keymgmt::derive_key_chain(&*storage, b"hunter2")
                .unwrap();

            let replica = ServerReplica::new(
                sqlite_file.to_str().unwrap(), Arc::new(key_chain),
                storage.clone(), "r00t", 1024,
                flate2::Compression::fast()).unwrap();
            replica.create_root().unwrap();

            let mut root = replica.root().unwrap();
            replica.create(&mut root, File(&subdir_name,
                                           &FileData::Directory(0o700)),
                           None).unwrap();
            let mut subdir = replica.chdir(&root, &subdir_name).unwrap();
            replica.create(&mut subdir,
                           File(&oss("secret"),
                                &FileData::Symlink(oss("plugh"))),
                           None).unwrap();

            let list = replica.list(&mut subdir).unwrap();
            assert_eq!(1, list.len());
        }

        {
            let key_chain = keymgmt::derive_key_chain(&*storage, b"hunter3")
                .unwrap();

            let replica = ServerReplica::new(
                sqlite_file.to_str().unwrap(), Arc::new(key_chain),
                storage.clone(), "r00t", 1024,
                flate2::Compression::fast()).unwrap();

            let mut root = replica.root().unwrap();
            let list = replica.list(&mut root).unwrap();
            assert_eq!(1, list.len());
            assert_eq!(subdir_name, list[0].0);
            assert_eq!(FileData::Directory(0o700), list[0].1);

            let mut subdir = replica.chdir(&root, &subdir_name).unwrap();
            let list = replica.list(&mut subdir).unwrap();
            assert_eq!(1, list.len());

            assert!(replica.create(&mut subdir, File(
                &oss("newdir"), &FileData::Directory(0o600)), None).is_err());
        }
    }

    #[test]
    fn rename_cannot_change_dir_config() {
        init!(replica, root);

        replica.create(&mut root, File(&oss("subdir"),
                                       &FileData::Directory(0o700)),
                       None).unwrap();
        assert!(replica.rename(&mut root, &oss("subdir"),
                               &oss("subdir.ensync[r=root]")).is_err());
    }

    #[test]
    fn rename_ignores_config_like_string_on_non_dir() {
        init!(replica, root);

        replica.create(&mut root, File(
            &oss("sym"), &FileData::Symlink(oss("plugh"))), None).unwrap();
        replica.rename(&mut root, &oss("sym"),
                       &oss("sym.ensync[r=root]")).unwrap();
    }

    #[test]
    fn rename_allows_noop_config_change_on_dir() {
        init!(replica, root);

        replica.create(&mut root, File(
            &oss("subdir"), &FileData::Directory(0o700)), None).unwrap();
        replica.rename(&mut root, &oss("subdir"),
                       &oss("subdir.ensync[r=everyone]")).unwrap();
    }
}
