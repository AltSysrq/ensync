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

use std::collections::hash_map::Entry::*;
use std::collections::HashMap;
use std::fs;
use std::io::{self, BufRead, Read, Seek, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;
use std::u32;

use sqlite;
use tempfile::{self, NamedTempFile, PersistError};
use tiny_keccak::Keccak;

use crate::defs::{DisplayHash, HashId, UNKNOWN_HASH};
use crate::errors::*;
use crate::server::storage::*;
use crate::sql::{self, SendConnection, StatementEx};

const NX: u32 = !0o7111;

/// Implements the server storage system on the local filesystem.
///
/// Remote servers are handled by layering on top of this implementation an
/// implementation that marshals requests over a pipe.
///
/// The storage is all maintained under a single "root" directory. The
/// directory contains:
///
/// - A shared SQLite database named `state.sqlite`. The presence of this
/// file is also used to ascertain whether a directory is in fact a server
/// root.
///
/// - A temporary directory `tmp`. This directory is usually empty. Temporary
/// files are created here and immediately deleted or populated then moved into
/// place. It is always safe to indiscriminately wipe the contents of `tmp`,
/// but it may interrupt active syncing processes.
///
/// - A directory `dirs` containing all directory data.
///
/// - A directory `objs` containing all object data.
///
/// Directories and objects are both identified by 16-byte sequences (though
/// note as described in the schema that the identifier for a directory file is
/// based on the id and version and is not just the id). These are converted
/// into sub-paths by encoding them in hexadecimal and inserting a forward
/// slash after the first two characters.
///
/// Transactions for the session are simply held in memory. The data submitted
/// with transactions is spilled to unlinked temporary files which are held
/// open until the transaction commits. Impending references to
/// already-existing objects are handled by opening the object for read and
/// holding onto the file handle until commit time; this ensures that even if
/// another process deletes the file by the time the transaction is committed,
/// the process still has access to the data and can reconstitute it.
///
/// This particular approach was chosen so that crashed processes have a
/// near-zero chance of leaving stray data on disk which would require a
/// complex cleanup mechanism to reclaim; instead, the worst that can happen
/// are empty files being left in the temporary directory, as well as
/// partially-written files that will usually be replaced and then cleaned by
/// the next run which would touch them.
///
/// An alternate implementation strategy would be to store *everything*,
/// including the blobs, in SQLite. This is something SQLite does well;
/// however, the separate-file approach was chosen as it is friendlier to
/// various backup solutions.
pub struct LocalStorage {
    tmpdir: PathBuf,
    dirdir: PathBuf,
    objdir: PathBuf,
    dirty_dir_buf: Mutex<fs::File>,
    db: Arc<Mutex<SendConnection>>,
    txns: Mutex<HashMap<Tx, TxData>>,
    // Permissions to use for new files. This is copied from the permissions on
    // the top-level directory.
    permissions: fs::Permissions,
    /// If Some, directories being monitored by the asynchronous watcher mapped
    /// to their expected version and length.
    ///
    /// Lock hierarchy: This is a leaf lock.
    watch_list: Option<Arc<Mutex<HashMap<HashId, (HashId, u32)>>>>,
}

#[derive(Debug, Default)]
struct TxData {
    ops: Vec<TxOp>,
}

#[derive(Debug)]
enum TxOp {
    Mkdir {
        id: HashId,
        ver: HashId,
        sver: HashId,
        data: Vec<u8>,
    },
    Updir {
        id: HashId,
        sver: HashId,
        old_len: u32,
        append: Vec<u8>,
    },
    Rmdir {
        id: HashId,
        sver: HashId,
        old_len: u32,
    },
    LinkObj {
        id: HashId,
        linkid: HashId,
        handle: fs::File,
    },
    UnlinkObj {
        id: HashId,
        linkid: HashId,
    },
}

impl LocalStorage {
    pub fn open(path: &Path) -> Result<Self> {
        let root = path.to_owned();
        let tmpdir = path.join("tmp");
        let objdir = path.join("objs");
        let dirdir = path.join("dirs");

        fn create_dir_all(path: &Path, perm: &fs::Permissions) -> Result<()> {
            fs::DirBuilder::new()
                .recursive(true)
                .mode(perm.mode())
                .create(path)
                .chain_err(|| {
                    format!("Failed to create directory '{}'", path.display())
                })
        }

        fs::create_dir_all(&root).chain_err(|| {
            format!("Failed to create server root '{}'", root.display())
        })?;
        let permissions = fs::metadata(&root)
            .map(|md| md.permissions())
            .chain_err(|| "Failed to read server directory permissions")?;

        create_dir_all(&tmpdir, &permissions)?;
        create_dir_all(&objdir, &permissions)?;
        create_dir_all(&dirdir, &permissions)?;
        for i in 0..256 {
            let suffix = format!("{:02x}", i);
            create_dir_all(&objdir.join(&suffix), &permissions)?;
            create_dir_all(&dirdir.join(&suffix), &permissions)?;
        }

        // SQLite doesn't respect umask. Making it do so is a C preprocessor
        // option only. However, it won't change the mode of an existing file,
        // and as of http://www.sqlite.org/src/info/84b324606a will copy the
        // permissions of the database to any auxiliary files. It will also
        // open an empty file as the database and behave properly. So create
        // the database file if it does not already exist and ensure it has the
        // proper mode.
        let sqlite_path = path.join("state.sqlite");
        fs::OpenOptions::new()
            .append(true)
            .create(true)
            .mode(permissions.mode() & NX)
            .open(&sqlite_path)
            .chain_err(|| {
                format!("Error creating '{}'", sqlite_path.display())
            })?;
        let cxn = sqlite::Connection::open(sqlite_path)?;

        cxn.execute(include_str!("storage-schema.sql"))?;

        let tmpfile = tempfile::tempfile()
            .chain_err(|| "Failed to create temporary file")?;

        Ok(LocalStorage {
            tmpdir: tmpdir,
            objdir: objdir,
            dirdir: dirdir,
            dirty_dir_buf: Mutex::new(tmpfile),
            db: Arc::new(Mutex::new(SendConnection(cxn))),
            txns: Mutex::new(HashMap::default()),
            permissions: permissions,
            watch_list: None,
        })
    }

    fn id_suffix(id: &HashId) -> String {
        use std::fmt::Write;

        let mut accum = String::with_capacity(2 * id.len() + 1);
        write!(accum, "{:02x}/", id[0]).unwrap();
        for &byte in &id[1..] {
            write!(accum, "{:02x}", byte).unwrap();
        }
        accum
    }

    fn dir_path(&self, id: &HashId, v: &HashId) -> PathBuf {
        let mut kc = Keccak::new_sha3_256();
        kc.update(id);
        kc.update(v);
        let mut hash = UNKNOWN_HASH;
        kc.finalize(&mut hash);
        self.dirdir.join(LocalStorage::id_suffix(&hash))
    }

    fn obj_path(&self, id: &HashId) -> PathBuf {
        self.objdir.join(LocalStorage::id_suffix(id))
    }

    fn tx_add(&self, txid: Tx, op: TxOp) -> Result<()> {
        let mut txns = self.txns.lock().unwrap();
        if let Some(tx) = txns.get_mut(&txid) {
            tx.ops.push(op);
            Ok(())
        } else {
            Err(ErrorKind::NoSuchTransaction(txid).into())
        }
    }

    fn named_temp_file(&self) -> io::Result<NamedTempFile> {
        let file = NamedTempFile::new_in(&self.tmpdir)?;
        let perm = fs::Permissions::from_mode(self.permissions.mode() & NX);
        fs::set_permissions(file.path(), perm)?;
        Ok(file)
    }

    fn do_commit(
        &self,
        db: &sqlite::Connection,
        tx: &mut TxData,
    ) -> Result<bool> {
        for op in &mut tx.ops {
            match *op {
                TxOp::Mkdir {
                    ref id,
                    ref ver,
                    ref sver,
                    ref data,
                } => {
                    if db
                        .prepare("SELECT 1 FROM `dirs` WHERE `id` = ?1")
                        .binding(1, &id[..])
                        .exists()?
                    {
                        return Ok(false);
                    }

                    db.prepare(
                        "INSERT INTO `dirs` (`id`, `ver`, `sver`, `length`)\
                                 VALUES (?1, ?2, ?3, ?4)",
                    )
                    .binding(1, &id[..])
                    .binding(2, &ver[..])
                    .binding(3, &sver[..])
                    .binding(4, data.len() as i64)
                    .run()?;
                    let mut tmpfile = self.named_temp_file()?;
                    tmpfile.write_all(data)?;

                    match tmpfile.persist(self.dir_path(&id, &ver)) {
                        Ok(persisted) => {
                            persisted.sync_all()?;
                        }
                        Err(PersistError { error, .. }) => {
                            return Err(error.into())
                        }
                    }
                }

                TxOp::Updir {
                    ref id,
                    ref sver,
                    old_len,
                    ref append,
                } => {
                    let ver =
                        match self.get_ver_for_modify(db, id, sver, old_len)? {
                            Some(v) => v,
                            None => return Ok(false),
                        };

                    if (u32::MAX as u64) - (old_len as u64)
                        < (append.len() as u64)
                    {
                        return Err(ErrorKind::DirectoryTooLarge.into());
                    }

                    // Run the update in SQLite first to ensure we have a write
                    // lock on the database, thus preventing two processes from
                    // appending to the same offset simultaneously.
                    db.prepare(
                        "UPDATE `dirs` SET `length` = ?2 \
                                 WHERE `id` = ?1",
                    )
                    .binding(1, &id[..])
                    .binding(2, (old_len as usize + append.len()) as i64)
                    .run()?;

                    let mut file = fs::OpenOptions::new()
                        .write(true)
                        .create(false)
                        .open(self.dir_path(&id, &ver))?;
                    file.seek(io::SeekFrom::Start(old_len as u64))?;
                    // This will have effect even if the transaction rolls
                    // back, but the length in SQLite will still reflect the
                    // correct length.
                    file.write_all(append)?;
                    file.sync_data()?;
                }

                TxOp::Rmdir {
                    ref id,
                    ref mut sver,
                    old_len,
                } => {
                    let ver =
                        match self.get_ver_for_modify(db, id, sver, old_len)? {
                            Some(v) => v,
                            None => return Ok(false),
                        };

                    db.prepare(
                        "DELETE FROM `dirs` \
                                 WHERE `id` = ?1",
                    )
                    .binding(1, &id[..])
                    .run()?;
                    // We have to wait with removing the actual file until
                    // postexecute so that readers can see the fact that the
                    // entry has been deleted and because we wouldn't be able
                    // to undo that should the transaction roll back.
                    //
                    // But we do need to store the normal version, which we do by
                    // stashing it in the `sver` field (kind of hackish, oh well).
                    *sver = ver;
                }

                TxOp::LinkObj {
                    ref id,
                    ref linkid,
                    ref mut handle,
                } => {
                    // Write tho the database first so that we get a lock on
                    // the table. This will block cleaners from noticing the
                    // entry has no refs and trying to delete it.
                    self.update_ref(db, id, linkid)?;

                    // Make sure the object actually exists.
                    //
                    // Since we do this *before* the database entry is visible
                    // to readers, if we fail halfway after reconstituting the
                    // object, it will be orphaned. However, we cannot delay
                    // reconstitution until after the database is committed, as
                    // this would make it possible for the transaction to
                    // commit without all data being available.
                    //
                    // In virtually all cases, the object will already exist,
                    // as `PutObj` adds a zero-ref entry eagerly. We only get
                    // here if we lost a race to a cleaner.
                    let objpath = self.obj_path(&id);
                    if fs::symlink_metadata(&objpath).is_err() {
                        let mut tmpfile = self.named_temp_file()?;
                        io::copy(handle, &mut tmpfile)?;
                        let persisted = tmpfile.persist(&objpath)?;
                        persisted.sync_all()?;
                    }
                }

                TxOp::UnlinkObj { ref id, ref linkid } => {
                    self.update_ref(db, id, linkid)?;
                    // We cannot delete the backing file now even if the ref
                    // vector becomes zero, because there'd be no way to undo
                    // it if the transaction rolls back.
                    //
                    // Note that we wouldn't want to anyway, so that renames
                    // can later reuse the object.
                    //
                    // Objects with zero references are cleaned during general
                    // cleanup.
                }
            }
        }

        Ok(true)
    }

    fn update_ref(
        &self,
        db: &sqlite::Connection,
        id: &HashId,
        linkid: &HashId,
    ) -> Result<()> {
        let vold_refs: Option<Vec<u8>> = db
            .prepare("SELECT `refs` FROM `objs` WHERE `id` = ?1")
            .binding(1, &id[..])
            .first(|s| s.read(0))?;
        if let Some(vold_refs) = vold_refs {
            let mut refs = UNKNOWN_HASH;
            if vold_refs.len() != refs.len() {
                return Err(ErrorKind::InvalidRefVector.into());
            }
            refs.copy_from_slice(&vold_refs);
            for (accum, &new) in refs.iter_mut().zip(linkid.iter()) {
                *accum ^= new;
            }

            db.prepare(
                "UPDATE `objs` SET `refs` = ?2 \
                             WHERE `id` = ?1",
            )
            .binding(1, &id[..])
            .binding(2, &refs[..])
            .run()?;
        } else {
            db.prepare(
                "INSERT INTO `objs` (`id`, `refs`) \
                             VALUES (?1, ?2)",
            )
            .binding(1, &id[..])
            .binding(2, &linkid[..])
            .run()?;
        }
        Ok(())
    }

    fn get_ver_for_modify(
        &self,
        db: &sqlite::Connection,
        id: &HashId,
        sver: &HashId,
        old_len: u32,
    ) -> Result<Option<HashId>> {
        let vver: Option<Vec<u8>> = db
            .prepare(
                "SELECT `ver` FROM `dirs` \
                        WHERE `id` = ?1 AND `sver` = ?2 \
                        AND   `length` = ?3",
            )
            .binding(1, &id[..])
            .binding(2, &sver[..])
            .binding(3, old_len as i64)
            .first(|r| r.read(0))?;

        let vver = match vver {
            Some(ver) => ver,
            None => return Ok(None),
        };
        let mut ver = UNKNOWN_HASH;
        if vver.len() != ver.len() {
            return Err(ErrorKind::InvalidHash.into());
        }
        ver.copy_from_slice(&vver);
        Ok(Some(ver))
    }

    fn postcommit_cleanup(&self, tx: &TxData) {
        for op in &tx.ops {
            match *op {
                TxOp::Mkdir { ref id, .. } | TxOp::Updir { ref id, .. } => {
                    if let Some(ref wl) = self.watch_list {
                        wl.lock().unwrap().remove(id);
                    }
                }

                TxOp::Rmdir {
                    ref id, ref sver, ..
                } => {
                    if let Some(ref wl) = self.watch_list {
                        wl.lock().unwrap().remove(id);
                    }

                    // `sver` has been overwritten with the normal version.
                    let _ = fs::remove_file(self.dir_path(id, sver));
                }

                _ => {}
            }
        }
    }

    fn do_clean_up(&self) -> Result<()> {
        let db = self.db.lock().unwrap();
        sql::tx_gen(&db, || {
            // Make sure we have a write lock.
            db.prepare("DELETE FROM `lock`").run()?;

            let mut stmt = db
                .prepare("SELECT `id` FROM `objs` WHERE `refs` = ?1")
                .binding(1, &UNKNOWN_HASH[..])?;

            while sqlite::State::Done != stmt.next()? {
                let vid: Vec<u8> = stmt.read(0)?;
                let mut id = UNKNOWN_HASH;
                if id.len() != vid.len() {
                    return Err(ErrorKind::InvalidObjectId.into());
                }
                id.copy_from_slice(&vid);

                // Delete the entry first to be sure we have the SQLite lock
                db.prepare("DELETE FROM `objs` WHERE `id` = ?1")
                    .binding(1, &id[..])
                    .run()?;

                let _ = fs::remove_file(self.obj_path(&id));
            }
            Ok(())
        })
    }
}

fn dir_exists_with_version(
    db: &SendConnection,
    id: &HashId,
    ver: &HashId,
    len: u32,
) -> Result<bool> {
    db.prepare(
        "SELECT 1 FROM `dirs` \
         WHERE `id` = ?1 AND ver = ?2 AND length = ?3",
    )
    .binding(1, &id[..])
    .binding(2, &ver[..])
    .binding(3, len as u64 as i64)
    .exists()
    .chain_err(|| {
        format!(
            "Error checking state of directory {}/{}",
            DisplayHash(*id),
            DisplayHash(*ver)
        )
    })
}

impl Storage for LocalStorage {
    fn getdir(&self, id: &HashId) -> Result<Option<(HashId, Vec<u8>)>> {
        for _ in 0..256 {
            let r: Option<(Vec<u8>, i64)> =
                {
                    let db = self.db.lock().unwrap();
                    let r = db.prepare(
                    "SELECT `ver`, `length` FROM `dirs` WHERE `id` = ?1")
                     .binding(1, &id[..])
                     .first(|s| Ok((s.read(0)?, s.read(1)?)))?;
                    r
                };

            if let Some((vh, iv)) = r {
                let mut v = UNKNOWN_HASH;
                if vh.len() != v.len() || iv < 0 || iv > u32::MAX as i64 {
                    return Err(ErrorKind::InvalidServerDirEntry.into());
                }
                v.copy_from_slice(&vh);

                match fs::File::open(self.dir_path(id, &v)) {
                    Ok(mut file) => {
                        let mut data = vec![0u8; iv as usize];
                        file.read_exact(&mut data[..])?;
                        return Ok(Some((v, data)));
                    }
                    // If the file was not found, we probably raced with
                    // another process which was deleting it, so reread from
                    // the database.
                    Err(ref ioe) if io::ErrorKind::NotFound == ioe.kind() => {
                        continue
                    }
                    Err(e) => return Err(e.into()),
                }
            } else {
                return Ok(None);
            }
        }

        // The chance of getting here via race conditions is vanishingly small;
        // the database probably has a now-unchanging reference to a file that
        // does not exist.
        Err(ErrorKind::DanglingServerDirectoryRef.into())
    }

    fn getobj(&self, id: &HashId) -> Result<Option<Vec<u8>>> {
        match fs::File::open(self.obj_path(id)) {
            Ok(mut file) => {
                let mut v = Vec::new();
                file.read_to_end(&mut v)?;
                Ok(Some(v))
            }
            Err(ref ioe) if io::ErrorKind::NotFound == ioe.kind() => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    fn check_dir_dirty(
        &self,
        id: &HashId,
        ver: &HashId,
        len: u32,
    ) -> Result<()> {
        if let Some(ref watch_list) = self.watch_list {
            watch_list.lock().unwrap().insert(*id, (*ver, len));
        }

        let exists =
            dir_exists_with_version(&self.db.lock().unwrap(), id, ver, len)?;

        if !exists {
            self.dirty_dir_buf
                .lock()
                .unwrap()
                .write_all(id)
                .chain_err(|| "Error writing to dirty directory buffer")?;
        }

        Ok(())
    }

    fn for_dirty_dir(
        &self,
        f: &mut dyn FnMut(&HashId) -> Result<()>,
    ) -> Result<()> {
        let mut d = self.dirty_dir_buf.lock().unwrap();
        d.seek(io::SeekFrom::Start(0))
            .chain_err(|| "Error seeking dirty directory buffer")?;

        {
            let mut reader = io::BufReader::new(&mut *d);
            while !reader
                .fill_buf()
                .chain_err(|| "Error reading dirty directory buffer")?
                .is_empty()
            {
                let mut id = HashId::default();
                reader
                    .read_exact(&mut id)
                    .chain_err(|| "Error reading dirty directory buffer")?;
                f(&id)?;
            }
        }

        d.seek(io::SeekFrom::Start(0))
            .chain_err(|| "Error seeking dirty directory buffer")?;
        d.set_len(0)
            .chain_err(|| "Error truncating dirty directory buffer")?;
        Ok(())
    }

    fn start_tx(&self, tx: Tx) -> Result<()> {
        let mut txns = self.txns.lock().unwrap();
        match txns.entry(tx) {
            Occupied(_) => Err(ErrorKind::TransactionAlreadyInUse(tx).into()),
            Vacant(e) => {
                e.insert(Default::default());
                Ok(())
            }
        }
    }

    fn commit(&self, tx: Tx) -> Result<bool> {
        enum CommitError {
            Error(Error),
            CommitFailed,
        }
        impl<T> From<T> for CommitError
        where
            Error: From<T>,
        {
            fn from(t: T) -> Self {
                CommitError::Error(Error::from(t))
            }
        }

        let mut txdat = self
            .txns
            .lock()
            .unwrap()
            .remove(&tx)
            .ok_or("No such transaction")?;

        {
            let db = self.db.lock().unwrap();
            // Atomically ensure that the transaction can be committed, then
            // commit it.
            match sql::tx_gen(&db, || {
                // Take a write lock before issuing any reads
                db.prepare("DELETE FROM `lock`").run()?;

                match self.do_commit(&db, &mut txdat) {
                    Ok(true) => Ok(()),
                    Ok(false) => Err(CommitError::CommitFailed),
                    Err(e) => Err(CommitError::Error(e)),
                }
            }) {
                Ok(()) => (),
                Err(CommitError::CommitFailed) => return Ok(false),
                Err(CommitError::Error(e)) => return Err(e),
            }
        }

        // Make a best-effort attempt to clean up now-orphaned resources that
        // could not be safely handled above. Errors are ignored since the
        // transaction really did commit successfully.
        self.postcommit_cleanup(&txdat);
        Ok(true)
    }

    fn abort(&self, tx: Tx) -> Result<()> {
        self.txns
            .lock()
            .unwrap()
            .remove(&tx)
            .ok_or("No such transaction")?;
        Ok(())
    }

    fn mkdir(
        &self,
        tx: Tx,
        id: &HashId,
        v: &HashId,
        sv: &HashId,
        data: &[u8],
    ) -> Result<()> {
        self.tx_add(
            tx,
            TxOp::Mkdir {
                id: *id,
                ver: *v,
                sver: *sv,
                data: data.to_vec(),
            },
        )
    }

    fn updir(
        &self,
        tx: Tx,
        id: &HashId,
        sv: &HashId,
        old_len: u32,
        append: &[u8],
    ) -> Result<()> {
        self.tx_add(
            tx,
            TxOp::Updir {
                id: *id,
                sver: *sv,
                old_len: old_len,
                append: append.to_vec(),
            },
        )
    }

    fn rmdir(
        &self,
        tx: Tx,
        id: &HashId,
        sv: &HashId,
        old_len: u32,
    ) -> Result<()> {
        self.tx_add(
            tx,
            TxOp::Rmdir {
                id: *id,
                sver: *sv,
                old_len: old_len,
            },
        )
    }

    fn linkobj(&self, tx: Tx, id: &HashId, linkid: &HashId) -> Result<bool> {
        match fs::File::open(self.obj_path(id)) {
            Ok(file) => {
                self.tx_add(
                    tx,
                    TxOp::LinkObj {
                        id: *id,
                        linkid: *linkid,
                        handle: file,
                    },
                )?;
                Ok(true)
            }
            Err(ref ioe) if io::ErrorKind::NotFound == ioe.kind() => Ok(false),
            Err(e) => Err(e.into()),
        }
    }

    fn putobj(
        &self,
        tx: Tx,
        id: &HashId,
        linkid: &HashId,
        data: &[u8],
    ) -> Result<()> {
        // First, write the data out, register it with no links, then move it
        // into place.
        //
        // The order is important, as the two main steps are not atomic. In
        // this order, there is the possibility of an entry indicating no links
        // which has no corresponding file. We test for object presence by
        // accessing the file, so this is safe, and also ensures we don't leak
        // orphaned data.
        //
        // There is a possibility of a concurrent process noticing the file
        // with no links and deleting it. This is fine, as we hold onto a
        // handle to the file and can reconstitute it later as with `LinkObj`.
        let mut tmpfile = self.named_temp_file()?;
        tmpfile.write_all(data)?;
        {
            let db = self.db.lock().unwrap();
            sql::tx(&db, || {
                // No need to invoke the `lock` table since we only do an
                // insert here.
                db.prepare(
                    "INSERT OR IGNORE INTO `objs` (\
                            `id`, `refs`) VALUES (?1, ?2)",
                )
                .binding(1, &id[..])
                .binding(2, &UNKNOWN_HASH[..])
                .run()
            })?;
        }

        let mut persisted = tmpfile.persist(self.obj_path(id))?;
        persisted.sync_all()?;
        persisted.seek(io::SeekFrom::Start(0))?;

        self.tx_add(
            tx,
            TxOp::LinkObj {
                id: *id,
                linkid: *linkid,
                handle: persisted,
            },
        )
    }

    fn unlinkobj(&self, tx: Tx, id: &HashId, linkid: &HashId) -> Result<()> {
        self.tx_add(
            tx,
            TxOp::UnlinkObj {
                id: *id,
                linkid: *linkid,
            },
        )
    }

    fn watch(
        &mut self,
        mut f: Box<dyn FnMut(Option<&HashId>) + Send>,
    ) -> Result<()> {
        if self.watch_list.is_some() {
            return Err(ErrorKind::AlreadyWatching.into());
        }

        let watch_list = Arc::new(Mutex::new(HashMap::new()));
        let weak_watch_list = Arc::downgrade(&watch_list);
        self.watch_list = Some(watch_list);

        let weak_db = Arc::downgrade(&self.db);

        thread::spawn(move || loop {
            // We implement watching by simple polling, which admittedly isn't
            // the most efficient approach. This was chosen on account of
            // several considerations:
            //
            // - We can't use the `notify` crate here, because we have no way
            // to go from the directory filename to the directory id since it
            // is SHA-3'ed.
            //
            // - Proper IPC systems would make ensync heavier and less
            // portable.
            //
            // - Using polling means zero overhead for uses not requiring
            // watching, nor do other parts of the code need to be aware of how
            // change detection happens.
            #[cfg(test)]
            const POLL_INTERVAL: u64 = 1;
            #[cfg(not(test))]
            const POLL_INTERVAL: u64 = 30;

            thread::sleep(Duration::new(POLL_INTERVAL, 0));

            let (watch_list, db) =
                match (weak_watch_list.upgrade(), weak_db.upgrade()) {
                    (Some(w), Some(d)) => (w, d),
                    _ => return,
                };

            let wl_copy = watch_list.lock().unwrap().clone();
            for (id, (ver, len)) in wl_copy {
                if let Ok(false) =
                    dir_exists_with_version(&db.lock().unwrap(), &id, &ver, len)
                {
                    watch_list.lock().unwrap().remove(&id);
                    f(Some(&id));
                }
            }
        });

        Ok(())
    }

    fn watchdir(&self, id: &HashId, ver: &HashId, len: u32) -> Result<()> {
        if let Some(ref watch_list) = self.watch_list {
            watch_list.lock().unwrap().insert(*id, (*ver, len));
        }
        Ok(())
    }

    fn clean_up(&self) {
        let _ = self.do_clean_up();
    }
}

#[cfg(test)]
mod test {
    include!("storage_tests.rs");

    #[test]
    fn adding_ops_to_nx_transaction_is_err() {
        init!(dir, storage);

        assert!(storage
            .mkdir(42, &hashid(1), &hashid(2), &hashid(3), b"hello world")
            .is_err());
        assert!(storage
            .updir(42, &hashid(1), &hashid(2), 99, b"hello world")
            .is_err());
        assert!(storage.rmdir(42, &hashid(1), &hashid(2), 99).is_err());
        // Use `putobj` first as it will actually create the object anyway, and
        // then `linkobj` will actually need to interact with the transaction.
        assert!(storage
            .putobj(42, &hashid(1), &hashid(2), b"hello world")
            .is_err());
        assert!(storage.linkobj(42, &hashid(1), &hashid(2)).is_err());
        assert!(storage.unlinkobj(42, &hashid(1), &hashid(2)).is_err());
    }

    #[test]
    fn starting_duplicate_transaction_is_err() {
        init!(dir, storage);

        storage.start_tx(42).unwrap();
        assert!(storage.start_tx(42).is_err());
    }

    use super::LocalStorage;
    fn create_storage(dir: &Path) -> LocalStorage {
        LocalStorage::open(dir).unwrap()
    }
}
