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

//! Logic for managing directory state on the server.
//!
//! This all takes place on the client; the server itself has no visibility
//! into these operations or the binary format.
//!
//! As the only mutations one can do to the directory files on the server is
//! replacing them entirely or appending data to the end, the directory format
//! is necessarily log-structured.
//!
//! Most directory operations are performed by appending a single chunk to the
//! end of the file. When the number of redundant entries exceeds the number of
//! live entries and the total size is over a certain threshold, the directory
//! is instead completely rebuilt to remove the redundant entries.
//!
//! # Header format
//!
//! A directory file starts with a header. The header is a single _chunk_ as
//! defined by the V0 format; the chunk contains a `Header` (see
//! `serde_types.in.rs`) encoded with CBOR. The decoder must verify that the id
//! and version in the header match the metadata returned alongside the data.
//! The format indicated in the header dictates how the rest of the file is
//! formatted.
//!
//! # V0 format
//!
//! ## Chunks
//!
//! The V0 directory format encodes the directory as a series of "chunks". Each
//! chunk is structured as follows:
//!
//! - A SHA-3 HMAC (32 bytes)
//! - A 4-byte little-endian integer indicating the length of the data in
//! BLKSZ-byte blocks (including the one containing this integer, but not
//! including the HMAC).
//! - The content of the chunk in CBOR. For everything but the header, this is
//! a `[v0::EntryPair]` as defined in `serde_types.in.rs`.
//! - Arbitrary data padding the content to the declared number of BLKSZ-byte
//! blocks.
//!
//! The HMAC is defined as the SHA-3 over the following data:
//!
//! - The byte content of the chunk, including the length prefix and any
//! padding at the end.
//! - If there is a chunk before this one, the HMAC of that chunk. Otherwise,
//! UNKNOWN_HASH.
//! - The HMAC secret.
//!
//! ## Semantics
//!
//! Each chunk holds a list of `v0::EntryPair`s, which is a file name and data
//! description. The current state of each file is the data on the final pair
//! with that name in the whole directory, or non-existent if the directory
//! never mentions that name.
//!
//! Deletions have an explicit state (`v0::Entry::X`) so that they can replace
//! an existing file without rebuilding the whole directory state. When a
//! rebuild does eventually happen, the explicit deleted entries are not
//! preserved.

#![allow(dead_code)]

use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
// This is another place where we need to be able to convert between byte
// arrays and `OsStr[ing]` which will need some attention for a hypothetical
// Windows port.
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicUsize, Ordering};

use error_chain::ResultExt;
use keccak;
use serde::{Deserialize, Serialize};
use serde::bytes::Bytes;
use serde_cbor;

use defs::{HashId, UNKNOWN_HASH};
use errors::*;
use replica::ReplicaDirectory;
use sql::{SendConnection, StatementEx};
use serde_types::dir::*;
use server::crypt::*;
use server::storage::*;

/// The well-known directory id of the "directory" object which stores the key
/// list.
pub const DIRID_KEYS: HashId = [0;32];
/// The well-known directory id of the pseudo-root directory. This is a mundane
/// directory, but is not the actual root, instead storing pointers to the
/// various named roots.
pub const DIRID_PROOT: HashId = [255;32];

/// Maintains the state of a server-side directory.
///
/// Quite a bit of replica logic ends up here as a result of the transactional
/// semantics of `Storage`. However, the interface exposed is simply reads and
/// unconditional updates; it is up to the replica to handle if-match and so
/// forth, for example.
pub struct Dir<S : Storage> {
    pub id: HashId,
    pub parent: Option<Arc<Dir<S>>>,
    pub path: OsString,

    db: Arc<Mutex<SendConnection>>,
    key: Arc<MasterKey>,
    storage: Arc<S>,
    tx_ctr: Arc<AtomicUsize>,

    // Lock hierarchy: Lock may not be acquired while lock on `db` is held.
    content: Mutex<DirContent>,
}

#[derive(Debug, Clone, Default)]
struct DirContent {
    /// The current (cleartext) version of this directory.
    version: u64,
    /// The current (ciphertext) version of this directory.
    cipher_version: HashId,
    /// The current length, in bytes, of this directory on the server.
    length: u32,
    /// The parsed content of this directory.
    files: HashMap<OsString, v0::Entry>,
    /// The HMAC of the last chunk in the directory content. (There is always
    /// at least one since the `Header` is always present.)
    prev_hmac: HashId,
    /// The number of physical entries within the directory content, to detect
    /// when we need to rebuild due to redundant entries.
    physical_entries: u32,
    /// The session key being used for encryption
    session_key: [u8;BLKSZ],
    /// The IV to pass to `encrypt_append_dir`
    iv: [u8;BLKSZ],
}

impl<S : Storage> ReplicaDirectory for Dir<S> {
    fn full_path(&self) -> &OsStr {
        &self.path
    }
}

impl<S : Storage> Dir<S> {
    /// Initialises the pseudo-root directory.
    pub fn root(db: Arc<Mutex<SendConnection>>, key: Arc<MasterKey>,
                storage: Arc<S>) -> Result<Self> {
        let this = Dir {
            id: DIRID_PROOT,
            parent: None,
            path: "".into(),
            db: db,
            key: key,
            storage: storage,
            tx_ctr: Arc::new(AtomicUsize::new(1)),
            content: Mutex::new(DirContent::default()),
        };
        Ok(this)
    }

    /// Constructs a `Dir` which is a subdirectory of the given `parent` having
    /// the given `name`.
    pub fn subdir(parent: Arc<Self>, name: &OsStr) -> Result<Self> {
        let id = match *parent.lookup(&mut parent.content.lock().unwrap(),
                                      name)? {
            v0::Entry::D(_, H(id)) => Ok(id),
            _ => Err(ErrorKind::NotADirectory),
        }?;

        Ok(Dir {
            id: id,
            path: parent.subdir_path(name),
            db: parent.db.clone(),
            key: parent.key.clone(),
            storage: parent.storage.clone(),
            tx_ctr: parent.tx_ctr.clone(),
            content: Mutex::new(DirContent::default()),
            parent: Some(parent),
        })
    }

    fn subdir_path(&self, name: &OsStr) -> OsString {
        let mut path = self.path.clone();
        path.push("/");
        path.push(name);
        path
    }

    /// Drops the cached state of this directory, forcing the next operation to
    /// re-read the directory content.
    pub fn invalidate(&self) {
        *self.content.lock().unwrap() = DirContent::default();
    }

    /// Remove a subdirectory of this directory for which `test` returns
    /// `true`.
    ///
    /// If `test` returns `false` for all directories, succeed and return
    /// `false`. If `test` fails for any directory, abort and return that
    /// error. Otherwise, remove the first so matched directory from this
    /// directory and return `true`.
    pub fn remove_subdir<F : Fn (&OsStr, &v0::Entry) -> Result<bool>>(
        &self, test: F) -> Result<bool>
    {
        let mut content = self.content.lock().unwrap();
        let child_id = self.do_tx(&mut content, |tx, content| {
            self.refresh_if_needed(content)?;

            // Search for the desired directory
            let mut child_name = None;
            let mut child_id = None;
            for (name, entry) in &content.files { match entry {
                &v0::Entry::D(mode, H(id))
                if test(&name, &v0::Entry::D(mode, H(id)))? => {
                    child_name = Some(name.to_owned());
                    child_id = Some(id);
                    break;
                },
                _ => { },
            } }

            if let Some(name) = child_name {
                // We need to fetch the child directory so we know the version
                // and length to remove, and to ensure it is empty.
                let child = Dir {
                    id: child_id.unwrap(),
                    parent: None, // Not relevant
                    path: self.subdir_path(&name),
                    db: self.db.clone(),
                    key: self.key.clone(),
                    storage: self.storage.clone(),
                    tx_ctr: self.tx_ctr.clone(),
                    content: Mutex::new(DirContent::default()),
                };
                // Fetch the child directory's data as necessary so we know its
                // current version and length, required when we try to
                // `rmdir()` it. Here we also need to check whether it is
                // actually empty.
                {
                    let mut child_content = child.content.lock().unwrap();
                    child.refresh_if_needed(&mut child_content)?;
                    if !child_content.files.is_empty() {
                        return Err(ErrorKind::DirNotEmpty.into());
                    }
                    self.storage.rmdir(tx, &child.id,
                                       &child_content.cipher_version,
                                       child_content.length)?;
                }
                self.add_entry(tx, content, name, v0::Entry::X)?;
                Ok(child_id)
            } else {
                // If this directory no longer exists, we basically succeeded.
                Ok(None)
            }
        })?;

        // Make a best effort to free the side data in the database
        if let Some(child_id) = child_id {
            let db = self.db.lock().unwrap();
            db.prepare(
                "DELETE FROM `latest_dir_ver` WHERE `id` = ?1")
                .binding(1, &child_id[..])
                .run()?;
        }

        Ok(child_id.is_some())
    }

    fn lookup<'a>(&self, content: &'a mut DirContent, name: &OsStr)
                  -> Result<&'a v0::Entry> {
        self.refresh_if_needed(content)?;
        match content.files.get(name) {
            Some(e) => Ok(e),
            None => Err(ErrorKind::NotFound.into()),
        }
    }

    fn refresh_if_needed(&self, content: &mut DirContent) -> Result<()> {
        if !content.is_valid() {
            self.refresh(content)
        } else {
            Ok(())
        }
    }

    fn refresh(&self, content: &mut DirContent) -> Result<()> {
        let db = self.db.lock().unwrap();

        let (cipher_version, cipher_data) = self.storage.getdir(&self.id)?
            // If missing, fail with `DirectoryMissing` instead of `NotFound`
            // because propagating `NotFound` out of the callers of `refresh()`
            // would have different meaning.
            .ok_or(ErrorKind::DirectoryMissing)?;
        let version = decrypt_dir_ver(&self.id, &cipher_version, &*self.key);

        // Validate that the version has not recessed from the latest thing we
        // ever successfully parsed.
        if let Some((latest_ver, latest_len)) =
            db.prepare("SELECT `ver`, `len` FROM `latest_dir_ver` \
                        WHERE `id` = ?1")
            .binding(1, &self.id[..])
            .first(|s| Ok((s.read::<i64>(0)?, s.read::<i64>(1)?)))?
        {
            if (version, cipher_data.len() as u64) <
                (latest_ver as u64, latest_len as u64)
            {
                return Err(ErrorKind::DirectoryVersionRecessed(
                    self.path.clone(), latest_ver as u64, latest_len as u64,
                    version, cipher_data.len() as u64).into());
            }
        }

        // Ok, now decrypt and read the header
        let mut data = Vec::<u8>::new();
        let session_key = decrypt_whole_dir(
            &mut data, &cipher_data[..], &*self.key)?;

        let mut data_reader = &data[..];
        let mut chunk_hmac = UNKNOWN_HASH;
        let header: Header = self.read_v0_chunk(
            &mut data_reader, &mut chunk_hmac)?
            .ok_or(ErrorKind::ServerDirectoryCorrupt(
                self.path.clone(),
                "No header at start of directory".to_owned()))?;

        // Ensure that we actually got the directory we asked for and the
        // version the server said was present.
        if header.dir_id.0 != self.id {
            return Err(ErrorKind::DirectoryEmbeddedIdMismatch(
                self.path.clone()).into());
        }
        if header.ver != version {
            return Err(ErrorKind::DirectoryEmbeddedVerMismatch(
                self.path.clone()).into());
        }
        // Validate we support this format
        if header.fmt > 0 {
            return Err(ErrorKind::UnsupportedServerDirectoryFormat(
                self.path.clone(), 0, header.fmt).into());
        }

        // Don't assign to *content until we've read everything successfully,
        // otherwise we might leave the directory in an inconsistent state
        let mut new_content = DirContent {
            version: version,
            cipher_version: cipher_version,
            length: cipher_data.len() as u32,
            files: Default::default(),
            prev_hmac: chunk_hmac,
            physical_entries: 0,
            session_key: session_key,
            iv: dir_append_iv(&cipher_data),
        };

        while let Some(entries) = self.read_v0_chunk::<Vec<v0::EntryPair>>(
            &mut data_reader, &mut new_content.prev_hmac)?
        {
            for entry in entries {
                new_content.apply_entry(entry);
            }
        }

        // Read all content successfully; write back to the current content and
        // record this as the latest version we've ever seen.
        *content = new_content;

        db.prepare("INSERT OR IGNORE INTO `latest_dir_ver` \
                    (`id`, `ver`, `len`) VALUES (?1, ?2, ?3)")
            .binding(1, &self.id[..])
            .binding(2, version as i64)
            .binding(3, cipher_data.len() as u64 as i64)
            .run()?;

        Ok(())
    }

    fn read_v0_chunk<T : Deserialize>(&self,
                                      data: &mut&[u8], prev_hmac: &mut HashId)
                                      -> Result<Option<T>> {
        // If data is completely empty, we're done.
        if data.is_empty() {
            return Ok(None);
        }

        let mut chunk_hmac = UNKNOWN_HASH;

        // Ensure there is enough space for the chunk header
        if data.len() < chunk_hmac.len() + 4 {
            return Err(ErrorKind::ServerDirectoryCorrupt(
                self.path.clone(),
                "Trailing bytes too small to be a chunk header".to_owned())
                .into());
        }

        // Decode the chunk header and make sure it is valid
        chunk_hmac.copy_from_slice(&data[0..UNKNOWN_HASH.len()]);
        *data = &data[chunk_hmac.len()..];

        let len_blocks =
            ((data[0] as usize) <<  0) |
            ((data[1] as usize) <<  8) |
            ((data[2] as usize) << 16) |
            ((data[3] as usize) << 24);
        if data.len() < len_blocks * BLKSZ {
            return Err(ErrorKind::ServerDirectoryCorrupt(
                self.path.clone(),
                "Chunk length larger than remainder of content".to_owned())
               .into());
        }

        let hmac_data = &data[..len_blocks * BLKSZ];
        let cbor_data = &hmac_data[4..];
        *data = &data[len_blocks * BLKSZ ..];

        // Verify this chunk's HMAC
        let mut kc = keccak::Keccak::new_sha3_256();
        kc.update(hmac_data);
        kc.update(prev_hmac);
        kc.update(self.key.hmac_secret());

        let mut calculated_hmac = UNKNOWN_HASH;
        kc.finalize(&mut calculated_hmac);

        if calculated_hmac != chunk_hmac {
            return Err(ErrorKind::ServerDirectoryCorrupt(
                self.path.clone(),
                "Chunk HMAC is invalid".to_owned()).into());
        }

        *prev_hmac = chunk_hmac;

        // Everything checks out, read the data from the chunk.
        serde_cbor::from_slice(cbor_data).chain_err(
            || ErrorKind::ServerDirectoryCorrupt(
                self.path.clone(),
                "Chunk contains invalid data".to_owned()))
    }

    /// Atomically run `f`.
    ///
    /// Start a server transaction and pass its id to `f`. If `f` fails, abort
    /// the transaction and forward the error. If `f` returns `Ok(None)`, abort
    /// the transaction and return `Ok(None)`. Otherwise, attempt to commit the
    /// transaction and return the same value. If committing the transaction
    /// fails, discard the return value and retry.
    ///
    /// The old value of `content` is backed up before running `f` and is
    /// either restored or invalidated if the transaction is not committed.
    fn do_tx<R, F : FnMut (Tx, &mut DirContent) -> Result<Option<R>>>(
        &self, content: &mut DirContent, mut f: F) -> Result<Option<R>> {
        for _ in 0..16 {
            let tx = self.tx_ctr.fetch_add(1, Ordering::SeqCst) as Tx;
            self.storage.start_tx(tx)?;
            let old_content = content.clone();
            match f(tx, content) {
                Ok(Some(r)) => {
                    if self.storage.commit(tx)? {
                        return Ok(Some(r));
                    } else {
                        // Invalidate so the data is re-fetched if needed
                        *content = DirContent::default();
                    }
                },
                Ok(None) => {
                    *content = old_content;
                    self.storage.abort(tx)?;
                    return Ok(None);
                },
                Err(e) => {
                    let _ = self.storage.abort(tx);
                    *content = old_content;
                    return Err(e);
                },
            }
        }

        Err(ErrorKind::TooManyTxRetries.into())
    }

    fn add_entry(&self, tx: Tx, content: &mut DirContent,
                 name: OsString, entry: v0::Entry)
                 -> Result<()> {
        if content.rewrite_instead_of_append() {
            content.apply_entry((name.into_vec().into(), entry));
            self.rewrite(tx, content)?;
        } else {
            self.append_entry(tx, content, &name, &entry)?;
            content.apply_entry((name.into_vec().into(), entry));
        }

        Ok(())
    }

    fn rewrite(&self, tx: Tx, content: &mut DirContent) -> Result<()> {
        self.storage.rmdir(tx, &self.id,
                           &content.cipher_version, content.length)?;

        content.version += 1;
        content.cipher_version = encrypt_dir_ver(
            &self.id, content.version, &self.key);

        let header = Header {
            dir_id: H(self.id),
            ver: content.version,
            fmt: 0,
        };

        let entries = content.files.iter()
            .map(|(k, v)| (Bytes::new(k.as_bytes()), v))
            .collect::<Vec<_>>();
        content.physical_entries = entries.len() as u32;

        let mut cleartext = Vec::new();
        content.prev_hmac = UNKNOWN_HASH;
        self.encode_chunk(&mut cleartext, &header, &mut content.prev_hmac);
        self.encode_chunk(&mut cleartext, &entries, &mut content.prev_hmac);

        let mut ciphertext = Vec::<u8>::new();
        content.session_key = encrypt_whole_dir(
            &mut ciphertext, &mut&cleartext[..], &self.key)?;
        content.length = ciphertext.len() as u32;
        content.iv = dir_append_iv(&ciphertext);

        self.storage.mkdir(tx, &self.id, &content.cipher_version,
                           &ciphertext)?;
        Ok(())
    }

    fn append_entry(&self, tx: Tx, content: &mut DirContent,
                    name: &OsStr, entry: &v0::Entry)
                    -> Result<()> {
        let mut cleartext = Vec::new();
        self.encode_chunk(&mut cleartext,
                          &[(Bytes::new(name.as_bytes()), entry)],
                          &mut content.prev_hmac);

        let mut ciphertext = Vec::<u8>::new();
        encrypt_append_dir(&mut ciphertext, &cleartext[..],
                           &content.session_key, &content.iv)?;
        self.storage.updir(tx, &self.id, &content.cipher_version,
                           content.length, &ciphertext)?;

        content.length += ciphertext.len() as u32;
        content.iv = dir_append_iv(&ciphertext);
        Ok(())
    }

    fn encode_chunk<T : Serialize>(&self, dst: &mut Vec<u8>, value: T,
                                   prev_hmac: &mut HashId) {
        let start = dst.len();
        // Allocate space for the header
        dst.extend(&UNKNOWN_HASH);
        dst.extend(&[0u8;4]);
        // Write the actual content
        serde_cbor::ser::to_writer(dst, &value)
            .expect("CBOR serialisation failed");
        // Pad until a multiple of the AES block size
        while dst.len() % BLKSZ != 0 {
            dst.push(0);
        }

        // Now we know enough to fill in the length
        let mut length = (dst.len() - start - UNKNOWN_HASH.len()) / BLKSZ;
        for byte in 0..4 {
            dst[UNKNOWN_HASH.len() + byte] = (length & 0xFF) as u8;
            length >>= 8;
        }
        // And generate the HMAC
        let mut kc = keccak::Keccak::new_sha3_256();
        kc.update(&dst[start + UNKNOWN_HASH.len() ..]);
        kc.update(prev_hmac);
        kc.update(self.key.hmac_secret());
        kc.finalize(&mut dst[start .. start + UNKNOWN_HASH.len()]);
        prev_hmac.copy_from_slice(&dst[start .. start + UNKNOWN_HASH.len()]);
    }
}

impl DirContent {
    fn apply_entry(&mut self, entry: v0::EntryPair) {
        let name = OsString::from_vec(entry.0.into());

        match entry.1 {
            v0::Entry::X => { self.files.remove(&name); },
            e => { self.files.insert(name, e); },
        }

        self.physical_entries += 1;
    }

    fn is_valid(&self) -> bool {
        self.length > 0
    }

    fn rewrite_instead_of_append(&self) -> bool {
        self.physical_entries > 16 &&
            (self.physical_entries as usize) * 2 > self.files.len()
    }
}
