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

#![allow(dead_code)]

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
        Ok(FileData::Directory(md.mode()))
    } else if typ.is_symlink() {
        let target = try!(fs::read_link(path)).into_os_string();
        Ok(FileData::Symlink(target))
    } else if typ.is_file() {
        let mode = md.mode();
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
    Ok(blocklist.total)
}

impl Replica for PosixReplica {
    type Directory = DirHandle;
    type TransferIn = Option<Box<ContentAddressableSource>>;
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
                &*self.dao.lock().unwrap(), true, &*self.config));
            dir.toggle_file(File(&name, &fd));
            ret.push((name, fd));
        }

        Ok(ret)
    }

    fn rename(&self, dir: &mut DirHandle, old: &OsStr, new: &OsStr)
              -> Result<()> {
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
        let path = dir.child(target.0);
        try!(self.check_matches(&path, target.1));

        if let FileData::Directory(_) = *target.1 {
            try!(fs::remove_dir(&path));
        } else {
            try!(self.hide_file(&path));
        }

        let _ = self.dao.lock().unwrap().delete_cache(&path);
        dir.toggle_file(target);

        Ok(())
    }

    fn create(&self, dir: &mut DirHandle, source: File,
              xfer: Option<Box<ContentAddressableSource>>)
              -> Result<FileData> {
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
              xfer: Option<Box<ContentAddressableSource>>)
              -> Result<FileData> {
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
            try!(self.hide_file(&shunted));
        }

        dir.toggle_file(File(name, old));
        dir.toggle_file(File(name, new));

        Ok(ret)
    }

    fn chdir(&self, dir: &DirHandle, subdir: &OsStr)
             -> Result<DirHandle> {
        let path = dir.child(subdir);
        let md = try!(fs::symlink_metadata(&path));
        if !md.file_type().is_dir() {
            Err(Error::NotADir.into())
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
        dir.toggle_file(File(&path, &FileData::Directory(md.mode())));
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
            let mut path: PathBuf = self.config.root.clone().into();
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
        // symlink_metadata() consistently fails for some other reason.
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
        xfer: Option<Box<ContentAddressableSource>>,
        before_establish: F)
        -> Result<FileData>
    {
        match *source.1 {
            FileData::Special =>
                panic!("Attempted to create generic special file locally"),

            FileData::Directory(mode) => {
                try!(before_establish());
                try!(fs::DirBuilder::new()
                     .mode(mode)
                     .create(dir.child(source.0)));
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
                    try!(self.scratch_regular(mode));
                if let Some(xfer) = xfer {
                    // Copy the file to the local filesystem
                    try!(self.xfer_file(&mut scratch_file, &*xfer));
                    // Move anything out of the way as needed
                    try!(before_establish());
                    let new_path = dir.child(source.0);
                    // Atomically put into place
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
        if let Ok(Some(srcname)) =
            self.dao.lock().unwrap().find_file_with_hash(hash)
        {
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
        if let Ok(Some((path, off, len))) = self.dao.lock().unwrap()
            .find_block_with_hash(hash)
        {
            // Looks like we know about one. Try to read the block in. Quietly
            // drop errors; if anything fails, the hash of `data` will not
            // match `hash`.
            let mut data: Vec<u8> = Vec::new();
            let _ = fs::File::open(&path).and_then(|mut src| {
                try!(src.seek(io::SeekFrom::Start(off as u64)));
                try!(src.take(len as u64).read_to_end(&mut data));
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
