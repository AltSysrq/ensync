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

use std::ffi::{OsStr,OsString};
use std::fs;
use std::io::{self,Read,Seek,Write};
use std::mem;
use std::os::unix;
use std::os::unix::fs::DirBuilderExt;
use std::os::unix::prelude::*;
use std::path::{Path,PathBuf};
use std::sync::{Arc,Mutex};
use std::sync::atomic::{AtomicUsize,Ordering};

use defs::*;
use replica::*;
use block_xfer::{BlockList,StreamSource,ContentAddressableSource,BlockFetch};
use block_xfer::{blocks_to_stream,hash_block,stream_to_blocks};
use super::dao::{Dao,InodeStatus};
use super::dir::*;

quick_error! {
    #[derive(Debug)]
    enum Error {
        RenameTargetAlreadyExists {
            description("New name already in use")
        }
        NotMatched {
            description("File changed since last seen")
        }
        AlreadyExists {
            description("File with this name created since directory listed")
        }
        AllSuffixesInUse {
            description("Shunt failed: All file sufixes in use")
        }
        MissingXfer {
            description("BUG: No xfer provided for file transfer")
        }
        NotADir {
            description("Not a directory")
        }
        RmdirRoot {
            description("Cannot remove root directory")
        }
        ChdirXDev {
            description("Cannot traverse filesystem boundary")
        }
        PrivateXDev {
            description("Private directory is on different filesystem from \
                         sync root")
        }
        BadFilename(name: OsString) {
            description("Illegal file name")
            display("Illegal file name: {:?}", name)
        }
    }
}

struct Config {
    hmac_secret: Vec<u8>,
    root: OsString,
    private_dir: OsString,
    block_size: usize,
    cache_generation: i64,
    root_dev: u64,
}

pub struct PosixReplica {
    // Since the transfer objects need to write back into the DAO, access
    // configs, etc, and the lack of HKTs means they can't hold a reference to
    // the replica itself, we need to use Arc to allow the objects to be shared
    // explicitly.
    config: Arc<Config>,
    dao: Arc<Mutex<Dao>>,
    /// Suffix for the next temporary file generated
    tmpix: AtomicUsize,
}

fn metadata_to_fd(path: &OsStr, md: &fs::Metadata,
                  dao: &Dao, calc_hash_if_unknown: bool,
                  config: &Config)
                  -> Result<FileData> {
    let typ = md.file_type();
    if typ.is_dir() {
        Ok(FileData::Directory(md.mode() & 0o7777))
    } else if typ.is_symlink() {
        let target = try!(fs::read_link(path)).into_os_string();
        Ok(FileData::Symlink(target))
    } else if typ.is_file() {
        let mode = md.mode() & 0o7777;
        let mtime = md.mtime();
        let ino = md.ino();
        let size = md.size();
        let hash = try!(get_or_compute_hash(
            path, dao, &InodeStatus {
                ino: ino, mtime: mtime, size: size,
            }, calc_hash_if_unknown, config));
        Ok(FileData::Regular(mode, size, mtime, hash))
    } else {
        Ok(FileData::Special)
    }
}

fn get_or_compute_hash(path: &OsStr, dao: &Dao, stat: &InodeStatus,
                       calc_hash_if_unknown: bool,
                       config: &Config) -> Result<HashId> {
    if let Some(cached) = try!(dao.cached_file_hash(
        path, stat, config.cache_generation))
    {
        return Ok(cached);
    }

    if !calc_hash_if_unknown {
        return Ok(UNKNOWN_HASH);
    }

    let blocklist = try!(stream_to_blocks(
        try!(fs::File::open(path)), config.block_size, &config.hmac_secret[..],
        |_,_| Ok(())));
    let _ = dao.cache_file_hashes(
        path, &blocklist.total, &blocklist.blocks[..],
        config.block_size, stat, config.cache_generation);
    Ok(blocklist.total)
}

/// Return Err if `name` has special significance on UNIX, eg, is "." or "..",
/// or contains a `/` or NUL character.
///
/// This isn't strictly necessary. We do it as a safety precaution, so that
/// corrupted server replicas (accidental or otherwise) cannot "escape" the
/// sync root by having ".." as a literal directory, for example.
fn assert_sane_filename(name: &OsStr) -> Result<()> {
    if name == OsStr::new(".") || name == OsStr::new("..") {
        return Err(Error::BadFilename(name.to_owned()).into());
    }

    let s = name.to_string_lossy();
    if s.contains('/') || s.contains('\x00') {
        return Err(Error::BadFilename(name.to_owned()).into());
    }

    Ok(())
}

impl Replica for PosixReplica {
    type Directory = DirHandle;
    type TransferIn = Option<ContentAddressableSource>;
    type TransferOut = Option<Box<StreamSource>>;

    fn is_dir_dirty(&self, dir: &DirHandle) -> bool {
        return !self.dao.lock().unwrap().is_dir_clean(dir.full_path())
            .unwrap_or(true)
    }

    fn set_dir_clean(&self, dir: &DirHandle) -> Result<bool> {
        try!(self.dao.lock().unwrap().set_dir_clean(
            dir.full_path(), &dir.hash()));
        Ok(true)
    }

    fn root(&self) -> Result<DirHandle> {
        Ok(DirHandle::root(self.config.root.clone()))
    }

    fn list(&self, dir: &mut DirHandle) -> Result<Vec<(OsString,FileData)>> {
        let mut ret = Vec::new();

        dir.reset_hash();
        let read_result = fs::read_dir(dir.full_path());
        let entries = match read_result {
            Ok(entries) => entries,
            Err(err) => {
                if io::ErrorKind::NotFound == err.kind() &&
                    dir.is_synth()
                {
                    return Ok(ret);
                } else {
                    return Err(err.into());
                }
            },

        };
        for entry in entries {
            let entry = try!(entry);

            let name = entry.file_name();
            if OsStr::new(".") == &name ||
                OsStr::new("..") == &name
            {
                continue;
            }

            let fd = try!(metadata_to_fd(
                entry.path().as_os_str(), &try!(entry.metadata()),
                &*self.dao.lock().unwrap(), true, &*self.config));
            dir.toggle_file(File(&name, &fd));
            ret.push((name, fd));
        }

        Ok(ret)
    }

    fn rename(&self, dir: &mut DirHandle, old: &OsStr, new: &OsStr)
              -> Result<()> {
        try!(assert_sane_filename(old));
        try!(assert_sane_filename(new));

        // Make sure the name under `new` doesn't already exist. This isn't
        // quite atomic with the rest of the operation, but there's no way to
        // accomplish that, so this will have to be good enough.
        let new_path = dir.child(new);

        match fs::symlink_metadata(&new_path) {
            Ok(_) => return Err(Error::RenameTargetAlreadyExists.into()),
            Err(ref e) if io::ErrorKind::NotFound == e.kind() => { },
            Err(e) => return Err(e.into()),
        }

        // Get the file data representation so we can remove/add it to the sum
        // hash of the directory state.
        let old_path = dir.child(old);
        let fd = try!(metadata_to_fd(
            &old_path, &try!(fs::symlink_metadata(&old_path)),
            &*self.dao.lock().unwrap(), false, &*self.config));

        try!(fs::rename(&old_path, &new_path));

        // Update the directory state hash accordingly
        dir.toggle_file(File(old, &fd));
        dir.toggle_file(File(new, &fd));

        Ok(())
    }

    fn remove(&self, dir: &mut DirHandle, target: File) -> Result<()> {
        try!(assert_sane_filename(target.0));

        let path = dir.child(target.0);
        try!(self.check_matches(&path, target.1));

        try!(self.remove_general(&path, target.1));

        let _ = self.dao.lock().unwrap().delete_cache(&path);
        dir.toggle_file(target);

        Ok(())
    }

    fn create(&self, dir: &mut DirHandle, source: File,
              xfer: Option<ContentAddressableSource>)
              -> Result<FileData> {
        try!(assert_sane_filename(source.0));
        try!(dir.create_if_needed(self, None));

        let path = dir.child(source.0);
        let ret = try!(self.put_file(dir, source, xfer, || {
            // Make sure the file doesn't already exist.
            // This is a bit racy since we have no way to atomically create the
            // file with the desired content if and only if no file with that name
            // already exists.
            match fs::symlink_metadata(&path) {
                Ok(_) => Err(Error::AlreadyExists.into()),
                Err(ref err) if io::ErrorKind::NotFound == err.kind() =>
                    Ok(()),
                Err(err) => Err(err.into()),
            }
        }));

        dir.toggle_file(source);
        return Ok(ret);
    }

    fn update(&self, dir: &mut DirHandle, name: &OsStr,
              old: &FileData, new: &FileData,
              xfer: Option<ContentAddressableSource>)
              -> Result<FileData> {
        try!(assert_sane_filename(name));

        let path = dir.child(name);

        // If this can be translated to a simple `chmod()`, do so.
        match (old, UNKNOWN_HASH, new, UNKNOWN_HASH) {
            (&FileData::Directory(_), h1, &FileData::Directory(mode), h2) |
            (&FileData::Regular(_,_,_,h1), _, &FileData::Regular(mode,_,_,h2), _)
            if h1 == h2 => {
                try!(self.check_matches(&path, old));
                try!(fs::set_permissions(
                    &path, fs::Permissions::from_mode(mode)));
                dir.toggle_file(File(name, old));
                dir.toggle_file(File(name, new));
                return Ok(new.clone());
            },
            _ => { },
        }

        let mut shunted_name = None;
        let ret = try!(self.put_file(dir, File(name, new), xfer, || {
            try!(self.check_matches(&path, old));

            // Table of what we need to do before updating:
            //        old
            //       F D S X
            //     F 1 2 1 1
            // new D 2 * 2 1
            //     S 1 2 1 1
            //     X ! ! ! !
            //
            // 1: No action needed. This happens when replacing regular files
            // with other regular files, since the new file is created
            // elsewhere then renamed onto the new file.
            //
            // 2: The old file needs to be shunted away first, then removed
            // after the operation completes.
            //
            // *: Never happens because it would be resolved as a chmod.
            //
            // !: Never happens because we don't permit creating/updating
            // special files.
            //
            // Note that there would be some marginal benefit in shunting *all*
            // the time, since if regular files exchanged places, we could
            // reuse both when doing the transfer. However, this doesn't seem
            // like a situation that would come up meaningfully often, and the
            // atomic replacement is generally more important.
            if old.is_dir() || new.is_dir() {
                shunted_name = Some(try!(self.shunt_file(&path)));
            }
            Ok(())
        }));

        if let Some(shunted) = shunted_name {
            try!(self.remove_general(&shunted, old));
        }

        dir.toggle_file(File(name, old));
        dir.toggle_file(File(name, new));

        Ok(ret)
    }

    fn chdir(&self, dir: &DirHandle, subdir: &OsStr)
             -> Result<DirHandle> {
        try!(assert_sane_filename(subdir));

        let path = dir.child(subdir);
        let md = try!(fs::symlink_metadata(&path));
        if !md.file_type().is_dir() {
            Err(Error::NotADir.into())
        } else if md.dev() != self.config.root_dev {
            Err(Error::ChdirXDev.into())
        } else {
            Ok(dir.subdir(subdir, None))
        }
    }

    fn synthdir(&self, dir: &mut DirHandle, subdir: &OsStr, mode: FileMode)
                -> DirHandle {
        dir.subdir(subdir, Some(mode))
    }

    fn rmdir(&self, subdir: &mut DirHandle) -> Result<()> {
        let dir = try!(subdir.parent().cloned().ok_or(Error::RmdirRoot));
        // Need to build the simple name (without the leading slash) so we can
        // toggle it from the parent dir hash anyway.
        let path = dir.child(subdir.name());
        let md = try!(fs::symlink_metadata(&path));
        try!(fs::remove_dir(&path));
        dir.toggle_file(File(&path, &FileData::Directory(md.mode() & 0o7777)));
        Ok(())
    }

    fn transfer(&self, dir: &DirHandle, file: File)
                -> Option<Box<StreamSource>> {
        match *file.1 {
            FileData::Regular(mode, _, _, expected_hash) => Some(Box::new(
                FileStreamSource {
                    file: fs::File::open(dir.child(file.0)),
                    dir: dir.clone(),
                    name: file.0.to_owned(),
                    mode: mode,
                    expected_hash: expected_hash,
                    config: self.config.clone(),
                    dao: self.dao.clone(),
                }
            )),
            _ => None,
        }
    }

    fn prepare(&self) -> Result<()> {
        // Reclaim any files left over from a crashed run
        try!(self.clean_scratch());

        // Walk all directories marked clean and check whether they are still
        // clean.
        let dao = self.dao.lock().unwrap();
        for clean_dir in try!(dao.iter_clean_dirs()) {
            let (path, expected_hash) = try!(clean_dir);
            let dir = DirHandle::root(path);

            for entry in try!(fs::read_dir(dir.full_path())) {
                let entry = try!(entry);

                let name = entry.file_name();
                if OsStr::new(".") == &name ||
                    OsStr::new("..") == &name
                {
                    continue;
                }

                let fd = try!(metadata_to_fd(
                    entry.path().as_os_str(), &try!(entry.metadata()),
                    &*dao, false, &*self.config));
                dir.toggle_file(File(&name, &fd));
            }

            if expected_hash != dir.hash() {
                try!(dao.set_dir_dirty(dir.full_path()));
            }
        }

        Ok(())
    }

    fn clean_up(&self) -> Result<()> {
        try!(self.clean_scratch());
        let _ = self.dao.lock().unwrap().prune_hash_cache(
            &self.config.root, self.config.cache_generation);
        Ok(())
    }
}

impl PosixReplica {
    /// Creates a new POSIX replica.
    ///
    /// `root` is the root directory for syncing purposes. `private_dir` is the
    /// already-existing directory created for use by the replica.
    /// `hmac_secret` and `block_size` specify the secret and size for block
    /// hashing.
    pub fn new(root: &str, private_dir: &str,
               hmac_secret: &[u8], block_size: usize)
               -> Result<Self> {
        let dao = try!(Dao::open(&format!("{}/db.sqlite", private_dir)));
        let cache_generation = try!(dao.next_generation());
        let root_dev = try!(fs::symlink_metadata(root)).dev();
        let private_dev = try!(fs::symlink_metadata(private_dir)).dev();

        if root_dev != private_dev {
            return Err(Error::PrivateXDev.into());
        }

        Ok(PosixReplica {
            config: Arc::new(Config {
                hmac_secret: hmac_secret.to_vec(),
                root: root.into(),
                private_dir: private_dir.into(),
                block_size: block_size,
                cache_generation: cache_generation,
                root_dev: root_dev,
            }),
            dao: Arc::new(Mutex::new(dao)),
            tmpix: AtomicUsize::new(0),
        })
    }

    /// Atomically creates a new "scratch" file within the replica's private
    /// directory.
    ///
    /// On success, the value produced by `create` is returned, as well as the
    /// full path to that file. The file is not unlinked implicitly, since the
    /// typical use of this call is to eventually rename the created file into
    /// its desired location.
    ///
    /// This function does not attempt to be secure. Since we write to a
    /// private directory rather than a globally-writable location like `/tmp`,
    /// any attacker that could trick us into overwriting something is already
    /// able to do quite a bit anyway. We use exclusive open mode on the file
    /// anyway, though, to reduce the probability of damage if we fail to
    /// prevent the user from running multiple ensync instances concurrently on
    /// the same private directory.
    fn scratch_file<T, F : FnMut (&Path) -> io::Result<T>>(
        &self, mut create: F) -> io::Result<(T,OsString)>
    {
        loop {
            let ix = self.tmpix.fetch_add(1, Ordering::SeqCst);
            let mut path: PathBuf = self.config.private_dir.clone().into();
            path.push(format!("scratch-{}", ix));

            match create(&path) {
                Ok(file) => return Ok((file, path.into_os_string())),
                Err(ref err) if io::ErrorKind::AlreadyExists == err.kind() =>
                    continue,
                Err(err) => return Err(err),
            }
        }
    }

    /// Like `scratch_file`, but specifically creates a regular file with the
    /// given mode, returning an open read/write handle to that file.
    fn scratch_regular(&self, mode: FileMode)
                       -> io::Result<(fs::File,OsString)> {
        let mut opts = fs::OpenOptions::new();
        opts.read(true).write(true)
            .mode(mode)
            .create_new(true);

        self.scratch_file(|path| opts.open(path))
    }

    /// Renames the path given by `old_path` to a new path in the same
    /// directory, for the purpose of moving it out of the way for a
    /// replacement.
    ///
    /// This renaming is distinct from what happens during reconciliation, as
    /// the renamed file is to be deleted immeiately after its replacement is
    /// established.
    fn shunt_file(&self, old_path: &OsStr) -> Result<OsString> {
        // Less persistent here than the reconciler's renamer in case
        // symlink_metadata() consistently succeeds for some other reason.
        for sfx in 1..65536 {
            let mut new_path = old_path.to_owned();
            new_path.push(format!("!{}", sfx));

            if let Err(err) = fs::symlink_metadata(&new_path) {
                if io::ErrorKind::NotFound == err.kind() {
                    try!(fs::rename(old_path, &new_path));
                    let _ = self.dao.lock().unwrap().rename_cache(
                        &old_path, &new_path);
                    return Ok(new_path);
                } else {
                    return Err(err.into());
                }
            }
        }

        Err(Error::AllSuffixesInUse.into())
    }

    /// Transfers `source` into `dir` using `xfer`.
    ///
    /// There is no "If-Match" / "If-None-Match" type of functionality here;
    /// such checks are the responsibility of the caller.
    ///
    /// `before_establish` is executed immediately before the file is created
    /// or renamed into its final place. If the callback fails, the operation
    /// is aborted and this function returns an error. The callback is used for
    /// things such as moving a directory to be replaced out of the way.
    fn put_file<F : FnOnce () -> Result<()>>(
        &self, dir: &mut DirHandle, source: File,
        xfer: Option<ContentAddressableSource>,
        before_establish: F)
        -> Result<FileData>
    {
        match *source.1 {
            FileData::Special =>
                panic!("Attempted to create generic special file locally"),

            FileData::Directory(mode) => {
                try!(before_establish());
                let path = dir.child(source.0);
                try!(fs::DirBuilder::new()
                     .mode(mode)
                     .create(&path));
                // mkdir() restricts the mode via umask, so we need to make
                // another call to get the mode we actually want.
                try!(fs::set_permissions(
                    &path, fs::Permissions::from_mode(mode)));
                Ok(source.1.clone())
            },

            FileData::Symlink(ref target) => {
                let (_, scratch_path) = try!(self.scratch_file(
                    |path| unix::fs::symlink(target, path)));
                try!(before_establish());
                try!(fs::rename(&scratch_path, &dir.child(source.0)));
                Ok(source.1.clone())
            },

            FileData::Regular(mode, _, _, _) => {
                let (mut scratch_file, scratch_path) =
                    try!(self.scratch_regular(0o600));
                if let Some(xfer) = xfer {
                    // Copy the file to the local filesystem
                    try!(self.xfer_file(&mut scratch_file, &xfer));
                    // Move anything out of the way as needed
                    try!(before_establish());
                    let new_path = dir.child(source.0);
                    // Atomically put into place after setting the mode
                    try!(fs::set_permissions(
                        &scratch_path, fs::Permissions::from_mode(mode)));
                    try!(fs::rename(&scratch_path, &new_path));
                    // Cache the content of the file, assuming that nobody
                    // modified it between us renaming it there and `stat()`ing
                    // it now.
                    //
                    // Quietly ignore errors here, since they don't affect
                    // correctness.
                    let _ = self.update_cache(
                        &new_path, &xfer.blocks, xfer.block_size);
                    Ok(source.1.clone())
                } else {
                    Err(Error::MissingXfer.into())
                }
            },
        }
    }

    /// Removes the file at `path` with data `fd` in the most appropriate way.
    fn remove_general(&self, path: &OsStr, fd: &FileData) -> Result<()> {
        match *fd {
            FileData::Regular(..) => self.hide_file(path),
            FileData::Directory(..) => Ok(try!(fs::remove_dir(path))),
            _ => Ok(try!(fs::remove_file(path))),
        }
    }

    /// Moves the file at `old_path` to a scratch location, so that it will be
    /// absent from its current location and scheduled for deletion, while
    /// still available in the block cache for efficient renames.
    fn hide_file(&self, old_path: &OsStr) -> Result<()> {
        let (_, new_path) = try!(self.scratch_regular(0o600));
        try!(fs::rename(old_path, &new_path));
        let _ = self.dao.lock().unwrap().rename_cache(
            &old_path, &new_path);
        Ok(())
    }

    /// Transfers the file described by `xfer` into `dst`.
    ///
    /// If an identical file is available locally, it is copied into `dst`,
    /// with a check that it actually has the content we expect it to.
    ///
    /// Otherwise, the file is copied block by block either from known-correct
    /// file blocks locally or by using `xfer.fetch` fo obtain them from the
    /// other replica.
    ///
    /// If this call fails, `dst` may be left in an intermediate state.
    fn xfer_file(&self, dst: &mut fs::File, xfer: &ContentAddressableSource)
                 -> Result<()> {
        // Try to copy from a known local file first. But don't bother if
        // `xfer` specifies zero blocks, since it's not worth consulting the
        // cache and "copying" another empty file.
        if !xfer.blocks.blocks.is_empty() &&
            self.copy_file_local(dst, &xfer.blocks.total, xfer.block_size)
        {
            return Ok(());
        }

        // We may have partially written the file in the above attempt, so
        // reset it to 0-length
        try!(dst.seek(io::SeekFrom::Start(0)));
        try!(dst.set_len(0));

        // Write the file a block at a time. Use local blocks when possible,
        // otherwise fetch from the transfer object.
        try!(blocks_to_stream(
            &xfer.blocks, dst, &self.config.hmac_secret[..],
            |hid| self.xfer_block(hid, &*xfer.fetch)));
        Ok(())
    }

    /// Copies a local file whose content is `hash` into `dst`.
    ///
    /// Returns `false` if an unexpected problem occurred. Returns `false` if
    /// no file is known to have content matching `hash`; this may occur even
    /// though data was written to `dst`. Returns `true` if a file was found
    /// matching `hash`.
    fn copy_file_local(&self, dst: &mut fs::File, hash: &HashId,
                       block_size: usize) -> bool {
        let srcname = self.dao.lock().unwrap().find_file_with_hash(hash);
        if let Ok(Some(srcname)) = srcname {
            // We think srcname has content equal to `hash`. Copy it to `dst`
            // while calculating the real hash at the same time.
            //
            // Errors are silently mapped into `UNKNOWN_HASH` so that we clear
            // the problematic cache entry.
            let actual_hash =
                fs::File::open(&srcname).and_then(
                    |src| stream_to_blocks(src, block_size,
                                           &self.config.hmac_secret[..],
                                           |_, data| dst.write_all(data)))
                .map(|bl| bl.total)
                .unwrap_or(UNKNOWN_HASH);

            if *hash == actual_hash {
                // Ok, it had the correct hash.
                true
            } else {
                // Hash wasn't what we expected. Remove the incorrect entry
                // from the cache and return failure.
                let _ = self.dao.lock().unwrap().delete_cache(&srcname);
                false
            }
        } else {
            // Nothing known with this hash
            false
        }
    }

    /// Transfers a single block.
    ///
    /// If a block matching `hash` is available locally, it is loaded into
    /// memory and returned. Otherwise, `fetch` is used to stream the data from
    /// the other replica.
    fn xfer_block(&self, hash: &HashId, fetch: &BlockFetch)
                  -> io::Result<Box<io::Read>> {
        if let Some(data) = self.fetch_block_local(hash) {
            Ok(Box::new(io::Cursor::new(data)))
        } else {
            fetch.fetch(hash)
        }
    }

    /// Attempts to fetch a block matching `hash` from the local filesystem.
    ///
    /// If no such block is available, or an error occurs, or the file that
    /// should have had the desired content no longer does, `None` is returned.
    /// Otherwise, a `Vec` of the data corresponding to `hash` is returned.
    fn fetch_block_local(&self, hash: &HashId) -> Option<Vec<u8>> {
        // See if we know about any blocks with this hash
        let pbo = self.dao.lock().unwrap()
            .find_block_with_hash(hash);
        if let Ok(Some((path, bs, off))) = pbo {
            // Looks like we know about one. Try to read the block in. Quietly
            // drop errors; if anything fails, the hash of `data` will not
            // match `hash`.
            let mut data: Vec<u8> = Vec::new();
            let _ = fs::File::open(&path).and_then(|mut src| {
                try!(src.seek(io::SeekFrom::Start((off * bs) as u64)));
                try!(src.take(bs as u64).read_to_end(&mut data));
                Ok(())
            });

            // Make sure we read the correct data in
            if *hash == hash_block(&self.config.hmac_secret[..], &data[..]) {
                // Matched
                Some(data)
            } else {
                // No match or error. Remove the offending cache entry before
                // returning failure.
                let _ = self.dao.lock().unwrap().delete_cache(&path);
                None
            }
        } else {
            None
        }
    }

    fn update_cache(&self, path: &OsStr, bl: &BlockList,
                    block_size: usize) -> Result<()> {
        let md = try!(fs::symlink_metadata(path));
        let mtime = md.mtime();
        let ino = md.ino();
        let size = md.size();

        try!(self.dao.lock().unwrap().cache_file_hashes(
            path, &bl.total, &bl.blocks[..], block_size,
            &InodeStatus { mtime: mtime, ino: ino, size: size },
            self.config.cache_generation));
        Ok(())
    }

    /// Checks whether the file at `path` matches `fd`.
    ///
    /// If it matches, returns `Ok`. Otherwise, or if any error occurs, returns
    /// `Err`.
    fn check_matches(&self, path: &OsStr, fd: &FileData)
                     -> Result<()> {
        let dao = self.dao.lock().unwrap();
        let curr_fd = try!(metadata_to_fd(
            &path, &try!(fs::symlink_metadata(&path)),
            &dao, false, &*self.config));
        if !curr_fd.matches(fd) {
            Err(Error::NotMatched.into())
        } else {
            Ok(())
        }
    }

    /// Removes all scratch files in the private directory.
    fn clean_scratch(&self) -> Result<()> {
        for file in try!(fs::read_dir(&self.config.private_dir)) {
            let file = try!(file);
            if file.file_name().to_str().map_or(
                false, |name| name.starts_with("scratch"))
            {
                let path = file.path();
                let _ = fs::remove_file(&path);
                let _ = self.dao.lock().unwrap()
                    .delete_cache(path.as_os_str());
            }
        }
        Ok(())
    }
}

/// The `TransferOut` used for the POSIX replica.
struct FileStreamSource {
    /// A read handle to the file, or `Err` if the file was not able to be
    /// opened. Errors are reflected from `io::Read::read()`.
    file: io::Result<fs::File>,
    /// Shared handle to the directory containing this file.
    dir: DirHandle,
    /// The name of the file being read.
    name: OsString,
    /// The mode bits on the file (as per `FileData`), used to update the
    /// directory hash.
    mode: FileMode,
    /// The hash that was originally reported for the transfer, so the
    /// directory hash can be updated.
    expected_hash: HashId,

    config: Arc<Config>,
    dao: Arc<Mutex<Dao>>,
}

impl io::Read for FileStreamSource {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self.file {
            Ok(ref mut file) => file.read(buf),
            Err(ref mut err) => {
                let kind = err.kind();
                Err(mem::replace(
                    err, io::Error::new(kind, "Error already reported")))
            },
        }
    }
}

impl StreamSource for FileStreamSource {
    fn finish(&mut self, blocks: &BlockList) -> Result<()> {
        let path = self.dir.child(&self.name);
        // Make a best effort to update the cache
        if let Ok(md) = fs::symlink_metadata(&path) {
            let stat = InodeStatus {
                mtime: md.mtime(), ino: md.ino(), size: md.size(),
            };
            let _ = self.dao.lock().unwrap().cache_file_hashes(
                &path, &blocks.total, &blocks.blocks[..],
                self.config.block_size, &stat, self.config.cache_generation);
        }

        // Update the hash of the containing directory, removing whatever
        // placeholder or possibly incorrect entry had been there before.
        self.dir.toggle_file(File(&self.name, &FileData::Regular(
            self.mode, 0, 0, self.expected_hash)));
        self.dir.toggle_file(File(&self.name, &FileData::Regular(
            self.mode, 0, 0, blocks.total)));

        Ok(())
    }
}

#[cfg(test)]
mod test {
    use std::collections::HashMap;
    use std::ffi::CString;
    use std::fs;
    use std::io::{self,Read,Write};
    use std::os::unix;
    use std::os::unix::fs::{DirBuilderExt, MetadataExt};
    use std::path::Path;
    use std::sync::Arc;
    use libc;

    use tempdir::TempDir;

    use defs::*;
    use defs::test_helpers::*;
    use replica::*;
    use block_xfer;
    use super::*;

    static SECRET: &'static str = "secret";
    const BLOCK_SZ: usize = 4;

    fn new_dirs() -> (TempDir, TempDir) {
        let root = TempDir::new("posix-root").unwrap();
        let private = TempDir::new("posix-private").unwrap();
        (root, private)
    }

    fn new_in(root: &TempDir, private: &TempDir) -> PosixReplica {
        PosixReplica::new(
            root.path().to_str().unwrap(),
            private.path().to_str().unwrap(),
            SECRET.as_bytes(), BLOCK_SZ).unwrap()
    }

    fn new_simple() -> (TempDir,TempDir,PosixReplica) {
        let (root, private) = new_dirs();
        let replica = new_in(&root, &private);

        (root, private, replica)
    }

    fn spit<P : AsRef<Path>>(path: P, text: &str) {
        let mut out = fs::File::create(path).unwrap();
        out.write_all(text.as_bytes()).unwrap();
    }

    fn slurp<P : AsRef<Path>>(path: P) -> String {
        let mut inf = fs::File::open(path).unwrap();
        let mut ret = String::new();
        inf.read_to_string(&mut ret).unwrap();
        ret
    }

    struct MemoryBlockFetch {
        blocks: HashMap<HashId,Vec<u8>>,
    }

    impl block_xfer::BlockFetch for MemoryBlockFetch {
        fn fetch(&self, block: &HashId) -> io::Result<Box<Read>> {
            Ok(Box::new(io::Cursor::new(
                self.blocks.get(block).expect("Unexpected block fetched")
                    .clone())))
        }
    }

    fn make_ca_source(text: &str) -> block_xfer::ContentAddressableSource {
        let mut blocks = HashMap::new();
        let bl = block_xfer::stream_to_blocks(&mut text.as_bytes(), BLOCK_SZ,
                                              SECRET.as_bytes(), |hash, data| {
            blocks.insert(*hash, data.to_vec());
            Ok(())
        }).unwrap();

        block_xfer::ContentAddressableSource {
            blocks: bl,
            block_size: BLOCK_SZ,
            fetch: Arc::new(MemoryBlockFetch {
                blocks: blocks,
            })
        }
    }

    fn make_ca_empty_source(text: &str)
                            -> block_xfer::ContentAddressableSource {
        let mut xfer = make_ca_source(text);
        xfer.fetch = Arc::new(MemoryBlockFetch {
            blocks: HashMap::new(),
        });
        xfer
    }

    #[test]
    fn trivial() {
        let (_root, _private, replica) = new_simple();
        replica.prepare().unwrap();
        replica.clean_up().unwrap();
    }

    #[test]
    fn list_and_transfer_out_regular_file() {
        let (root, _private, replica) = new_simple();
        spit(root.path().join("foo"), "hello world");

        replica.prepare().unwrap();
        let mut dir = replica.root().unwrap();
        let list = replica.list(&mut dir).unwrap();
        assert_eq!(1, list.len());
        assert_eq!(oss("foo"), list[0].0);

        if let FileData::Regular(mode, size, _, hash) = list[0].1 {
            assert_eq!(0o644, mode);
            assert_eq!("hello world".as_bytes().len() as FileSize, size);
            assert_eq!(block_xfer::stream_to_blocks(
                "hello world".as_bytes(), BLOCK_SZ, SECRET.as_bytes(),
                |_, _| Ok(())).unwrap().total, hash);

            let mut data = Vec::new();
            replica.transfer(&dir, File(&list[0].0, &list[0].1)).unwrap()
                .read_to_end(&mut data).unwrap();
            assert_eq!("hello world".as_bytes(), &data[..]);
        } else {
            panic!("Unexpected file returned: {:?}", list[0].1);
        }
    }

    #[test]
    fn list_non_regular_files() {
        let (root, _private, replica) = new_simple();
        fs::DirBuilder::new().mode(0o700).create(
            root.path().join("dir")).unwrap();
        unix::fs::symlink("target", root.path().join("sym")).unwrap();
        unsafe {
            assert_eq!(0, libc::mkfifo(
                CString::new(root.path().join("fifo").to_str().unwrap())
                    .unwrap().as_ptr(), 0o000));
        }

        replica.prepare().unwrap();
        let mut dir = replica.root().unwrap();
        let list = replica.list(&mut dir).unwrap();
        assert_eq!(3, list.len());
        for (name, fd) in list {
            if oss("dir") == name {
                assert_eq!(FileData::Directory(0o700), fd);
            } else if oss("sym") == name {
                assert_eq!(FileData::Symlink(oss("target")), fd);
            } else if oss("fifo") == name {
                assert_eq!(FileData::Special, fd);
            } else {
                panic!("Unexpected filename: {:?}", name);
            }
        }
    }

    #[test]
    fn create_regular_file_via_xfer() {
        let (root, _private, replica) = new_simple();

        replica.prepare().unwrap();
        let mut dir = replica.root().unwrap();

        let xfer = make_ca_source("Three pounds of VAX!");
        replica.create(&mut dir, File(
            &oss("vax"), &FileData::Regular(
                0o600, 0, 0, xfer.blocks.total)),
            Some(xfer)).unwrap();

        assert_eq!("Three pounds of VAX!",
                   &slurp(root.path().join("vax")));
        assert_eq!(0o600, fs::symlink_metadata(root.path().join("vax"))
                   .unwrap().mode() & 0o7777);
    }

    #[test]
    fn create_regular_file_with_perm_777() {
        let (root, _private, replica) = new_simple();

        replica.prepare().unwrap();
        let mut dir = replica.root().unwrap();

        let xfer = make_ca_source("Three pounds of VAX!");
        replica.create(&mut dir, File(
            &oss("vax"), &FileData::Regular(
                0o777, 0, 0, xfer.blocks.total)),
            Some(xfer)).unwrap();

        assert_eq!("Three pounds of VAX!",
                   &slurp(root.path().join("vax")));
        assert_eq!(0o777, fs::symlink_metadata(root.path().join("vax"))
                   .unwrap().mode() & 0o7777);
    }

    #[test]
    fn create_directory() {
        let (root, _private, replica) = new_simple();

        replica.prepare().unwrap();
        let mut dir = replica.root().unwrap();
        replica.create(&mut dir, File(&oss("d"), &FileData::Directory(0o700)),
                       None).unwrap();

        let md = fs::symlink_metadata(root.path().join("d")).unwrap();
        assert!(md.is_dir());
        assert_eq!(0o700, md.mode() & 0o7777);
    }

    #[test]
    fn create_symlink() {
        let (root, _private, replica) = new_simple();

        replica.prepare().unwrap();
        let mut dir = replica.root().unwrap();
        replica.create(&mut dir, File(&oss("sym"),
                                      &FileData::Symlink(oss("plugh"))),
                       None).unwrap();

        let md = fs::symlink_metadata(root.path().join("sym")).unwrap();
        assert!(md.file_type().is_symlink());
        assert_eq!(oss("plugh"), fs::read_link(root.path().join("sym"))
                   .unwrap().into_os_string());
    }

    #[test]
    fn create_fails_if_alread_exists() {
        let (root, _private, replica) = new_simple();

        replica.prepare().unwrap();
        let mut dir = replica.root().unwrap();
        let _ = replica.list(&mut dir).unwrap();

        spit(root.path().join("foo"), "exists");

        assert!(replica.create(&mut dir, File(&oss("foo"),
                                              &FileData::Directory(0o700)),
                               None).is_err());
    }

    #[test]
    fn replace_file_with_file() {
        let (root, _private, replica) = new_simple();

        replica.prepare().unwrap();
        let mut dir = replica.root().unwrap();

        spit(root.path().join("foo"), "Three pounds of VAX");
        let list = replica.list(&mut dir).unwrap();
        assert_eq!(1, list.len());

        let xfer = make_ca_source("Three pounds of flax");
        replica.update(&mut dir, &oss("foo"),
                       &list[0].1,
                       &FileData::Regular(
                           0o611, 0, 0, xfer.blocks.total),
                       Some(xfer)).unwrap();

        let md = fs::symlink_metadata(root.path().join("foo")).unwrap();
        assert_eq!(0o611, md.mode() & 0o7777);
        assert_eq!("Three pounds of flax", slurp(root.path().join("foo")));
    }

    #[test]
    fn chmod_file() {
        let (root, _private, replica) = new_simple();

        replica.prepare().unwrap();
        let mut dir = replica.root().unwrap();

        spit(root.path().join("foo"), "Three pounds of VAX");
        let old_ino = fs::symlink_metadata(root.path().join("foo"))
            .unwrap().ino();

        let list = replica.list(&mut dir).unwrap();
        assert_eq!(1, list.len());

        let new_fd = if let FileData::Regular(_, _, _, hash) = list[0].1 {
            FileData::Regular(0o611, 0, 0, hash)
        } else {
            panic!("Unexpected file data: {:?}", list[0].1);
        };

        replica.update(&mut dir, &oss("foo"),
                       &list[0].1, &new_fd, None).unwrap();

        let md = fs::symlink_metadata(root.path().join("foo")).unwrap();
        assert_eq!(0o611, md.mode() & 0o7777);
        assert_eq!("Three pounds of VAX", slurp(root.path().join("foo")));
        assert_eq!(old_ino, md.ino());
    }

    #[test]
    fn replace_file_with_dir() {
        let (root, _private, replica) = new_simple();

        replica.prepare().unwrap();
        let mut dir = replica.root().unwrap();

        spit(root.path().join("foo"), "foo");
        let list = replica.list(&mut dir).unwrap();
        assert_eq!(1, list.len());

        replica.update(&mut dir, &oss("foo"),
                       &list[0].1, &FileData::Directory(0o711), None)
            .unwrap();

        let md = fs::symlink_metadata(root.path().join("foo")).unwrap();
        assert!(md.file_type().is_dir());
        assert_eq!(0o711, md.mode() & 0o7777);
    }

    #[test]
    fn replace_file_with_symlink() {
        let (root, _private, replica) = new_simple();

        replica.prepare().unwrap();
        let mut dir = replica.root().unwrap();

        spit(root.path().join("foo"), "foo");
        let list = replica.list(&mut dir).unwrap();
        assert_eq!(1, list.len());

        replica.update(&mut dir, &oss("foo"),
                       &list[0].1, &FileData::Symlink(oss("plugh")), None)
            .unwrap();

        let md = fs::symlink_metadata(root.path().join("foo")).unwrap();
        assert!(md.file_type().is_symlink());
        assert_eq!(oss("plugh"), fs::read_link(root.path().join("foo"))
                   .unwrap().into_os_string());
    }

    #[test]
    fn replace_dir_with_file() {
        let (root, _private, replica) = new_simple();

        replica.prepare().unwrap();
        let mut dir = replica.root().unwrap();

        fs::DirBuilder::new().mode(0o700).create(
            root.path().join("foo")).unwrap();
        let list = replica.list(&mut dir).unwrap();
        assert_eq!(1, list.len());

        let xfer = make_ca_source("Three pounds of flax");
        replica.update(&mut dir, &oss("foo"),
                       &list[0].1,
                       &FileData::Regular(
                           0o611, 0, 0, xfer.blocks.total),
                       Some(xfer)).unwrap();

        let md = fs::symlink_metadata(root.path().join("foo")).unwrap();
        assert_eq!(0o611, md.mode() & 0o7777);
        assert_eq!("Three pounds of flax", slurp(root.path().join("foo")));
    }

    #[test]
    fn chmod_dir() {
        let (root, _private, replica) = new_simple();

        replica.prepare().unwrap();
        let mut dir = replica.root().unwrap();

        fs::DirBuilder::new().mode(0o700).create(
            root.path().join("foo")).unwrap();
        let list = replica.list(&mut dir).unwrap();
        assert_eq!(1, list.len());

        replica.update(&mut dir, &oss("foo"),
                       &list[0].1,
                       &FileData::Directory(0o711),
                       None).unwrap();

        let md = fs::symlink_metadata(root.path().join("foo")).unwrap();
        assert!(md.is_dir());
        assert_eq!(0o711, md.mode() & 0o7777);
    }

    #[test]
    fn replace_dir_with_symlink() {
        let (root, _private, replica) = new_simple();

        replica.prepare().unwrap();
        let mut dir = replica.root().unwrap();

        fs::DirBuilder::new().mode(0o700).create(
            root.path().join("foo")).unwrap();
        let list = replica.list(&mut dir).unwrap();
        assert_eq!(1, list.len());

        replica.update(&mut dir, &oss("foo"),
                       &list[0].1,
                       &FileData::Symlink(oss("plugh")),
                       None).unwrap();

        let md = fs::symlink_metadata(root.path().join("foo")).unwrap();
        assert!(md.file_type().is_symlink());
        assert_eq!(oss("plugh"), fs::read_link(root.path().join("foo"))
                   .unwrap().into_os_string());
    }

    #[test]
    fn replace_symlink_with_file() {
        let (root, _private, replica) = new_simple();

        replica.prepare().unwrap();
        let mut dir = replica.root().unwrap();

        unix::fs::symlink("plugh", root.path().join("foo")).unwrap();
        let list = replica.list(&mut dir).unwrap();
        assert_eq!(1, list.len());

        let xfer = make_ca_source("Three pounds of flax");
        replica.update(&mut dir, &oss("foo"),
                       &list[0].1,
                       &FileData::Regular(
                           0o611, 0, 0, xfer.blocks.total),
                       Some(xfer)).unwrap();

        let md = fs::symlink_metadata(root.path().join("foo")).unwrap();
        assert_eq!(0o611, md.mode() & 0o7777);
        assert_eq!("Three pounds of flax", slurp(root.path().join("foo")));
    }

    #[test]
    fn replace_symlink_with_dir() {
        let (root, _private, replica) = new_simple();

        replica.prepare().unwrap();
        let mut dir = replica.root().unwrap();

        unix::fs::symlink("plugh", root.path().join("foo")).unwrap();
        let list = replica.list(&mut dir).unwrap();
        assert_eq!(1, list.len());

        replica.update(&mut dir, &oss("foo"),
                       &list[0].1,
                       &FileData::Directory(0o711),
                       None).unwrap();

        let md = fs::symlink_metadata(root.path().join("foo")).unwrap();
        assert!(md.is_dir());
        assert_eq!(0o711, md.mode() & 0o7777);
    }

    #[test]
    fn replace_symlink_with_symlink() {
        let (root, _private, replica) = new_simple();

        replica.prepare().unwrap();
        let mut dir = replica.root().unwrap();

        unix::fs::symlink("plugh", root.path().join("foo")).unwrap();
        let list = replica.list(&mut dir).unwrap();
        assert_eq!(1, list.len());

        replica.update(&mut dir, &oss("foo"),
                       &list[0].1,
                       &FileData::Symlink(oss("xyzzy")),
                       None).unwrap();

        let md = fs::symlink_metadata(root.path().join("foo")).unwrap();
        assert!(md.file_type().is_symlink());
        assert_eq!(oss("xyzzy"), fs::read_link(root.path().join("foo"))
                   .unwrap().into_os_string());
    }

    #[test]
    fn replace_fails_if_not_matched() {
        let (root, _private, replica) = new_simple();

        replica.prepare().unwrap();
        let mut dir = replica.root().unwrap();

        spit(root.path().join("foo"), "foo");
        let list = replica.list(&mut dir).unwrap();
        assert_eq!(1, list.len());

        // Given how fast this test is, it is possible that changing to "bar"
        // wouldn't be detected, since the mtime could be the same and the file
        // size and inode would not change.
        spit(root.path().join("foo"), "quux");

        assert!(replica.update(&mut dir, &oss("foo"),
                               &list[0].1,
                               &FileData::Directory(0o700),
                               None)
                .is_err());

        assert_eq!("quux", &slurp(root.path().join("foo")));
    }

    #[test]
    fn remove_file() {
        let (root, _private, replica) = new_simple();

        replica.prepare().unwrap();
        let mut dir = replica.root().unwrap();

        spit(root.path().join("foo"), "content");
        let list = replica.list(&mut dir).unwrap();
        assert_eq!(1, list.len());

        replica.remove(&mut dir, File(&oss("foo"), &list[0].1))
            .unwrap();

        assert!(fs::symlink_metadata(root.path().join("foo"))
                .is_err());
    }

    #[test]
    fn remove_dir() {
        let (root, _private, replica) = new_simple();

        replica.prepare().unwrap();
        let mut dir = replica.root().unwrap();

        fs::DirBuilder::new().mode(0o700).create(
            root.path().join("foo")).unwrap();
        let list = replica.list(&mut dir).unwrap();
        assert_eq!(1, list.len());

        replica.remove(&mut dir, File(&oss("foo"), &list[0].1))
            .unwrap();

        assert!(fs::symlink_metadata(root.path().join("foo"))
                .is_err());
    }

    #[test]
    fn remove_symlink() {
        let (root, _private, replica) = new_simple();

        replica.prepare().unwrap();
        let mut dir = replica.root().unwrap();

        unix::fs::symlink("plugh", root.path().join("foo")).unwrap();
        let list = replica.list(&mut dir).unwrap();
        assert_eq!(1, list.len());

        replica.remove(&mut dir, File(&oss("foo"), &list[0].1))
            .unwrap();

        assert!(fs::symlink_metadata(root.path().join("foo"))
                .is_err());
    }

    #[test]
    fn remove_fails_if_not_matched() {
        let (root, _private, replica) = new_simple();

        replica.prepare().unwrap();
        let mut dir = replica.root().unwrap();

        spit(root.path().join("foo"), "content");
        let list = replica.list(&mut dir).unwrap();
        assert_eq!(1, list.len());

        // Again, we need a length change to ensure the change is detected due
        // to how fast this test executes.
        spit(root.path().join("foo"), "new-content");

        assert!(replica.remove(&mut dir, File(&oss("foo"), &list[0].1))
                .is_err());
        assert_eq!("new-content", &slurp(root.path().join("foo")));
    }

    #[test]
    fn file_copies_optimised() {
        let (root, _private, replica) = new_simple();

        replica.prepare().unwrap();
        let mut dir = replica.root().unwrap();

        spit(root.path().join("foo"), "Three pounds of VAX");
        let list = replica.list(&mut dir).unwrap();
        assert_eq!(1, list.len());

        let xfer = make_ca_empty_source("Three pounds of VAX");
        replica.create(&mut dir, File(&oss("bar"), &list[0].1),
                       Some(xfer))
            .unwrap();

        assert_eq!("Three pounds of VAX", &slurp(root.path().join("bar")));
    }

    #[test]
    fn file_block_copies_optimised() {
        let (root, _private, replica) = new_simple();

        replica.prepare().unwrap();
        let mut dir = replica.root().unwrap();

        spit(root.path().join("race"), "racecars");
        spit(root.path().join("ways"), "pathways");
        let _ = replica.list(&mut dir).unwrap();

        let xfer = make_ca_empty_source("raceways");
        replica.create(
            &mut dir, File(
                &oss("raceways"),
                &FileData::Regular(
                    0o600, 8, 0, make_ca_source("raceways")
                        .blocks.total)),
            Some(xfer)).unwrap();

        assert_eq!("raceways", &slurp(root.path().join("raceways")));
    }

    #[test]
    fn delete_create_renames_optimised() {
        let (root, _private, replica) = new_simple();

        replica.prepare().unwrap();
        let mut dir = replica.root().unwrap();

        spit(root.path().join("foo"), "Three pounds of VAX");
        let list = replica.list(&mut dir).unwrap();
        assert_eq!(1, list.len());

        replica.remove(&mut dir, File(&oss("foo"), &list[0].1))
            .unwrap();

        let xfer = make_ca_empty_source("Three pounds of VAX");
        replica.create(&mut dir, File(&oss("bar"), &list[0].1),
                       Some(xfer)).unwrap();

        assert_eq!("Three pounds of VAX", &slurp(root.path().join("bar")));
    }

    #[test]
    fn file_copy_optimisation_handles_corruption() {
        let (root, _private, replica) = new_simple();

        replica.prepare().unwrap();
        let mut dir = replica.root().unwrap();

        spit(root.path().join("foo"), "Three pounds of VAX");
        let list = replica.list(&mut dir).unwrap();
        assert_eq!(1, list.len());

        // Concurrent modification that won't be reflected in the hash cache
        spit(root.path().join("foo"), "Three kilos of flax");

        let xfer = make_ca_source("Three pounds of VAX");
        replica.create(&mut dir, File(&oss("bar"), &list[0].1),
                       Some(xfer))
            .unwrap();

        assert_eq!("Three pounds of VAX", &slurp(root.path().join("bar")));
    }

    #[test]
    fn block_copy_optimisation_handles_corruption() {
        let (root, _private, replica) = new_simple();

        replica.prepare().unwrap();
        let mut dir = replica.root().unwrap();

        spit(root.path().join("race"), "racecars");
        spit(root.path().join("ways"), "pathways");
        let _ = replica.list(&mut dir).unwrap();

        spit(root.path().join("race"), "RACECARS");
        spit(root.path().join("ways"), "PATHWAYS");

        let xfer = make_ca_source("raceways");
        replica.create(
            &mut dir, File(
                &oss("raceways"),
                &FileData::Regular(
                    0o600, 8, 0, make_ca_source("raceways")
                        .blocks.total)),
            Some(xfer)).unwrap();

        assert_eq!("raceways", &slurp(root.path().join("raceways")));
    }


    #[test]
    fn chdir_not_found() {
        let (_root, _private, replica) = new_simple();

        replica.prepare().unwrap();
        let dir = replica.root().unwrap();

        assert!(replica.chdir(&dir, &oss("foo"))
                .is_err());
    }

    #[test]
    fn chdir_into_non_dir() {
        let (root, _private, replica) = new_simple();
        spit(root.path().join("foo"), "bar");

        replica.prepare().unwrap();
        let dir = replica.root().unwrap();
        assert!(replica.chdir(&dir, &oss("foo"))
                .is_err());
    }

    #[test]
    fn chdir_into_subdirs() {
        let (root, _private, replica) = new_simple();
        fs::DirBuilder::new().mode(0o700).create(
            root.path().join("child")).unwrap();
        fs::DirBuilder::new().mode(0o700).create(
            root.path().join("child").join("grandchild"))
            .unwrap();
        unix::fs::symlink(
            "target", root.path().join("child")
                .join("grandchild").join("foo")).unwrap();

        replica.prepare().unwrap();

        let dir = replica.root().unwrap();

        let mut subdir = replica.chdir(&dir, &oss("child")).unwrap();
        let s_list = replica.list(&mut subdir).unwrap();
        assert_eq!(1, s_list.len());
        assert_eq!(&oss("grandchild"), &s_list[0].0);
        assert_eq!(&FileData::Directory(0o700), &s_list[0].1);

        let mut ssdir = replica.chdir(&subdir, &oss("grandchild")).unwrap();
        let ss_list = replica.list(&mut ssdir).unwrap();
        assert_eq!(1, ss_list.len());
        assert_eq!(&oss("foo"), &ss_list[0].0);
        assert_eq!(&FileData::Symlink(oss("target")),
                   &ss_list[0].1);
    }

    #[test]
    fn synthdir_tree() {
        let (_root, _private, replica) = new_simple();

        let mut dir = replica.root().unwrap();
        let mut dfoo = replica.synthdir(&mut dir, &oss("foo"), 0o700);
        let mut dbar = replica.synthdir(&mut dfoo, &oss("bar"), 0o740);
        let mut dbaz = replica.synthdir(&mut dfoo, &oss("baz"), 0o770);

        assert_eq!(0, replica.list(&mut dir).unwrap().len());
        assert_eq!(0, replica.list(&mut dfoo).unwrap().len());

        replica.create(&mut dbar, File(&oss("plugh"),
                                       &FileData::Symlink(oss("xyzzy"))),
                       None).unwrap();
        replica.create(&mut dbaz, File(&oss("fee"),
                                       &FileData::Symlink(oss("fum"))),
                       None).unwrap();

        let foo_list = replica.list(&mut dfoo).unwrap();
        assert_eq!(2, foo_list.len());
        for (name, fd) in foo_list {
            if &oss("bar") == &name {
                assert_eq!(FileData::Directory(0o740), fd);
            } else if &oss("baz") == &name {
                assert_eq!(FileData::Directory(0o770), fd);
            } else {
                panic!("Unexpected filename: {:?}", name);
            }
        }
    }

    #[test]
    fn rmdir_success() {
        let (root, _private, replica) = new_simple();
        fs::DirBuilder::new().mode(0o700).create(
            root.path().join("child")).unwrap();

        replica.prepare().unwrap();

        let mut dir = replica.root().unwrap();
        let mut subdir = replica.chdir(&dir, &oss("child")).unwrap();
        replica.rmdir(&mut subdir).unwrap();

        assert_eq!(0, replica.list(&mut dir).unwrap().len());
    }

    #[test]
    fn cannot_rmdir_root() {
        let (_root, _private, replica) = new_simple();

        replica.prepare().unwrap();
        let mut dir = replica.root().unwrap();

        assert!(replica.rmdir(&mut dir).is_err());
    }

    #[test]
    fn simple_dirty_tracking() {
        let (root, private) = new_dirs();
        let subdir = root.path().join("child");
        fs::DirBuilder::new().mode(0o700).create(&subdir).unwrap();
        unix::fs::symlink("target", subdir.join("sym")).unwrap();

        {
            let replica = new_in(&root, &private);
            replica.prepare().unwrap();
            let mut rdir = replica.root().unwrap();
            assert!(replica.is_dir_dirty(&rdir));
            replica.list(&mut rdir).unwrap();

            let mut cdir = replica.chdir(&rdir, &oss("child")).unwrap();
            assert!(replica.is_dir_dirty(&cdir));
            replica.list(&mut cdir).unwrap();

            replica.set_dir_clean(&cdir).unwrap();
            replica.set_dir_clean(&rdir).unwrap();
            replica.clean_up().unwrap();
        }

        {
            let replica = new_in(&root, &private);
            replica.prepare().unwrap();
            let rdir = replica.root().unwrap();
            assert!(!replica.is_dir_dirty(&rdir));
            let cdir = replica.chdir(&rdir, &oss("child")).unwrap();
            assert!(!replica.is_dir_dirty(&cdir));
        }

        unix::fs::symlink("xyzzy", subdir.join("plugh")).unwrap();
        {
            let replica = new_in(&root, &private);
            replica.prepare().unwrap();
            let rdir = replica.root().unwrap();
            assert!(replica.is_dir_dirty(&rdir));
            let cdir = replica.chdir(&rdir, &oss("child")).unwrap();
            assert!(replica.is_dir_dirty(&cdir));
        }
    }

    #[test]
    fn own_edits_accounted_for_in_dirty_tracking() {
        let (root, private) = new_dirs();
        let subdir = root.path().join("child");
        fs::DirBuilder::new().mode(0o700).create(&subdir).unwrap();

        {
            let replica = new_in(&root, &private);
            replica.prepare().unwrap();

            let mut rdir = replica.root().unwrap();
            replica.list(&mut rdir).unwrap();

            let mut cdir = replica.chdir(&rdir, &oss("child")).unwrap();
            replica.list(&mut cdir).unwrap();

            replica.create(&mut cdir, File(
                &oss("sym"), &FileData::Symlink(oss("target"))), None)
                .unwrap();
            replica.set_dir_clean(&cdir).unwrap();
            replica.set_dir_clean(&rdir).unwrap();
            replica.clean_up().unwrap();
        }

        {
            let replica = new_in(&root, &private);
            replica.prepare().unwrap();

            let rdir = replica.root().unwrap();
            assert!(!replica.is_dir_dirty(&rdir));
            let cdir = replica.chdir(&rdir, &oss("child")).unwrap();
            assert!(!replica.is_dir_dirty(&cdir));
        }
    }

    #[test]
    fn dir_clean_marker_tracks_placeholder_hash_of_untransferred_regular() {
        let (root, private) = new_dirs();
        let subdir = root.path().join("child");
        fs::DirBuilder::new().mode(0o700).create(&subdir).unwrap();
        spit(subdir.join("foo"), "bar");

        {
            let replica = new_in(&root, &private);
            replica.prepare().unwrap();

            let mut rdir = replica.root().unwrap();
            replica.list(&mut rdir).unwrap();

            let mut cdir = replica.chdir(&rdir, &oss("child")).unwrap();
            replica.list(&mut cdir).unwrap();

            replica.set_dir_clean(&cdir).unwrap();
            replica.set_dir_clean(&rdir).unwrap();
            replica.clean_up().unwrap();
        }

        {
            let replica = new_in(&root, &private);
            replica.prepare().unwrap();

            let rdir = replica.root().unwrap();
            assert!(!replica.is_dir_dirty(&rdir));
            let cdir = replica.chdir(&rdir, &oss("child")).unwrap();
            assert!(!replica.is_dir_dirty(&cdir));
        }
    }

    #[test]
    fn dir_clean_marker_tracks_actual_hash_of_transferred_regular() {
        let (root, private) = new_dirs();
        let subdir = root.path().join("child");
        fs::DirBuilder::new().mode(0o700).create(&subdir).unwrap();
        spit(subdir.join("foo"), "bar");

        {
            let replica = new_in(&root, &private);
            replica.prepare().unwrap();

            let mut rdir = replica.root().unwrap();
            replica.list(&mut rdir).unwrap();

            let mut cdir = replica.chdir(&rdir, &oss("child")).unwrap();
            let list = replica.list(&mut cdir).unwrap();
            assert_eq!(1, list.len());
            let (fname, fdata) = list.into_iter().next().unwrap();

            // Concurrent modification
            spit(subdir.join("foo"), "plugh");

            let mut ss = replica.transfer(&cdir, File(&fname, &fdata)).unwrap();
            let bl = block_xfer::stream_to_blocks(&mut ss, BLOCK_SZ,
                                                  SECRET.as_bytes(),
                                                  |_, _| Ok(()))
                .unwrap();
            ss.finish(&bl).unwrap();

            replica.set_dir_clean(&cdir).unwrap();
            replica.set_dir_clean(&rdir).unwrap();
            replica.clean_up().unwrap();
        }

        {
            let replica = new_in(&root, &private);
            replica.prepare().unwrap();

            let rdir = replica.root().unwrap();
            assert!(!replica.is_dir_dirty(&rdir));
            let cdir = replica.chdir(&rdir, &oss("child")).unwrap();
            assert!(!replica.is_dir_dirty(&cdir));
        }
    }

    #[test]
    fn insane_filenames_blocked() {
        let (_root, _private, replica) = new_simple();

        let dir = replica.root().unwrap();
        assert!(replica.chdir(&dir, &oss(".")).is_err());
        assert!(replica.chdir(&dir, &oss("..")).is_err());
        assert!(replica.chdir(&dir, &oss("a/b")).is_err());
        assert!(replica.chdir(&dir, &oss("a\x00b")).is_err());
    }
}
