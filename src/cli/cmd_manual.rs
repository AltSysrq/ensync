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

use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{self, Read, Write};
use std::path::Path;

use defs::*;
use errors::*;
use replica::{Replica, ReplicaDirectory};
use server::{ServerReplica, Storage};

pub fn ls<'a, S : Storage + ?Sized, IT : Iterator<Item = &'a str>>
    (replica: &ServerReplica<S>, paths: IT,
     show_headers: bool, human_readable: bool)
    -> Result<()>
{
    for path in paths {
        if show_headers {
            println!("{}:", path);
        }
        ls_one(replica, path, human_readable)?;
        if show_headers {
            println!();
        }
    }
    Ok(())
}

fn ls_one<S : Storage + ?Sized, P : AsRef<Path>>
    (replica: &ServerReplica<S>, path: P, human_readable: bool)
     -> Result<()>
{
    let (mut dir, single) = navigate(replica, path, true)?;
    let list = replica.list(&mut dir)
        .chain_err(|| format!("Failed to list '{}'",
                              dir.full_path().to_string_lossy()))?;
    for (name, fd) in list {
        if single.as_ref().map_or(false, |s| *s != name) { continue; }

        let (typ, mode, mut size, time, target) = match fd {
            FileData::Regular(mode, size, time, _) =>
                ('-', mode, size, time, None),

            FileData::Directory(mode) =>
                ('d', mode, 0, 0, None),

            FileData::Symlink(target) => {
                let target = target.to_string_lossy().into_owned();
                ('l', 0o6777, target.len() as u64, 0, Some(target))
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
            format!("{:>16}", size)
        };

        println!("{}{}{}{}{}{}{}{}{}{} {} {} {}{}{}",
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

    Ok(())
}

pub fn mkdir<'a, S : Storage + ?Sized, IT : Iterator<Item = &'a str>>
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

pub fn rmdir<'a, S : Storage + ?Sized, IT : Iterator<Item = &'a str>>
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
            Err(Error(ErrorKind::NotADirectory, _)) if allow_tail => {
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
