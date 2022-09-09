//-
// Copyright (c) 2017, 2018, 2021, Jason Lingle
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
use std::cmp::{max, min};
use std::collections::HashSet;
use std::ffi::OsStr;
use std::fmt;
use std::fs;
use std::io::{self, stderr, stdout, BufWriter, Read, Write};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering::SeqCst;
use std::sync::{Arc, Mutex, RwLock};
use std::thread;
use std::time::Duration;

use chrono::Utc;
use libc::isatty;
use serde::Serialize;

use crate::ancestor::*;
use crate::cli::config::Config;
use crate::cli::format_date;
use crate::cli::open_server::open_server_replica;
use crate::defs::*;
use crate::dry_run_replica::DryRunReplica;
use crate::errors::*;
use crate::interrupt;
use crate::log::*;
use crate::posix::*;
use crate::reconcile;
use crate::reconcile::compute::*;
use crate::replica::{
    Condemn, NullTransfer, PrepareType, Replica, Watch, WatchHandle,
};
use crate::rules;
use crate::server::*;
use crate::work_stack;

macro_rules! perrln {
    ($($arg:expr),+) => {
        let _ = writeln!(stderr(), $($arg),+);
    }
}
macro_rules! perr {
    ($($arg:expr),+) => {
        let _ = write!(stderr(), $($arg),+);
    }
}

trait AsPath {
    fn as_path(&self) -> &Path;
}

impl AsPath for OsStr {
    fn as_path(&self) -> &Path {
        self.as_ref()
    }
}

struct PathDisplay<'a, T>(&'a Path, T);
impl<'a> fmt::Display for PathDisplay<'a, &'a OsStr> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "{}",
            self.1.as_path().strip_prefix(self.0).unwrap().display()
        )
    }
}

impl<'a> fmt::Display for PathDisplay<'a, (&'a OsStr, &'a OsStr)> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let prefix = (self.1).0.as_path().strip_prefix(self.0).unwrap();
        if prefix.components().next().is_some() {
            write!(f, "{}/{}", prefix.display(), (self.1).1.as_path().display())
        } else {
            write!(f, "{}", (self.1).1.as_path().display())
        }
    }
}

impl<'a> fmt::Display for PathDisplay<'a, (&'a OsStr, Option<&'a OsStr>)> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        if let Some(name) = (self.1).1 {
            PathDisplay(self.0, ((self.1).0, name)).fmt(f)
        } else {
            PathDisplay(self.0, (self.1).0).fmt(f)
        }
    }
}

#[derive(Debug)]
struct LoggerImpl {
    client_root: PathBuf,
    verbose_level: LogLevel,
    include_ops_under_opped_directory: bool,
    itemise_level: LogLevel,
    include_ancestors: bool,
    colour: bool,
    created_directories: RwLock<HashSet<PathBuf>>,
    recdel_directories: RwLock<HashSet<PathBuf>>,
    spin: Option<Mutex<SpinState>>,
    json_status_out: Option<Mutex<BufWriter<fs::File>>>,
}

#[derive(Debug, Default)]
struct SpinState {
    cycle: u8,
    cli: ReplicaSpinState,
    srv: ReplicaSpinState,
}

#[derive(Debug, Default)]
struct ReplicaSpinState {
    created: u64,
    updated: u64,
    deleted: u64,
    transfer: u64,
}

#[derive(Serialize)]
struct LogJson<'a> {
    status_version: usize,
    time: chrono::DateTime<Utc>,
    #[serde(flatten)]
    contents: LogJsonContents<'a>,
    #[serde(flatten)]
    location: Option<LogJsonLocation<'a>>,
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum LogJsonContents<'a> {
    SyncStarted,
    SyncFinished,
    SyncConflict {
        #[serde(rename = "conflict_type")]
        type_: &'a str,
        #[serde(rename = "conflict_reconciliation")]
        reconciliation: &'a str,
    },
    SyncCreate,
    SyncUpdate {
        #[serde(rename = "update_old_info")]
        old_info: LogJsonPathInfo,
        #[serde(rename = "update_new_info")]
        new_info: LogJsonPathInfo,
    },
    SyncRename {
        #[serde(rename = "rename_new_name")]
        new_name: String,
    },
    SyncRemove,
    SyncRemoveRecursively,
    SyncRemoveDirectory,
    SyncError {
        error_message: String,
    },
}

#[derive(Serialize)]
struct LogJsonLocation<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    side: Option<&'a str>,
    path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    info: Option<LogJsonPathInfo>,
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum LogJsonPathInfo {
    Directory {
        mode: u32,
    },
    File {
        mode: u32,
        size: u64,
        modified_time: chrono::DateTime<Utc>,
    },
    Symlink {
        target: String,
    },
    Special,
}

impl Logger for LoggerImpl {
    fn log(&self, level: LogLevel, what: &Log) {
        if level <= self.verbose_level {
            self.write_human_readable(level, what);
        }
        if level <= self.itemise_level {
            self.write_itemised(what);
        }

        if let Some(json_status_out_mutex) = &self.json_status_out {
            let mut json_status_out = json_status_out_mutex.lock().unwrap();

            self.write_json_status(&mut json_status_out, what);
        }
    }
}

/// Gets the basename of the path of the given `ErrorOperation`, if any.
fn get_error_basename<'a>(op: &'a ErrorOperation) -> Option<&'a OsStr> {
    match *op {
        ErrorOperation::List => None,
        ErrorOperation::MarkClean => None,
        ErrorOperation::Chdir(name) => Some(name),
        ErrorOperation::Create(name) => Some(name),
        ErrorOperation::Update(name) => Some(name),
        ErrorOperation::Rename(name) => Some(name),
        ErrorOperation::Remove(name) => Some(name),
        ErrorOperation::Rmdir => None,
        ErrorOperation::Access(name) => Some(name),
    }
}

/// Format the given `ErrorOperation` into a human-readable error message.
fn format_error_log<'a>(op: &'a ErrorOperation, err: &'a Error) -> String {
    let mut errs = err.to_string();

    for e in err.iter().skip(1) {
        errs.push_str(&format!("\ncaused by: {}", e));
    }

    if let Some(bt) = err.backtrace() {
        errs.push_str(&format!("\n{:?}\n", bt));
    }

    match *op {
        ErrorOperation::List => {
            format!("Failed to list directory: {}", errs)
        }

        ErrorOperation::MarkClean => {
            format!("Failed to mark directory clean: {}", errs)
        }

        ErrorOperation::Chdir(..) => {
            format!("Failed to enter directory: {}", errs)
        }

        ErrorOperation::Create(..) => {
            format!("Failed to create: {}", errs)
        }

        ErrorOperation::Update(..) => {
            format!("Failed to update: {}", errs)
        }

        ErrorOperation::Rename(..) => {
            format!("Failed to rename: {}", errs)
        }

        ErrorOperation::Remove(..) => {
            format!("Failed to remove: {}", errs)
        }

        ErrorOperation::Rmdir => {
            format!("Failed to remove: {}", errs)
        }

        ErrorOperation::Access(..) => {
            format!("Failed to access: {}", errs)
        }
    }
}

impl LoggerImpl {
    fn write_human_readable(&self, level: LogLevel, what: &Log) {
        let stderr_handle = stderr();
        let mut stderr_lock = stderr_handle.lock();

        macro_rules! perrln {
            ($($arg:expr),+) => {
                let _ = writeln!(stderr_lock, $($arg),+);
            }
        }
        macro_rules! perr {
            ($($arg:expr),+) => {
                let _ = write!(stderr_lock, $($arg),+);
            }
        }

        fn name_side<S: Into<ReplicaSide>>(side: S) -> &'static str {
            match side.into() {
                ReplicaSide::Client => "local",
                ReplicaSide::Server => "remote",
                ReplicaSide::Ancestor => "ancestor",
            }
        }

        fn name_edit(e: ConflictingEdit) -> &'static str {
            match e {
                ConflictingEdit::Mode => "file mode",
                ConflictingEdit::Content => "content",
            }
        }

        fn pretty_size(mut size: u64) -> String {
            let suffixes =
                ["bytes", "kB", "MB", "GB", "TB", "PB", "EB", "ZB", "YB"];
            let mut suffix_ix = 0usize;
            while size > 10000 {
                size /= 1024;
                suffix_ix += 1;
            }

            format!("{} {}", size, suffixes[suffix_ix])
        }

        struct FDD<'a>(&'a FileData);
        impl<'a> fmt::Display for FDD<'a> {
            fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
                match *self.0 {
                    FileData::Directory(mode) => {
                        write!(f, "directory (mode {:04o})", mode)
                    }
                    FileData::Regular(mode, size, time, _) => {
                        write!(
                            f,
                            "regular file (mode {:04o}, size {}, \
                                   modified {})",
                            mode,
                            pretty_size(size),
                            format_date::format_timestamp(time)
                        )
                    }
                    FileData::Symlink(ref target) => {
                        write!(f, "symlink to {}", target.as_path().display())
                    }
                    FileData::Special => write!(f, "special file"),
                }
            }
        }

        fn spin_size(fd: &FileData) -> u64 {
            match *fd {
                FileData::Regular(_, size, _, _) => size,
                _ => 0,
            }
        }

        let mut reprint_spin = false;

        macro_rules! say {
            ($path:expr, $side:expr, $extra:tt $(, $arg:expr)*) => {{
                let (start_colour, level_name) = match level {
                    FATAL => ("\x1B[1;31m", "FATAL"),
                    ERROR => ("\x1B[31m", "ERROR"),
                    WARN  => ("\x1B[33m", "WARN "),
                    EDIT  => ("\x1B[36m", "EDIT "),
                    INFO  => ("", "INFO "),
                    _ => panic!("Unexpected level {}", level),
                };

                let side = ReplicaSide::from($side);
                let side_name = match side {
                    ReplicaSide::Client =>   "local ",
                    ReplicaSide::Server =>   "remote",
                    ReplicaSide::Ancestor => "ancest",
                };

                // Only show errors in the ancestor store unless the user wants
                // to know everything going on there.
                let not_ancestor_or_allowed =
                    ReplicaSide::Ancestor != side || self.include_ancestors ||
                    level <= ERROR;
                // Interrupting the process with ^C will generally kill the
                // server process too, so don't log fatal errors from the
                // server after interruption.
                let not_interrupted_server_death =
                    level != FATAL || ReplicaSide::Server != side ||
                    !interrupt::is_interrupted();

                if not_ancestor_or_allowed && not_interrupted_server_death {
                    perrln!(concat!("\x1B[K[{}{}{}] {} {}{}{}: ", $extra),
                            if self.colour { start_colour } else { "" },
                            level_name,
                            if self.colour { "\x1B[0m" } else { "" },
                            side_name,
                            if self.colour { "\x1B[1m" } else { "" },
                            PathDisplay(&self.client_root, $path),
                            if self.colour { "\x1B[0m" } else { "" }
                            $(, $arg)*);
                    reprint_spin = true;
                }
            }}
        }
        macro_rules! update_spin {
            (|$spin:ident| $body:block) => {
                if let Some(ref spin) = self.spin {
                    let mut $spin = spin.lock().unwrap();
                    $body;
                    reprint_spin = true;
                }
            };
        }

        match *what {
            Log::SyncStarted | Log::SyncFinished => {}
            Log::Inspect(dir, name, reconciliation, conflict) => {
                let recon_str = match reconciliation {
                    Reconciliation::InSync => Cow::Borrowed("in sync"),
                    Reconciliation::Unsync => {
                        Cow::Borrowed("out of sync, not changing")
                    }
                    Reconciliation::Irreconcilable => {
                        Cow::Borrowed("irreconcilable")
                    }
                    Reconciliation::Use(side) => {
                        Cow::Owned(format!("using {} version", name_side(side)))
                    }
                    Reconciliation::Split(side, _) => Cow::Owned(format!(
                        "renaming file on {}",
                        name_side(side)
                    )),
                };
                let conflict_str = match conflict {
                    Conflict::NoConflict => Cow::Borrowed(""),
                    Conflict::EditDelete(deleted_side) => Cow::Owned(format!(
                        "\n        (conflict: deleted on {} side, \
                                 changed on {} side)",
                        name_side(deleted_side),
                        name_side(deleted_side.rev())
                    )),
                    Conflict::EditEdit(client, server) => Cow::Owned(format!(
                        "\n        (conflict: {} changed locally, \
                                 {} changed remotely)",
                        name_edit(client),
                        name_edit(server)
                    )),
                };

                say!(
                    (dir, name),
                    ReplicaSide::Client,
                    "{}{}",
                    recon_str,
                    conflict_str
                )
            }

            Log::Create(side, path, name, state) => {
                update_spin!(|spin| {
                    match side {
                        ReplicaSide::Client => {
                            spin.cli.created += 1;
                            spin.cli.transfer += spin_size(state);
                        }
                        ReplicaSide::Server => {
                            spin.srv.created += 1;
                            spin.srv.transfer += spin_size(state);
                        }
                        ReplicaSide::Ancestor => {}
                    }
                });

                if let FileData::Directory(..) = *state {
                    self.created_directories
                        .write()
                        .unwrap()
                        .insert(Path::new(path).join(name));
                }

                if self.include_ops_under_opped_directory
                    || !self
                        .created_directories
                        .read()
                        .unwrap()
                        .contains(Path::new(path))
                {
                    say!(
                        (path, name),
                        side,
                        "create\
                                              \n        + {}",
                        FDD(state)
                    )
                }
            }

            Log::Update(side, path, name, old, new) => {
                let content_change = !old.matches_content(new);
                update_spin!(|spin| {
                    match side {
                        ReplicaSide::Client => {
                            spin.cli.updated += 1;
                            if content_change {
                                spin.cli.transfer += spin_size(new);
                            }
                        }
                        ReplicaSide::Server => {
                            spin.srv.updated += 1;
                            if content_change {
                                spin.srv.transfer += spin_size(new);
                            }
                        }
                        ReplicaSide::Ancestor => {}
                    }
                });

                say!(
                    (path, name),
                    side,
                    "update\
                                          \n        - {}\
                                          \n        + {}",
                    FDD(old),
                    FDD(new)
                );
            }

            Log::Rename(side, path, old, new) => say!(
                (path, old),
                side,
                "rename\
                                         \n        -> {}",
                new.as_path().display()
            ),

            Log::Remove(side, path, name, state) => {
                update_spin!(|spin| {
                    match side {
                        ReplicaSide::Client => spin.cli.deleted += 1,
                        ReplicaSide::Server => spin.srv.deleted += 1,
                        ReplicaSide::Ancestor => (),
                    }
                });
                if self.include_ops_under_opped_directory
                    || !self
                        .recdel_directories
                        .read()
                        .unwrap()
                        .contains(Path::new(path))
                {
                    say!(
                        (path, name),
                        side,
                        "delete\
                                              \n        - {}",
                        FDD(state)
                    );
                }
            }

            Log::RecursiveDelete(side, path) => {
                self.recdel_directories
                    .write()
                    .unwrap()
                    .insert(Path::new(path).to_owned());
                if self.include_ops_under_opped_directory
                    || Path::new(path).parent().map_or(true, |p| {
                        !self.recdel_directories.read().unwrap().contains(p)
                    })
                {
                    say!(path, side, "delete recursively");
                }
            }

            Log::Rmdir(side, path) => {
                // Don't update the spinner here, since we don't know whether
                // there is an actual directory being deleted.

                if self.include_ops_under_opped_directory
                    || !self
                        .recdel_directories
                        .read()
                        .unwrap()
                        .contains(Path::new(path))
                {
                    say!(path, side, "remove directory");
                }
            }

            Log::Error(side, path, ref op, err) => {
                let name = get_error_basename(op);
                let message = format_error_log(op, err);

                say!((path, name), side, "{}", message);
            }
        }

        if reprint_spin {
            if let Some(ref spin) = self.spin {
                let mut spin = spin.lock().unwrap();
                spin.cycle = (spin.cycle + 1) % 4;
                perr!(
                    "\x1B[K{} in: +{} *{} -{} {}, out: +{} *{} -{} {}\r",
                    ['-', '\\', '|', '/'][spin.cycle as usize],
                    spin.cli.created,
                    spin.cli.updated,
                    spin.cli.deleted,
                    pretty_size(spin.cli.transfer),
                    spin.srv.created,
                    spin.srv.updated,
                    spin.srv.deleted,
                    pretty_size(spin.srv.transfer)
                );
            }
        }
    }

    fn write_itemised(&self, what: &Log) {
        // Match the format output by rsync as best we can
        // The rsync format is an 11-character string which is either the
        // following sequence of flags, or a '*', and a short message,
        // right-padded.
        //
        // 0. Update type
        //    < Transfer to remote host
        //    > Transfer to local host
        //    c Item being created
        //    h Create hard link (we don't support this)
        //    . No update
        //
        // 1. File type
        //    f Regular
        //    d Directory
        //    L Symlink
        //    D Device (we don't distinguish from special)
        //    S Special
        //
        // 2. 'c' if content change, fill otherwise.
        //
        // 3. 's' file size changed, fill otherwise.
        //
        // 4. 't' file modification time changed, fill otherwise.
        //
        // 5. 'p' file mode changed, fill otherwise.
        //
        // 6. 'o' owner changed. We don't track this, so always fill.
        //
        // 7. 'g' group changed. We don't track this, so always fill.
        //
        // 8. 'f' "fileflags" changed. Again, always fill.
        //
        // 9. 'a' ACL changed. Always fill.
        //
        // 10. 'x' extended attributes changed. Always fill.
        //
        // The fill character is '.' by default. If something is being created,
        // it is instead '+'. If the item is being completely unchanged, it is
        // ' ' instead.
        //
        // We need to extend this a bit. For the most part, this is simply a
        // matter of using more '*'-format things, but renaming is complicated
        // by the fact that there are two filenames in play. We handle this by
        // emitting consecutive `*renamefrom` and `*renameto  ` lines.

        let stdout_handle = stdout();
        let mut stdout_lock = stdout_handle.lock();

        #[derive(Debug, Clone, Copy, Default)]
        struct LineItem {
            update_type: Option<char>,
            file_type: Option<char>,
            content_change: bool,
            size_change: bool,
            time_change: bool,
            mode_change: bool,
            fill: Option<char>,
        }

        impl fmt::Display for LineItem {
            fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
                trait Ifc {
                    fn ifc(self, ifc: char, fill: char) -> char;
                }
                impl Ifc for bool {
                    fn ifc(self, ifc: char, fill: char) -> char {
                        if self {
                            ifc
                        } else {
                            fill
                        }
                    }
                }

                let fill = self.fill.unwrap_or('.');
                write!(
                    f,
                    "{}{}{}{}{}{}{}{}{}{}{}",
                    self.update_type.unwrap_or(fill),
                    self.file_type.unwrap_or(fill),
                    self.content_change.ifc('c', fill),
                    self.size_change.ifc('s', fill),
                    self.time_change.ifc('t', fill),
                    self.mode_change.ifc('p', fill),
                    fill,
                    fill,
                    fill,
                    fill,
                    fill
                )
            }
        }

        macro_rules! say {
            ($item:expr, $path:expr) => {{
                let _ = writeln!(
                    stdout_lock,
                    "{:<11} {}",
                    $item,
                    PathDisplay(&self.client_root, $path)
                );
            }};
        }

        fn file_type(fd: &FileData) -> Option<char> {
            match *fd {
                FileData::Regular(..) => Some('f'),
                FileData::Directory(..) => Some('d'),
                FileData::Symlink(..) => Some('L'),
                FileData::Special => Some('S'),
            }
        }

        fn update_type(side: ReplicaSide, data: &FileData) -> Option<char> {
            match (side, data) {
                (ReplicaSide::Client, &FileData::Regular(..)) => Some('>'),
                (ReplicaSide::Server, &FileData::Regular(..)) => Some('<'),
                _ => Some('c'),
            }
        }

        fn nan(side: ReplicaSide) -> bool {
            ReplicaSide::Ancestor != side
        }

        match *what {
            Log::SyncStarted | Log::SyncFinished => {}
            Log::Error(..) => {}
            Log::RecursiveDelete(..) => {}

            Log::Inspect(parent, name, Reconciliation::InSync, _)
            | Log::Inspect(parent, name, Reconciliation::Unsync, _)
            | Log::Inspect(parent, name, Reconciliation::Irreconcilable, _) => {
                // We can't really output a file type even if the data were
                // included in this log type, since the three replicas could
                // each have a different file type.
                say!(
                    LineItem {
                        fill: Some(' '),
                        update_type: Some('.'),
                        file_type: Some('?'),
                        ..LineItem::default()
                    },
                    (parent, name)
                );
            }

            Log::Inspect(..) => {}

            Log::Create(side, parent, name, data) => {
                if nan(side) {
                    say!(
                        LineItem {
                            fill: Some('+'),
                            update_type: update_type(side, data),
                            file_type: file_type(data),
                            ..LineItem::default()
                        },
                        (parent, name)
                    )
                }
            }

            Log::Update(side, parent, name, old, new) => {
                if nan(side) {
                    let (content_change, size_change, time_change, mode_change) =
                        match (old, new) {
                            (
                                &FileData::Regular(
                                    mode1,
                                    size1,
                                    time1,
                                    content1,
                                ),
                                &FileData::Regular(
                                    mode2,
                                    size2,
                                    time2,
                                    content2,
                                ),
                            ) => (
                                content1 != content2,
                                size1 != size2,
                                time1 != time2,
                                mode1 != mode2,
                            ),

                            (
                                &FileData::Symlink(..),
                                &FileData::Symlink(..),
                            ) => (true, false, false, false),

                            (
                                &FileData::Directory(..),
                                &FileData::Directory(..),
                            ) => (false, false, false, true),

                            _ => (true, false, false, true),
                        };

                    say!(
                        LineItem {
                            update_type: if content_change {
                                update_type(side, new)
                            } else {
                                Some('.')
                            },
                            file_type: file_type(new),
                            content_change: content_change,
                            mode_change: mode_change,
                            time_change: time_change,
                            size_change: size_change,
                            ..LineItem::default()
                        },
                        (parent, name)
                    );
                }
            }

            Log::Rename(side, parent, old, new) => {
                if nan(side) {
                    say!("*renamefrom", (parent, old));
                    say!("*renameto", (parent, new));
                }
            }

            Log::Remove(side, parent, name, _) => {
                if nan(side) {
                    say!("*delete", (parent, name));
                }
            }

            Log::Rmdir(side, path) => {
                if nan(side) {
                    say!("*delete", path);
                }
            }
        }
    }

    fn write_json_status(&self, mut out: &mut BufWriter<fs::File>, what: &Log) {
        let now = chrono::Utc::now();

        let display_path = |dir: &OsStr, name: &OsStr| -> String {
            format!("{}", PathDisplay(&self.client_root, (dir, name)))
        };

        let display_maybe_path =
            |dir: &OsStr, name: Option<&OsStr>| -> String {
                format!("{}", PathDisplay(&self.client_root, (dir, name)))
            };

        let display_dir_path = |dir: &OsStr| -> String {
            format!("{}", PathDisplay(&self.client_root, dir))
        };

        fn name_side<S: Into<ReplicaSide>>(side: S) -> &'static str {
            match side.into() {
                ReplicaSide::Client => "local",
                ReplicaSide::Server => "remote",
                ReplicaSide::Ancestor => "ancestor",
            }
        }

        fn convert_state_to_info(state: &FileData) -> LogJsonPathInfo {
            match *state {
                FileData::Directory(mode) => {
                    LogJsonPathInfo::Directory { mode }
                }

                FileData::Regular(mode, size, modified_time, ..) => {
                    LogJsonPathInfo::File {
                        mode,
                        size,
                        modified_time: chrono::DateTime::<Utc>::from_utc(
                            chrono::NaiveDateTime::from_timestamp(
                                modified_time,
                                0,
                            ),
                            Utc,
                        ),
                    }
                }
                FileData::Symlink(ref target) => LogJsonPathInfo::Symlink {
                    target: target.to_string_lossy().clone().into_owned(),
                },
                FileData::Special => LogJsonPathInfo::Special,
            }
        }

        let contents = match *what {
            Log::SyncStarted => LogJsonContents::SyncStarted,
            Log::SyncFinished => LogJsonContents::SyncFinished,
            Log::Inspect(.., reconciliation, conflict) => {
                LogJsonContents::SyncConflict {
                    type_: match conflict {
                        Conflict::NoConflict => return,
                        Conflict::EditEdit(..) => "edit_edit",
                        Conflict::EditDelete(deleted_side) => {
                            match deleted_side {
                                ReconciliationSide::Client => {
                                    "edit_remote_delete_local"
                                }
                                ReconciliationSide::Server => {
                                    "edit_local_delete_remote"
                                }
                            }
                        }
                    },
                    reconciliation: match reconciliation {
                        Reconciliation::InSync => return,
                        Reconciliation::Unsync => "unsynced",
                        Reconciliation::Irreconcilable => "irreconcilable",
                        Reconciliation::Use(side) => match side {
                            ReconciliationSide::Client => "use_local",
                            ReconciliationSide::Server => "use_remote",
                        },
                        Reconciliation::Split(side, ..) => match side {
                            ReconciliationSide::Client => "split_local",
                            ReconciliationSide::Server => "split_remote",
                        },
                    },
                }
            }

            Log::Create(..) => LogJsonContents::SyncCreate,
            Log::Update(.., old, new) => LogJsonContents::SyncUpdate {
                old_info: convert_state_to_info(old),
                new_info: convert_state_to_info(new),
            },
            Log::Rename(_, dir, _, new) => LogJsonContents::SyncRename {
                new_name: display_path(dir, new),
            },
            Log::Remove(..) => LogJsonContents::SyncRemove,
            Log::RecursiveDelete(..) => LogJsonContents::SyncRemoveRecursively,
            Log::Rmdir(..) => LogJsonContents::SyncRemoveDirectory,
            Log::Error(.., ref op, err) => {
                let error_message = format_error_log(op, err);

                LogJsonContents::SyncError { error_message }
            }
        };

        let location = match *what {
            Log::Inspect(dir, name, ..) => Some(LogJsonLocation {
                side: None,
                path: display_path(dir, name),
                info: None,
            }),
            Log::Create(side, dir, name, state)
            | Log::Remove(side, dir, name, state) => Some(LogJsonLocation {
                side: Some(name_side(side)),
                path: display_path(dir, name),
                info: Some(convert_state_to_info(state)),
            }),
            Log::Update(side, dir, name, ..)
            | Log::Rename(side, dir, name, ..) => Some(LogJsonLocation {
                side: Some(name_side(side)),
                path: display_path(dir, name),
                info: None,
            }),
            Log::Rmdir(side, dir) | Log::RecursiveDelete(side, dir) => {
                Some(LogJsonLocation {
                    side: Some(name_side(side)),
                    path: display_dir_path(dir),
                    info: None,
                })
            }
            Log::Error(side, dir, ref op, ..) => {
                let name = get_error_basename(op);

                Some(LogJsonLocation {
                    side: Some(name_side(side)),
                    path: display_maybe_path(dir, name),
                    info: None,
                })
            }
            _ => None,
        };

        serde_json::to_writer(
            &mut out,
            &LogJson {
                status_version: 1,
                time: now,
                contents,
                location,
            },
        )
        .expect("Failed to write to JSON status output");
        write!(&mut out, "\n").expect("Failed to write to JSON status output");
    }
}

pub fn run(
    config: &Config,
    storage: Arc<dyn Storage>,
    verbosity: i32,
    quietness: i32,
    itemise: bool,
    itemise_unchanged: bool,
    json_status_out: Option<fs::File>,
    colour: &str,
    spin: &str,
    include_ancestors: bool,
    dry_run: bool,
    watch: Option<u64>,
    num_threads: u32,
    prepare_type: &str,
    override_mode: Option<rules::SyncMode>,
    // Since --reconnect can cause multiple runs to occur, allow the
    // caller to remember the derived keychain so we don't need to
    // prompt for the passphrase again.
    key_chain: &mut Option<Arc<KeyChain>>,
) -> Result<()> {
    check_for_copied_private_dir(&config.private_root)?;

    let colour = match colour {
        "never" => false,
        "always" => true,
        "auto" => 1 == unsafe { isatty(2) },
        _ => false,
    };
    let spin = match spin {
        "never" => false,
        "always" => true,
        "auto" => 1 == unsafe { isatty(2) },
        _ => false,
    };
    let prepare_type = match prepare_type {
        "auto" => {
            if override_mode.is_some() {
                PrepareType::Clean
            } else {
                PrepareType::Fast
            }
        }
        "fast" => PrepareType::Fast,
        "clean" => PrepareType::Clean,
        "scrub" => PrepareType::Scrub,
        _ => PrepareType::Fast,
    };

    if key_chain.is_none() {
        let passphrase =
            config.passphrase.read_passphrase("passphrase", false)?;
        *key_chain =
            Some(Arc::new(keymgmt::derive_key_chain(&*storage, &passphrase)?));
    }

    let key_chain = key_chain.as_ref().unwrap().clone();

    let mut server_replica =
        open_server_replica(config, storage, Some(key_chain.clone()))?;

    let client_private_dir = config.private_root.join("client");
    fs::create_dir_all(&client_private_dir).chain_err(|| {
        format!(
            "Failed to create client replica private directory '{}'",
            client_private_dir.display()
        )
    })?;
    let mut client_replica = PosixReplica::new(
        config.client_root.clone(),
        client_private_dir,
        key_chain.obj_hmac_secret()?,
        config.block_size as usize,
    )
    .chain_err(|| "Failed to set up client replica")?;

    let ancestor_replica = AncestorReplica::open(
        config
            .private_root
            .join("ancestor.sqlite")
            .to_str()
            .ok_or_else(|| {
                format!(
                    "Path '{}' is not valid UTF-8",
                    config.private_root.display()
                )
            })?,
    )
    .chain_err(|| "Failed to set up ancestor replica")?;

    // Default to EDIT, but don't show newly created items under a directory
    // which itself is now since that does not convey information.
    let mut nominal_log_level = (EDIT as i32) + verbosity - quietness;

    // Virtual log level between EDIT and INFO in which creations under new
    // directories are also logged.
    let include_ops_under_opped_directory;
    if nominal_log_level > (EDIT as i32) {
        include_ops_under_opped_directory = true;
        nominal_log_level -= 1;
    } else {
        include_ops_under_opped_directory = false;
    }

    let level = max(FATAL as i32, min(255, nominal_log_level)) as LogLevel;

    let log = LoggerImpl {
        client_root: config.client_root.to_owned(),
        verbose_level: level,
        include_ops_under_opped_directory: include_ops_under_opped_directory,
        itemise_level: if !itemise {
            0
        } else if !itemise_unchanged {
            EDIT
        } else {
            INFO
        },
        include_ancestors: include_ancestors,
        colour: colour,
        created_directories: RwLock::new(HashSet::new()),
        recdel_directories: RwLock::new(HashSet::new()),
        spin: if spin {
            Some(Mutex::new(SpinState::default()))
        } else {
            None
        },
        json_status_out: json_status_out.map(|j| Mutex::new(BufWriter::new(j))),
    };

    interrupt::install_signal_handler();

    let rules = match override_mode {
        None => config.sync_rules.clone(),
        Some(overide) => {
            Arc::new(rules::engine::SyncRules::single_mode(overide, true))
        }
    };

    if dry_run {
        let context = Arc::new(reconcile::Context {
            cli: DryRunReplica(client_replica),
            anc: DryRunReplica(ancestor_replica),
            srv: DryRunReplica(server_replica),
            log: Box::new(log),
            root_rules: rules::engine::FileEngine::new(rules),
            work: work_stack::WorkStack::new(),
            tasks: reconcile::UnqueuedTasks::new(),
        });

        run_sync(
            context,
            level,
            num_threads,
            prepare_type,
            config,
            true,
            spin,
        )
    } else {
        let watch_handle = Arc::new(WatchHandle::new()?);
        if let Some(seconds) = watch {
            let debounce = Duration::new(seconds, 0);

            client_replica
                .watch(Arc::downgrade(&watch_handle), debounce)
                .chain_err(|| "Error setting watch on client replica")?;
            server_replica
                .watch(Arc::downgrade(&watch_handle), debounce)
                .chain_err(|| "Error setting watch on server replica")?;
            interrupt::notify_on_signal(watch_handle.clone());
        }

        let context = Arc::new(reconcile::Context {
            cli: client_replica,
            anc: ancestor_replica,
            srv: server_replica,
            log: Box::new(log),
            root_rules: rules::engine::FileEngine::new(rules),
            work: work_stack::WorkStack::new(),
            tasks: reconcile::UnqueuedTasks::new(),
        });

        run_sync(
            context.clone(),
            level,
            num_threads,
            prepare_type,
            config,
            true,
            spin,
        )?;

        if watch.is_some() && !interrupt::is_interrupted() && level >= EDIT {
            perrln!(
                "Ensync will now continue to monitor for changes.\n\
                     Note that it may take a minute or so for changes \
                     to be noticed.\n\
                     Press Control+C to stop."
            );
        }

        if watch.is_some() {
            while !interrupt::is_interrupted() {
                watch_handle.wait();
                // Sleep for a seconds in case there are multiple notifications
                // coming in, but bail immediately if we're responding to ^C.
                for _ in 0..10 {
                    if interrupt::is_interrupted() {
                        break;
                    }
                    thread::sleep(Duration::new(0, 100_000_000));
                }
                if !watch_handle.check_dirty() {
                    continue;
                }

                run_sync(
                    context.clone(),
                    level,
                    num_threads,
                    if watch_handle.check_context_lost() {
                        PrepareType::Fast
                    } else {
                        PrepareType::Watched
                    },
                    config,
                    false,
                    spin,
                )?;
            }
        }

        Ok(())
    }
}

fn run_sync<
    CLI: Replica + 'static,
    ANC: Replica + NullTransfer + Condemn + 'static,
    SRV: Replica<TransferIn = CLI::TransferOut, TransferOut = CLI::TransferIn>
        + 'static,
>(
    context: Arc<reconcile::Context<CLI, ANC, SRV>>,
    level: LogLevel,
    num_threads: u32,
    prepare_type: PrepareType,
    config: &Config,
    show_messages: bool,
    spin: bool,
) -> Result<()> {
    macro_rules! spawn {
        (|$context:ident| $body:expr) => {{
            let $context = $context.clone();
            thread::spawn(move || $body)
        }};
    }

    context.log.log(INFO, &Log::SyncStarted);

    let last_config_path = config.private_root.join("last-config.dat");
    let min_prepare_type =
        match fs::File::open(&last_config_path).and_then(|mut file| {
            let mut hash = HashId::default();
            file.read_exact(&mut hash)?;
            Ok(hash)
        }) {
            Ok(h) if h == config.hash => PrepareType::Fast,
            Ok(_) => {
                perrln!(
                    "Configuration changed since last sync; all directories \
                     will be re-scanned."
                );
                PrepareType::Clean
            }
            Err(ref e) if io::ErrorKind::NotFound == e.kind() => {
                PrepareType::Clean
            }
            Err(e) => {
                perrln!(
                    "Error reading '{}': {}.\n\
                     Assuming configuration may have \
                     changed; all directories will be re-scanned.",
                    last_config_path.display(),
                    e
                );
                PrepareType::Clean
            }
        };

    let prepare_type = max(prepare_type, min_prepare_type);

    if level >= EDIT && show_messages {
        perrln!("Scanning files for changes...");
    }
    let client_prepare = spawn!(|context| context.cli.prepare(prepare_type));
    let server_prepare = spawn!(|context| context.srv.prepare(prepare_type));
    client_prepare
        .join()
        .expect("Child thread panicked")
        .chain_err(|| "Scanning for local changes failed")?;
    server_prepare
        .join()
        .expect("Child thread panicked")
        .chain_err(|| "Scanning for remote changes failed")?;

    if let Err(e) = fs::File::create(&last_config_path)
        .and_then(|mut file| file.write_all(&config.hash))
    {
        perrln!("Failed to write to '{}': {}", last_config_path.display(), e);
    }

    let root_state = context
        .start_root()
        .chain_err(|| "Failed to start sync process")?;

    let mut threads = Vec::new();
    for _ in 1..num_threads {
        threads.push(spawn!(|context| context.run_work()));
    }
    context.run_work();
    for thread in threads {
        let _ = thread.join().expect("Child thread panicked");
    }
    if spin {
        if show_messages {
            perrln!("");
        } else {
            perr!("\x1B[K");
        }
    }

    if interrupt::is_interrupted() {
        if level >= ERROR {
            perrln!("Syncing interrupted");
        }
    } else if root_state.success.load(SeqCst) {
        if level >= EDIT && show_messages {
            perrln!("Syncing completed successfully");
        }
    } else {
        if level >= ERROR && show_messages {
            perrln!("Syncing completed, but not clean");
        }
    }

    if level >= EDIT && show_messages {
        perrln!("Cleaning up...");
    }

    let client_cleanup = spawn!(|context| context.cli.clean_up());
    let server_cleanup = spawn!(|context| context.srv.clean_up());
    if let Err(err) = client_cleanup.join().expect("Child thread panicked") {
        if level >= ERROR {
            perrln!("Client cleanup failed: {}", err);
        }
    }
    if let Err(err) = server_cleanup.join().expect("Child thread panicked") {
        if level >= ERROR {
            perrln!("Server cleanup failed: {}", err);
        }
    }

    context.log.log(INFO, &Log::SyncFinished);

    Ok(())
}

fn check_for_copied_private_dir(private_dir: &Path) -> Result<()> {
    const FSID_VERSION: &str = "FSID2:";

    let ancestor_path = private_dir.join("ancestor.sqlite");
    let expected_content = match ancestor_path.metadata() {
        Err(_) => return Ok(()),
        // In an older version, we also included md.dev() here. This has been
        // removed since the device IDs are not stable on all systems (i.e.
        // different boots can result in different ids for the same files).
        Ok(md) => format!("{}{}", FSID_VERSION, md.ino()),
    };

    let marker_path = private_dir.join("fsid");
    let marker_content = fs::read(&marker_path).unwrap_or_default();
    if marker_content.starts_with(FSID_VERSION.as_bytes())
        && &marker_content[..] != expected_content.as_bytes()
    {
        perrln!(
            "\
            It looks like the ensync internal state was copied from somewhere else.
Aborting syncing since this could lead to catastrophic results.

You should probably delete {}
and then run `ensync sync` again. At worst, this will simply result in a much
slower and more conservative sync.

If you copied {}
from another machine in its entirety, please only copy the `config.toml` file
in the future.

If you are sure that the internal state corresponds to the state of your local
file system, you can instead disable this safety check by deleting
{}",
            private_dir.display(),
            private_dir.parent().unwrap().display(),
            marker_path.display()
        );

        return Err(ErrorKind::SanityCheckFailed.into());
    } else {
        let _ = fs::write(&marker_path, expected_content.as_bytes());
    }

    Ok(())
}
