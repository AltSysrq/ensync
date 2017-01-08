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

use std::borrow::Cow;
use std::cmp::{max, min};
use std::ffi::OsStr;
use std::fmt;
use std::fs;
use std::io::{Write, stdout, stderr};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering::SeqCst;
use std::thread;
use libc::isatty;

use ancestor::*;
use cli::open_server::open_server_replica;
use cli::config::Config;
use defs::*;
use errors::*;
use log::*;
use posix::*;
use reconcile::compute::*;
use reconcile;
use replica::Replica;
use rules;
use server::*;
use work_stack;

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
        write!(f, "{}", self.1.as_path().strip_prefix(self.0)
               .unwrap().display())
    }
}
impl<'a> fmt::Display for PathDisplay<'a, (&'a OsStr, &'a OsStr)> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}/{}",
               (self.1).0.as_path().strip_prefix(self.0)
               .unwrap().display(),
               (self.1).1.as_path().display())
    }
}

#[derive(Debug)]
struct LoggerImpl {
    client_root: PathBuf,
    verbose_level: LogLevel,
    itemise_level: LogLevel,
    include_ancestors: bool,
    colour: bool,
}

impl Logger for LoggerImpl {
    fn log(&self, level: LogLevel, what: &Log) {
        if level <= self.verbose_level {
            self.write_human_readable(level, what);
        }
        if level <= self.itemise_level {
            self.write_itemised(what);
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

        fn name_side<S : Into<ReplicaSide>> (side: S) -> &'static str {
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

        struct FDD<'a>(&'a FileData);
        impl<'a> fmt::Display for FDD<'a> {
            fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
                match *self.0 {
                    FileData::Directory(mode) =>
                        write!(f, "directory (mode {:04o})", mode),
                    FileData::Regular(mode, mut size, _, _) => {
                        let suffixes = ["bytes", "kB", "MB", "GB",
                                        "TB", "PB", "EB", "ZB", "YB"];
                        let mut suffix_ix = 0usize;
                        while size > 10000 {
                            size /= 1024;
                            suffix_ix += 1;
                        }

                        write!(f, "regular file (mode {:04o}, size {} {})",
                               mode, size, suffixes[suffix_ix])
                    },
                    FileData::Symlink(ref target) =>
                        write!(f, "symlink to {}",
                               target.as_path().display()),
                    FileData::Special => write!(f, "special file"),
                }
            }
        }

        macro_rules! say {
            ($path:expr, $side:expr, $extra:tt $(, $arg:expr)*) => {{
                let (start_colour, level_name) = match level {
                    FATAL => ("\x1B[1;31m", "FATAL"),
                    ERROR => ("\x1B[31m", "ERROR"),
                    WARN  => ("\x1B[33m", "WARN "),
                    EDIT  => ("\x1B[34m", "EDIT "),
                    INFO  => ("", "INFO "),
                    _ => panic!("Unexpected level {}", level),
                };

                let side = ReplicaSide::from($side);
                let side_name = match side {
                    ReplicaSide::Client =>   "local ",
                    ReplicaSide::Server =>   "remote",
                    ReplicaSide::Ancestor => "ancest",
                };

                if ReplicaSide::Ancestor != side || self.include_ancestors {
                    perrln!(concat!("[{}{}{}] {} {}: ", $extra),
                            if self.colour { start_colour } else { "" },
                            level_name,
                            if self.colour { "\x1B[0m" } else { "" },
                            side_name, PathDisplay(&self.client_root, $path)
                            $(, $arg)*);
                }
            }}
        }

        match *what {
            Log::EnterDirectory(dir) =>
                say!(dir, ReplicaSide::Client, "Entering directory"),
            Log::LeaveDirectory(dir) =>
                say!(dir, ReplicaSide::Client, "Leaving directory"),

            Log::Inspect(dir, name, reconciliation, conflict) => {
                let recon_str = match reconciliation {
                    Reconciliation::InSync =>
                        Cow::Borrowed("in sync"),
                    Reconciliation::Unsync =>
                        Cow::Borrowed("out of sync, not changing"),
                    Reconciliation::Irreconcilable =>
                        Cow::Borrowed("irreconcilable"),
                    Reconciliation::Use(side) => Cow::Owned(
                        format!("using {} version", name_side(side))),
                    Reconciliation::Split(side, _) => Cow::Owned(
                        format!("renaming file on {}", name_side(side))),
                };
                let conflict_str = match conflict {
                    Conflict::NoConflict => Cow::Borrowed(""),
                    Conflict::EditDelete(deleted_side) => Cow::Owned(
                        format!(" (conflict: deleted on {} side, \
                                 changed on {} side)",
                                name_side(deleted_side),
                                name_side(deleted_side.rev()))),
                    Conflict::EditEdit(client, server) => Cow::Owned(
                        format!(" (conflict: {} changed locally, \
                                 {} changed remotely)", name_edit(client),
                                name_edit(server))),
                };

                say!((dir, name), ReplicaSide::Client, "{}{}",
                     recon_str, conflict_str)
            },

            Log::Create(side, path, name, state) =>
                say!((path, name), side, "New {}", FDD(state)),

            Log::Update(side, path, name, old, new) =>
                say!((path, name), side, "Change {} to {}",
                     FDD(old), FDD(new)),

            Log::Rename(side, path, old, new) =>
                say!((path, old), side, "Rename to '{}'",
                     new.as_path().display()),

            Log::Remove(side, path, name, state) =>
                say!((path, name), side, "Remove {}", FDD(state)),

            Log::Rmdir(side, path) =>
                say!(path, side, "Remove directory"),

            Log::Error(side, path, ref op, err) => {
                match *op {
                    ErrorOperation::List =>
                        say!(path, side, "Failed to list directory: {}", err),

                    ErrorOperation::MarkClean =>
                        say!(path, side, "Failed to mark directory clean: {}",
                             err),

                    ErrorOperation::Chdir(name) =>
                        say!((path, name), side,
                             "Failed to enter directory: {}", err),

                    ErrorOperation::Create(name) =>
                        say!((path, name), side,
                             "Failed to create: {}", err),

                    ErrorOperation::Update(name) =>
                        say!((path, name), side,
                             "Failed to update: {}", err),

                    ErrorOperation::Rename(name) =>
                        say!((path, name), side,
                             "Failed to rename: {}", err),

                    ErrorOperation::Remove(name) =>
                        say!((path, name), side,
                             "Failed to remove: {}", err),

                    ErrorOperation::Rmdir =>
                        say!(path, side, "Failed to remove: {}", err),

                    ErrorOperation::Access(name) =>
                        say!((path, name), side, "Failed to access: {}", err),
                }

                for e in err.iter().skip(1) {
                    perrln!("caused by: {}", e);
                }

                if let Some(bt) = err.backtrace() {
                    perrln!("{:?}", bt);
                }
            },
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
                write!(f, "{}{}{}{}{}{}{}{}{}{}{}",
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
                       fill)
            }
        }

        macro_rules! say {
            ($item:expr, $path:expr) => { {
                let _ = writeln!(stdout_lock, "{:<11} {}",
                                 $item, PathDisplay(&self.client_root, $path));
            } }
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
            Log::EnterDirectory(_) |
            Log::LeaveDirectory(_) |
            Log::Error(..) => { },

            Log::Inspect(parent, name, Reconciliation::InSync, _) |
            Log::Inspect(parent, name, Reconciliation::Unsync, _) |
            Log::Inspect(parent, name, Reconciliation::Irreconcilable, _) => {
                // TODO We should get the actual file type here too
                say!(LineItem {
                    fill: Some(' '),
                    update_type: Some('.'),
                    file_type: Some('?'),
                    .. LineItem::default()
                }, (parent, name));
            },

            Log::Inspect(..) => { },

            Log::Create(side, parent, name, data) => if nan(side) {
                say!(LineItem {
                    fill: Some('+'),
                    update_type: update_type(side, data),
                    file_type: file_type(data),
                    .. LineItem::default()
                }, (parent, name))
            },

            Log::Update(side, parent, name, old, new) => if nan(side) {
                let (content_change, size_change, time_change, mode_change) =
                    match (old, new) {
                        (&FileData::Regular(mode1, size1, time1, content1),
                         &FileData::Regular(mode2, size2, time2, content2)) =>
                            (content1 != content2, size1 != size2,
                             time1 != time2, mode1 != mode2),

                        (&FileData::Symlink(..), &FileData::Symlink(..)) =>
                            (true, false, false, false),

                        (&FileData::Directory(..), &FileData::Directory(..)) =>
                            (false, false, false, true),

                        _ => (true, false, false, true),
                    };

                say!(LineItem {
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
                    .. LineItem::default()
                }, (parent, name));
            },

            Log::Rename(side, parent, old, new) => if nan(side) {
                say!("*renamefrom", (parent, old));
                say!("*renameto", (parent, new));
            },

            Log::Remove(side, parent, name, _) => if nan(side) {
                say!("*delete", (parent, name));
            },

            Log::Rmdir(side, path) => if nan(side) {
                say!("*delete", path);
            },
        }
    }
}

pub fn run(config: &Config, storage: Arc<Storage>,
           verbosity: i32, quietness: i32,
           itemise: bool, itemise_unchanged: bool,
           colour: &str, include_ancestors: bool,
           num_threads: u32) -> Result<()> {
    let colour = match colour {
        "never" => false,
        "always" => true,
        "auto" => 1 == unsafe { isatty(2) },
        _ => false,
    };

    let passphrase = config.passphrase.read_passphrase(
        "passphrase", false)?;
    let master_key = Arc::new(
        keymgmt::derive_master_key(&*storage, &passphrase)?);

    fs::create_dir_all(&config.private_root).chain_err(
        || format!("Failed to create ensync private directory '{}'",
                   config.private_root.display()))?;

    let server_replica = open_server_replica(
        config, storage.clone(), Some(master_key.clone()))?;
    // TODO This should be a manual step elsewhere
    server_replica.create_root()?;

    let client_private_dir = config.private_root.join("client");
    fs::create_dir_all(&client_private_dir).chain_err(
        || format!("Failed to create client replica private directory '{}'",
                   client_private_dir.display()))?;
    let client_replica = PosixReplica::new(
        config.client_root.clone(), client_private_dir,
        master_key.hmac_secret(), config.block_size as usize)
        .chain_err(|| "Failed to set up client replica")?;

    let ancestor_replica = AncestorReplica::open(
        config.private_root.join("ancestor.sqlite")
            .to_str().ok_or_else(
                || format!("Path '{}' is not valid UTF-8",
                           config.private_root.display()))?)
        .chain_err(|| "Failed to set up ancestor replica")?;

    let nominal_log_level = (WARN as i32) + verbosity - quietness;
    let level = max(FATAL as i32, min(255, nominal_log_level)) as LogLevel;

    let log = LoggerImpl {
        client_root: config.client_root.to_owned(),
        verbose_level: level,
        itemise_level: if !itemise {
            0
        } else if !itemise_unchanged {
            EDIT
        } else {
            INFO
        },
        include_ancestors: include_ancestors,
        colour: colour,
    };

    // For some reason the type parms on `Context` are required
    let context = Arc::new(reconcile::Context::<
            PosixReplica, AncestorReplica, ServerReplica<Storage>,
            LoggerImpl, rules::engine::DirEngine> {
        cli: client_replica,
        anc: ancestor_replica,
        srv: server_replica,
        log: log,
        root_rules: rules::engine::FileEngine::new(
            config.sync_rules.clone()),
        work: work_stack::WorkStack::new(),
        tasks: reconcile::UnqueuedTasks::new(),
    });

    macro_rules! spawn {
        (|$context:ident| $body:expr) => { {
            let $context = $context.clone();
            thread::spawn(move || $body)
        } }
    }
    macro_rules! perrln {
        ($($arg:expr),+) => {
            let _ = writeln!(stderr(), $($arg),+);
        }
    }

    if level >= WARN {
        perrln!("Scanning files for changes...");
    }
    let client_prepare = spawn!(
        |context| context.cli.prepare());
    let server_prepare = spawn!(
        |context| context.srv.prepare());
    client_prepare.join()
        .expect("Child thread panicked")
        .chain_err(|| "Scanning for local changes failed")?;
    server_prepare.join()
        .expect("Child thread panicked")
        .chain_err(|| "Scanning for remote changes failed")?;

    let root_state = context.start_root()
        .chain_err(|| "Failed to start sync process")?;

    let mut threads = Vec::new();
    for _ in 1..num_threads {
        threads.push(spawn!(|context| context.run_work()));
    }
    context.run_work();
    for thread in threads {
        let _ = thread.join().expect("Child thread panicked");
    }

    if root_state.success.load(SeqCst) {
        if level >= EDIT {
            perrln!("Syncing completed successfully");
        }
    } else {
        if level >= ERROR {
            perrln!("Syncing completed, but errors occurred");
        }
    }

    if level >= EDIT {
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

    Ok(())
}
