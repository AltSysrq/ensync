//-
// Copyright (c) 2017, 2018, Jason Lingle
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
use std::io::{self, Read, Write, stdout, stderr};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};
use std::sync::atomic::Ordering::SeqCst;
use std::thread;
use std::time::Duration;
use libc::isatty;

use ancestor::*;
use cli::open_server::open_server_replica;
use cli::config::Config;
use cli::format_date;
use defs::*;
use dry_run_replica::DryRunReplica;
use errors::*;
use interrupt;
use log::*;
use posix::*;
use reconcile::compute::*;
use reconcile;
use replica::{Condemn, NullTransfer, PrepareType, Replica, Watch, WatchHandle};
use rules;
use server::*;
use work_stack;

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
        write!(f, "{}", self.1.as_path().strip_prefix(self.0)
               .unwrap().display())
    }
}
impl<'a> fmt::Display for PathDisplay<'a, (&'a OsStr, &'a OsStr)> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let prefix = (self.1).0.as_path().strip_prefix(self.0).unwrap();
        if prefix.components().next().is_some() {
            write!(f, "{}/{}",
                   prefix.display(),
                   (self.1).1.as_path().display())
        } else {
            write!(f, "{}", (self.1).1.as_path().display())
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
        macro_rules! perr {
            ($($arg:expr),+) => {
                let _ = write!(stderr_lock, $($arg),+);
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

        fn pretty_size(mut size: u64) -> String {
            let suffixes = ["bytes", "kB", "MB", "GB",
                            "TB", "PB", "EB", "ZB", "YB"];
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
                    FileData::Directory(mode) =>
                        write!(f, "directory (mode {:04o})", mode),
                    FileData::Regular(mode, size, time, _) => {
                        write!(f, "regular file (mode {:04o}, size {}, \
                                   modified {})",
                               mode, pretty_size(size),
                               format_date::format_timestamp(time))
                    },
                    FileData::Symlink(ref target) =>
                        write!(f, "symlink to {}",
                               target.as_path().display()),
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
            }
        }

        match *what {
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
                        format!("\n        (conflict: deleted on {} side, \
                                 changed on {} side)",
                                name_side(deleted_side),
                                name_side(deleted_side.rev()))),
                    Conflict::EditEdit(client, server) => Cow::Owned(
                        format!("\n        (conflict: {} changed locally, \
                                 {} changed remotely)", name_edit(client),
                                name_edit(server))),
                };

                say!((dir, name), ReplicaSide::Client, "{}{}",
                     recon_str, conflict_str)
            },

            Log::Create(side, path, name, state) => {
                update_spin!(|spin| {
                    match side {
                        ReplicaSide::Client => {
                            spin.cli.created += 1;
                            spin.cli.transfer += spin_size(state);
                        },
                        ReplicaSide::Server => {
                            spin.srv.created += 1;
                            spin.srv.transfer += spin_size(state);
                        },
                        ReplicaSide::Ancestor => { },
                    }
                });

                if let FileData::Directory(..) = *state {
                    self.created_directories.write().unwrap()
                        .insert(Path::new(path).join(name));
                }

                if self.include_ops_under_opped_directory ||
                    !self.created_directories.read().unwrap().contains(
                        Path::new(path))
                {
                    say!((path, name), side, "create\
                                              \n        + {}", FDD(state))
                }
            },

            Log::Update(side, path, name, old, new) => {
                let content_change = !old.matches_content(new);
                update_spin!(|spin| {
                    match side {
                        ReplicaSide::Client => {
                            spin.cli.updated += 1;
                            if content_change {
                                spin.cli.transfer += spin_size(new);
                            }
                        },
                        ReplicaSide::Server => {
                            spin.srv.updated += 1;
                            if content_change {
                                spin.srv.transfer += spin_size(new);
                            }
                        },
                        ReplicaSide::Ancestor => { },
                    }
                });

                say!((path, name), side, "update\
                                          \n        - {}\
                                          \n        + {}",
                     FDD(old), FDD(new));
            },

            Log::Rename(side, path, old, new) =>
                say!((path, old), side, "rename\
                                         \n        -> {}",
                     new.as_path().display()),

            Log::Remove(side, path, name, state) => {
                update_spin!(|spin| {
                    match side {
                        ReplicaSide::Client => spin.cli.deleted += 1,
                        ReplicaSide::Server => spin.srv.deleted += 1,
                        ReplicaSide::Ancestor => (),
                    }
                });
                if self.include_ops_under_opped_directory ||
                    !self.recdel_directories.read().unwrap().contains(
                        Path::new(path))
                {
                    say!((path, name), side, "delete\
                                              \n        - {}", FDD(state));
                }
            },

            Log::RecursiveDelete(side, path) => {
                self.recdel_directories.write().unwrap()
                    .insert(Path::new(path).to_owned());
                if self.include_ops_under_opped_directory ||
                    Path::new(path).parent().map_or(
                        true, |p| !self.recdel_directories.read()
                            .unwrap().contains(p))
                {
                    say!(path, side, "delete recursively");
                }
            },

            Log::Rmdir(side, path) => {
                // Don't update the spinner here, since we don't know whether
                // there is an actual directory being deleted.

                if self.include_ops_under_opped_directory ||
                    !self.recdel_directories.read().unwrap().contains(
                        Path::new(path))
                {
                    say!(path, side, "remove directory");
                }
            },

            Log::Error(side, path, ref op, err) => {
                let mut errs = err.to_string();

                for e in err.iter().skip(1) {
                    errs.push_str(&format!("\ncaused by: {}", e));
                }

                if let Some(bt) = err.backtrace() {
                    errs.push_str(&format!("\n{:?}\n", bt));
                }

                match *op {
                    ErrorOperation::List =>
                        say!(path, side, "Failed to list directory: {}", errs),

                    ErrorOperation::MarkClean =>
                        say!(path, side, "Failed to mark directory clean: {}",
                             errs),

                    ErrorOperation::Chdir(name) =>
                        say!((path, name), side,
                             "Failed to enter directory: {}", errs),

                    ErrorOperation::Create(name) =>
                        say!((path, name), side,
                             "Failed to create: {}", errs),

                    ErrorOperation::Update(name) =>
                        say!((path, name), side,
                             "Failed to update: {}", errs),

                    ErrorOperation::Rename(name) =>
                        say!((path, name), side,
                             "Failed to rename: {}", errs),

                    ErrorOperation::Remove(name) =>
                        say!((path, name), side,
                             "Failed to remove: {}", errs),

                    ErrorOperation::Rmdir =>
                        say!(path, side, "Failed to remove: {}", errs),

                    ErrorOperation::Access(name) =>
                        say!((path, name), side, "Failed to access: {}", errs),
                }
            },
        }

        if reprint_spin {
            if let Some(ref spin) = self.spin {
                let mut spin = spin.lock().unwrap();
                spin.cycle = (spin.cycle + 1) % 4;
                perr!("\x1B[K{} in: +{} *{} -{} {}, out: +{} *{} -{} {}\r",
                      ['-', '\\', '|', '/'][spin.cycle as usize],
                      spin.cli.created, spin.cli.updated, spin.cli.deleted,
                      pretty_size(spin.cli.transfer),
                      spin.srv.created, spin.srv.updated, spin.srv.deleted,
                      pretty_size(spin.srv.transfer));
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
            Log::Error(..) => { },
            Log::RecursiveDelete(..) => { },

            Log::Inspect(parent, name, Reconciliation::InSync, _) |
            Log::Inspect(parent, name, Reconciliation::Unsync, _) |
            Log::Inspect(parent, name, Reconciliation::Irreconcilable, _) => {
                // We can't really output a file type even if the data were
                // included in this log type, since the three replicas could
                // each have a different file type.
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
           colour: &str, spin: &str,
           include_ancestors: bool,
           dry_run: bool,
           watch: bool,
           num_threads: u32,
           prepare_type: &str,
           override_mode: Option<rules::SyncMode>,
           // Since --reconnect can cause multiple runs to occur, allow the
           // caller to remember the derived keychain so we don't need to
           // prompt for the passphrase again.
           key_chain: &mut Option<Arc<KeyChain>>) -> Result<()> {
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
        "auto" => if override_mode.is_some() {
            PrepareType::Clean
        } else {
            PrepareType::Fast
        },
        "fast" => PrepareType::Fast,
        "clean" => PrepareType::Clean,
        "scrub" => PrepareType::Scrub,
        _ => PrepareType::Fast,
    };

    if key_chain.is_none() {
        let passphrase = config.passphrase.read_passphrase(
            "passphrase", false)?;
        *key_chain = Some(Arc::new(
            keymgmt::derive_key_chain(&*storage, &passphrase)?));
    }

    let key_chain = key_chain.as_ref().unwrap().clone();

    let mut server_replica = open_server_replica(
        config, storage, Some(key_chain.clone()))?;

    let client_private_dir = config.private_root.join("client");
    fs::create_dir_all(&client_private_dir).chain_err(
        || format!("Failed to create client replica private directory '{}'",
                   client_private_dir.display()))?;
    let mut client_replica = PosixReplica::new(
        config.client_root.clone(), client_private_dir,
        key_chain.obj_hmac_secret()?, config.block_size as usize)
        .chain_err(|| "Failed to set up client replica")?;

    let ancestor_replica = AncestorReplica::open(
        config.private_root.join("ancestor.sqlite")
            .to_str().ok_or_else(
                || format!("Path '{}' is not valid UTF-8",
                           config.private_root.display()))?)
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
        include_ops_under_opped_directory:
            include_ops_under_opped_directory,
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
    };

    interrupt::install_signal_handler();

    let rules = match override_mode {
        None => config.sync_rules.clone(),
        Some(overide) => Arc::new(
            rules::engine::SyncRules::single_mode(overide)),
    };

    if dry_run {
        let context = Arc::new(reconcile::Context::<
                DryRunReplica<PosixReplica>,
                DryRunReplica<AncestorReplica>,
                DryRunReplica<ServerReplica<Storage>>,
                LoggerImpl, rules::engine::DirEngine> {
            cli: DryRunReplica(client_replica),
            anc: DryRunReplica(ancestor_replica),
            srv: DryRunReplica(server_replica),
            log: log,
            root_rules: rules::engine::FileEngine::new(rules),
            work: work_stack::WorkStack::new(),
            tasks: reconcile::UnqueuedTasks::new(),
        });

        run_sync(context, level, num_threads, prepare_type, config, true)
    } else {
        let watch_handle = Arc::new(WatchHandle::default());
        if watch {
            client_replica.watch(Arc::downgrade(&watch_handle)).chain_err(
                || "Error setting watch on client replica")?;
            server_replica.watch(Arc::downgrade(&watch_handle)).chain_err(
                || "Error setting watch on server replica")?;
            interrupt::notify_on_signal(watch_handle.clone());
        }

        // For some reason the type parms on `Context` are required
        let context = Arc::new(reconcile::Context::<
                PosixReplica, AncestorReplica, ServerReplica<Storage>,
                LoggerImpl, rules::engine::DirEngine> {
            cli: client_replica,
            anc: ancestor_replica,
            srv: server_replica,
            log: log,
            root_rules: rules::engine::FileEngine::new(rules),
            work: work_stack::WorkStack::new(),
            tasks: reconcile::UnqueuedTasks::new(),
        });

        run_sync(context.clone(), level, num_threads, prepare_type,
                 config, true)?;

        if watch && !interrupt::is_interrupted() && level >= EDIT {
            perrln!("Ensync will now continue to monitor for changes.\n\
                     Note that it may take a minute or so for changes \
                     to be noticed.\n\
                     Press Control+C to stop.");
        }

        if watch { 'outer: while !interrupt::is_interrupted() {
            watch_handle.wait();
            // Sleep for a few seconds in case there are multiple notifications
            // coming in, but bail immediately if we're responding to ^C.
            for _ in 0..50 {
                if interrupt::is_interrupted() { break; }
                thread::sleep(Duration::new(0, 100_000_000));
            }
            if !watch_handle.check_dirty() { continue; }

            run_sync(context.clone(), level, num_threads,
                     if watch_handle.check_context_lost() {
                         PrepareType::Fast
                     } else {
                         PrepareType::Watched
                     }, config, false)?;
        } }

        Ok(())
    }
}

fn run_sync<CLI : Replica + 'static,
            ANC : Replica + NullTransfer + Condemn + 'static,
            SRV : Replica<TransferIn = CLI::TransferOut,
                          TransferOut = CLI::TransferIn> + 'static>
    (context: Arc<reconcile::Context<CLI, ANC, SRV, LoggerImpl,
                                     rules::engine::DirEngine>>,
     level: LogLevel, num_threads: u32, prepare_type: PrepareType,
     config: &Config, show_messages: bool)
    -> Result<()>
{
    macro_rules! spawn {
        (|$context:ident| $body:expr) => { {
            let $context = $context.clone();
            thread::spawn(move || $body)
        } }
    }

    let last_config_path = config.private_root.join("last-config.dat");
    let min_prepare_type = match fs::File::open(&last_config_path).and_then(
        |mut file| {
            let mut hash = HashId::default();
            file.read_exact(&mut hash)?;
            Ok(hash)
        })
    {
        Ok(h) if h == config.hash => PrepareType::Fast,
        Ok(_) => {
            perrln!("Configuration changed since last sync; all directories \
                     will be re-scanned.");
            PrepareType::Clean
        },
        Err(ref e) if io::ErrorKind::NotFound == e.kind() =>
            PrepareType::Clean,
        Err(e) => {
            perrln!("Error reading '{}': {}.\n\
                     Assuming configuration may have \
                     changed; all directories will be re-scanned.",
                    last_config_path.display(), e);
            PrepareType::Clean
        },
    };

    let prepare_type = max(prepare_type, min_prepare_type);

    if level >= EDIT && show_messages {
        perrln!("Scanning files for changes...");
    }
    let client_prepare = spawn!(
        |context| context.cli.prepare(prepare_type));
    let server_prepare = spawn!(
        |context| context.srv.prepare(prepare_type));
    client_prepare.join()
        .expect("Child thread panicked")
        .chain_err(|| "Scanning for local changes failed")?;
    server_prepare.join()
        .expect("Child thread panicked")
        .chain_err(|| "Scanning for remote changes failed")?;

    if let Err(e) = fs::File::create(&last_config_path).and_then(
        |mut file| file.write_all(&config.hash))
    {
        perrln!("Failed to write to '{}': {}", last_config_path.display(), e);
    }

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
    if context.log.spin.is_some() {
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

    Ok(())
}
