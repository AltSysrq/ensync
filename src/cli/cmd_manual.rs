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

// Lots of stuff in this module won't work on Windows as it is right now.

use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{self, Read, Seek, Write};
use std::path::Path;
use std::os::unix::fs::*;

use tempfile::NamedTempFile;

use block_xfer;
use defs::*;
use errors::*;
use replica::{Replica, ReplicaDirectory};
use posix;
use server::{ServerReplica, Storage};

pub fn ls<'a, S : Storage + ?Sized, IT : Iterator<Item = &'a OsStr>>
    (replica: &ServerReplica<S>, paths: IT,
     show_headers: bool, human_readable: bool)
    -> Result<()>
{
    for path in paths {
        if show_headers {
            println!("{}:", path.to_string_lossy());
        }
        ls_one(replica, path, human_readable)?;
        if show_headers {
            println!("");
        }
    }
    Ok(())
}

fn ls_one<S : Storage + ?Sized, P : AsRef<Path>>
    (replica: &ServerReplica<S>, path: P, human_readable: bool)
     -> Result<()>
{
    let path = path.as_ref();
    let (mut dir, single) = navigate(replica, path, true)?;
    let mut list = replica.list(&mut dir)
        .chain_err(|| format!("Failed to list '{}'",
                              dir.full_path().to_string_lossy()))?;
    list.sort_by(|a, b| a.0.cmp(&b.0));

    let mut found = false;

    for (name, fd) in list {
        if single.as_ref().map_or(false, |s| *s != name) { continue; }
        found = true;

        let (typ, mode, mut size, time, target) = match fd {
            FileData::Regular(mode, size, time, _) =>
                ('-', mode, size, time, None),

            FileData::Directory(mode) =>
                ('d', mode, 0, 0, None),

            FileData::Symlink(target) => {
                let target = target.to_string_lossy().into_owned();
                ('l', 0o7777, target.len() as u64, 0, Some(target))
            },

            FileData::Special =>
                ('c', 0, 0, 0, None),
        };

        let time = if time > 0 {
            super::format_date::format_timestamp(time)
        } else {
            super::format_date::EMPTY.to_owned()
        };

        fn bit(mode: FileMode, bit: FileMode, ch: char) -> char {
            if bit == (mode & bit) {
                ch
            } else {
                '-'
            }
        }
        fn twobit(mode: FileMode, bit1: FileMode, bit2: FileMode,
                  ch1: char, ch2: char, ch3: char) -> char {
            match (0 != (mode & bit1), 0 != (mode & bit2)) {
                (false, false) => '-',
                (true, false) => ch1,
                (false, true) => ch2,
                (true, true) => ch3,
            }
        }

        let size_str = if human_readable {
            let suffixes = ["B", "k", "M", "G", "T", "P", "E", "Z"];
            let mut index = 0;
            while size >= 10000 {
                index += 1;
                size /= 1024;
            }

            format!("{:>4}{}", size, suffixes[index])
        } else {
            format!("{:>12}", size)
        };

        println!("{}{}{}{}{}{}{}{}{}{}  {}  {}  {}{}{}",
                 typ,
                 bit(mode, 0o0400, 'r'),
                 bit(mode, 0o0200, 'w'),
                 twobit(mode, 0o0100, 0o4000, 'x', 'S', 's'),
                 bit(mode, 0o0040, 'r'),
                 bit(mode, 0o0020, 'w'),
                 twobit(mode, 0o0010, 0o2000, 'x', 'S', 's'),
                 bit(mode, 0o0004, 'r'),
                 bit(mode, 0o0002, 'w'),
                 twobit(mode, 0o0001, 0o1000, 'x', 'T', 't'),
                 size_str, time, name.to_string_lossy(),
                 if target.is_some() { " -> " } else { "" },
                 target.as_ref().map_or("", |s| &*s));
    }

    if !found && single.is_some() {
        return Err(format!("{}: File not found", path.display()).into());
    }

    Ok(())
}

pub fn mkdir<'a, S : Storage + ?Sized, IT : Iterator<Item = &'a OsStr>>
    (replica: &ServerReplica<S>, paths: IT, mode: FileMode) -> Result<()>
{
    for path in paths {
        let path: &Path = path.as_ref();
        let (mut dir, _) = navigate(
            replica, path.parent().unwrap_or("".as_ref()), false)?;

        let filename = path.file_name().ok_or_else(
            || format!("'{}' does not name a file", path.display()))?;

        replica.create(&mut dir, File(filename, &FileData::Directory(mode)), None)
            .chain_err(|| format!("Failed to create '{}'", path.display()))?;
    }
    Ok(())
}

pub fn rmdir<'a, S : Storage + ?Sized, IT : Iterator<Item = &'a OsStr>>
    (replica: &ServerReplica<S>, paths: IT) -> Result<()>
{
    for path in paths {
        let path: &Path = path.as_ref();
        let (mut dir, _) = navigate(replica, path, false)?;
        replica.rmdir(&mut dir)
            .chain_err(|| format!("Failed to remove '{}'", path.display()))?;
    }

    Ok(())
}

pub fn cat<'a, S : Storage + ?Sized, IT : Iterator<Item = &'a OsStr>>
    (replica: &ServerReplica<S>, paths: IT) -> Result<()>
{
    for path in paths {
        let path: &Path = path.as_ref();
        let parent = path.parent().unwrap_or("".as_ref());

        let (mut dir, _) = navigate(replica, parent, false)?;

        let filename = path.file_name().ok_or_else(
            || format!("'{}' does not name a file", path.display()))?;

        let fd = replica.list(&mut dir)
            .chain_err(|| format!("Error listing '{}'", parent.display()))?
            .into_iter()
            .filter(|&(ref name, _)| name == filename)
            .map(|(_, fd)| fd)
            .next()
            .ok_or_else(|| format!("'{}' does not exist", path.display()))?;

        let xfer = replica.transfer(&dir, File(filename, &fd))
            .chain_err(|| format!("Error fetching '{}'", path.display()))?
            .ok_or_else(|| format!("'{}' is not a regular file",
                                   path.display()))?;
        let stdout_handle = io::stdout();
        block_xfer::blocks_to_stream(
            &xfer.blocks,
            stdout_handle.lock(),
            replica.key_chain().obj_hmac_secret().chain_err(
                || format!("Error transferring '{}'", path.display()))?,
            |id| xfer.fetch.fetch(id))
            .chain_err(|| format!("Error transferring '{}'", path.display()))?;
    }

    Ok(())
}

pub fn get<S : Storage + ?Sized, P1 : AsRef<Path>, P2 : AsRef<Path>>
    (replica: &ServerReplica<S>, src: P1, dst: P2,
     allow_overwrite: bool, verbose: bool) -> Result<()>
{
    let src = src.as_ref();
    let mut dst = dst.as_ref().to_owned();

    let parent = src.parent().unwrap_or("".as_ref());
    let name = src.file_name();
    let (dir, _) = navigate(replica, parent, false)?;

    // If `dst` is a directory, copy to a child of that directory with the same
    // name as `src` unless `src` has no name, in which case simply use `dst`.
    if dst.is_dir() {
        if let Some(name) = name {
            dst.push(name);
        }
    }

    get_impl(replica, dir, name.map(|r| r.to_owned()),
             &dst, allow_overwrite, verbose)?;

    fn get_impl<S : Storage + ?Sized>
        (replica: &ServerReplica<S>,
         mut dir: <ServerReplica<S> as Replica>::Directory,
         single: Option<OsString>,
         dst: &Path,
         allow_overwrite: bool,
         verbose: bool) -> Result<()>
    {
        let list = replica.list(&mut dir)
            .chain_err(|| format!("Failed to list contents of '{}'",
                                  dir.full_path().to_string_lossy()))?;
        let mut found = false;

        for (name, fd) in list {
            if single.as_ref().map_or(false, |n| name != *n) { continue; }
            found = true;

            let sub_src = (dir.full_path().as_ref() as &Path).join(&name);
            let (sub_dst, tmpdir) = if single.is_some() {
                (dst.to_owned(), dst.parent().unwrap_or(".".as_ref()))
            } else {
                (dst.join(&name), dst)
            };

            if verbose {
                println!("{} => {}", sub_src.display(), sub_dst.display());
            }

            match fd {
                FileData::Regular(mode, _, time, _) => {
                    let mut tmpfile = NamedTempFile::new_in(tmpdir)
                        .chain_err(|| format!("Failed to create \
                                               temporary file"))?;

                    fs::set_permissions(tmpfile.path(),
                                        fs::Permissions::from_mode(mode))
                        .chain_err(|| format!("Error setting new file \
                                               permissions on '{}'",
                                              tmpfile.path().display()))?;

                    let xfer = replica.transfer(&dir, File(&name, &fd))
                        .chain_err(|| format!("Failed to start transfer \
                                               of '{}'", sub_src.display()))?
                        .ok_or(ErrorKind::MissingXfer)?;
                    block_xfer::blocks_to_stream(
                        &xfer.blocks,
                        &mut tmpfile,
                        replica.key_chain().obj_hmac_secret().chain_err(
                            || format!("Failed to start transfer of '{}'",
                                       sub_src.display()))?,
                        |id| xfer.fetch.fetch(id))
                        .chain_err(|| format!("Error transferring '{}'",
                                              sub_src.display()))?;

                    posix::set_mtime(&tmpfile, time)
                        .chain_err(|| format!("Failed to set mtime on '{}'",
                                              sub_dst.display()))?;

                    if allow_overwrite {
                        tmpfile.persist(&sub_dst)
                    } else {
                        tmpfile.persist_noclobber(&sub_dst)
                    }.chain_err(|| format!("Error persisting '{}'",
                                           sub_dst.display()))?;
                },

                FileData::Directory(mode) => {
                    match fs::DirBuilder::new().mode(mode).create(&sub_dst) {
                        Ok(_) => { },
                        Err(ref e) if
                            io::ErrorKind::AlreadyExists == e.kind() &&
                            sub_dst.is_dir() => { },
                        Err(e) => Err(e)
                            .chain_err(|| format!(
                                "Failed to create directory '{}'",
                                sub_dst.display()))?,
                    }

                    get_impl(replica,
                             replica.chdir(&dir, &name)
                             .chain_err(|| format!(
                                 "Failed to enter directory '{}'",
                                 sub_src.display()))?,
                             None, &sub_dst, allow_overwrite, verbose)?;
                },

                FileData::Symlink(target) => {
                    if !allow_overwrite && sub_dst.exists() {
                        return Err(format!(
                            "Not overwriting '{}': Already exists",
                            sub_dst.display()).into());
                    }

                    symlink(target, &sub_dst)
                        .chain_err(|| format!(
                            "Failed to create symlink '{}'",
                            sub_dst.display()))?;
                },

                FileData::Special => {
                    let _ = writeln!(
                        io::stderr(), "Ignoring special file '{}'",
                        sub_dst.display());
                },
            }
        }

        if !found && single.is_some() {
            return Err(format!("{}/{}: File not found",
                               dir.full_path().to_string_lossy(),
                               single.unwrap().to_string_lossy()).into());
        }

        Ok(())
    }

    Ok(())
}

pub fn put<S : Storage + ?Sized, P1 : AsRef<Path>, P2 : AsRef<Path>>
    (replica: &ServerReplica<S>, src: P1, dst: P2,
     allow_overwrite: bool, verbose: bool) -> Result<()>
{
    let src = src.as_ref();
    let dst = dst.as_ref();

    let (mut dir, single) = navigate(replica, dst, true)?;

    let name = single.or_else(|| src.file_name().map(|s| s.to_owned()))
        .ok_or_else(|| format!("'{}' does not name a file", src.display()))?;

    let existing = map_of(replica, &mut dir)?;
    put_file(replica, src, &mut dir, name, &existing,
             allow_overwrite, verbose)?;

    fn put_file<S : Storage + ?Sized>
        (replica: &ServerReplica<S>, src: &Path,
         dir: &mut <ServerReplica<S> as Replica>::Directory,
         name: OsString,
         existing: &HashMap<OsString, FileData>,
         allow_overwrite: bool, verbose: bool) -> Result<()>
    {
        let dst = (dir.full_path().as_ref() as &Path).join(&name);

        if verbose {
            println!("{} => {}", src.display(), dst.display());
        }

        let existing = existing.get(&name);
        if let Some(existing) = existing {
            if !existing.is_dir() && !allow_overwrite {
                return Err(format!("Not overwriting '{}': Already exists",
                                   dst.display()).into());
            }
        }

        let md = fs::symlink_metadata(src)
            .chain_err(|| format!("Failed to get metadata of '{}'",
                                  src.display()))?;
        let ft = md.file_type();
        if ft.is_dir() {
            let new_fd = FileData::Directory(md.mode());
            if let Some(existing) = existing {
                replica.update(dir, &name, existing, &new_fd, None)
            } else {
                replica.create(dir, File(&name, &new_fd), None)
            }.chain_err(|| format!("Failed to create/update '{}'",
                                   dst.display()))?;

            let mut subdir = replica.chdir(&dir, &name)
                .chain_err(|| format!("Failed to enter '{}'", dst.display()))?;
            let sub_existing = map_of(replica, &mut subdir)?;

            let dirit = fs::read_dir(&src)
                .chain_err(|| format!("Failed to list contents of '{}'",
                                      src.display()))?;

            for entry in dirit {
                let entry = entry.chain_err(
                    || format!("Error listing '{}'", src.display()))?;

                put_file(replica, &entry.path(), &mut subdir,
                         entry.file_name(), &sub_existing,
                         allow_overwrite, verbose)?;
            }
        } else if ft.is_file() {
            let new_fd = FileData::Regular(
                md.mode(), md.size(), md.mtime(), UNKNOWN_HASH);
            struct Xfer(fs::File);
            impl Read for Xfer {
                fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
                    self.0.read(buf)
                }
            }
            impl block_xfer::StreamSource for Xfer {
                fn reset(&mut self) -> Result<()> {
                    self.0.seek(io::SeekFrom::Start(0))?;
                    Ok(())
                }

                fn finish(&mut self, _: &block_xfer::BlockList) -> Result<()> {
                    Ok(())
                }
            }

            let xfer: Option<Box<block_xfer::StreamSource>> =
                Some(Box::new(Xfer(fs::File::open(src).chain_err(
                    || format!("Failed to open '{}' for reading",
                               src.display()))?)));

            if let Some(existing) = existing {
                replica.update(dir, &name, existing, &new_fd, xfer)
            } else {
                replica.create(dir, File(&name, &new_fd), xfer)
            }.chain_err(|| format!("Failed to create/update '{}'",
                                   dst.display()))?;
        } else if ft.is_symlink() {
            let target = fs::read_link(src)
                .chain_err(|| format!("Failed to read target of '{}'",
                                      src.display()))?;
            let new_fd = FileData::Symlink(target.into_os_string());

            if let Some(existing) = existing {
                replica.update(dir, &name, existing, &new_fd, None)
            } else {
                replica.create(dir, File(&name, &new_fd), None)
            }.chain_err(|| format!("Failed to create/update '{}'",
                                   dst.display()))?;
        } else {
            let _ = writeln!(io::stderr(), "Ignoring special file '{}'",
                             src.display());
        }

        Ok(())
    }

    fn map_of<S : Storage + ?Sized>
        (replica: &ServerReplica<S>,
         dir: &mut <ServerReplica<S> as Replica>::Directory)
        -> Result<HashMap<OsString, FileData>>
    {
        let mut ret = HashMap::new();
        let list = replica.list(dir).chain_err(
            || format!("Failed to list contents of '{}'",
                       dir.full_path().to_string_lossy()))?;
        for (name, value) in list {
            ret.insert(name, value);
        }
        Ok(ret)
    }

    Ok(())
}

pub fn rm<'a, S : Storage + ?Sized, IT : Iterator<Item = &'a OsStr>>(
    replica: &ServerReplica<S>, paths: IT,
    recursive: bool, verbose: bool) -> Result<()>
{
    for path in paths {
        rm_one(replica, path, recursive, verbose)?;
    }
    Ok(())
}

fn rm_one<S : Storage + ?Sized, P : AsRef<Path>>(
    replica: &ServerReplica<S>, path: P,
    recursive: bool, verbose: bool) -> Result<()>
{
    let path = path.as_ref();

    if path == Path::new("") {
        return Err("If you really want to remove the logical root \
                    directory, pass the fully-qualified path instead.".into());
    }
    if path == Path::new("/") {
        return Err("Cannot remove the physical root. If you really \
                    want to remove all the data under it, list each \
                    top-level item separately.".into());
    }

    let (mut dir, single) = navigate(replica, path, true)?;

    fn remove_by_name<S : Storage + ?Sized>
        (replica: &ServerReplica<S>,
         dir: &mut <ServerReplica<S> as Replica>::Directory,
         name: OsString, verbose: bool) -> Result<()>
    {
        if verbose {
            println!("{}/{}", dir.full_path().to_string_lossy(),
                     name.to_string_lossy());
        }

        let fd = replica.list(dir)
            .chain_err(|| format!("Failed to list contents of '{}'",
                                  dir.full_path().to_string_lossy()))?
            .into_iter()
            .filter(|&(ref n, _)| *n == name)
            .map(|(_, fd)| fd)
            .next()
            .ok_or_else(
                || format!("{}/{}: File not found",
                           dir.full_path().to_string_lossy(),
                           name.to_string_lossy()))?;

        replica.remove(dir, File(&name, &fd))
            .chain_err(|| format!("Failed to remove '{}/{}'",
                                  dir.full_path().to_string_lossy(),
                                  name.to_string_lossy()))
    }

    fn remove_recursively<S : Storage + ?Sized>
        (replica: &ServerReplica<S>,
         mut dir: <ServerReplica<S> as Replica>::Directory,
         verbose: bool) -> Result<()>
    {
        let list = replica.list(&mut dir).chain_err(
            || format!("Failed to list contents of '{}'",
                       dir.full_path().to_string_lossy()))?;
        for (name, fd) in list {
            if fd.is_dir() {
                let subdir = replica.chdir(&dir, &name).chain_err(
                    || format!("Failed to enter '{}/{}'",
                               dir.full_path().to_string_lossy(),
                               name.to_string_lossy()))?;
                remove_recursively(replica, subdir, verbose)?;
            } else {
                if verbose {
                    println!("{}/{}", dir.full_path().to_string_lossy(),
                             name.to_string_lossy());
                }
                replica.remove(&mut dir, File(&name, &fd)).chain_err(
                    || format!("Failed to remove '{}/{}'",
                               dir.full_path().to_string_lossy(),
                               name.to_string_lossy()))?;
            }
        }

        if verbose {
            println!("{}", dir.full_path().to_string_lossy());
        }
        replica.rmdir(&mut dir).chain_err(
            || format!("Failed to remove '{}'",
                       dir.full_path().to_string_lossy()))
    }

    if let Some(single) = single {
        remove_by_name(replica, &mut dir, single, verbose)
    } else if !recursive {
        Err("Not removing directory without `--recursive` flag.".into())
    } else {
        remove_recursively(replica, dir, verbose)
    }
}

fn navigate<S : Storage + ?Sized, P : AsRef<Path>>(
    replica: &ServerReplica<S>, path: P, allow_tail: bool)
    -> Result<(<ServerReplica<S> as Replica>::Directory,
               Option<OsString>)>
{
    let path = path.as_ref();

    let mut dir = if path.is_absolute() {
        replica.pseudo_root()
    } else {
        replica.root().chain_err(
            || "Failed to open logical server root")?
    };

    let mut it = path.iter();
    while let Some(component) = it.next() {
        if component.is_empty() || OsStr::new("/") == component { continue; }

        match replica.chdir(&dir, component) {
            Ok(subdir) => dir = subdir,
            Err(Error(ErrorKind::NotADirectory, _)) |
            Err(Error(ErrorKind::NotFound, _)) if allow_tail => {
                if it.next().is_some() {
                    return Err(
                        format!("Path '{}/{}' is not a directory",
                                dir.full_path().to_string_lossy(),
                                component.to_string_lossy()).into());
                } else {
                    return Ok((dir, Some(component.to_owned())));
                }
            },
            Err(e) => return Err(e).chain_err(
                || format!("Failed to enter '{}/{}'",
                           dir.full_path().to_string_lossy(),
                           component.to_string_lossy())),
        }
    }

    Ok((dir, None))
}
