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

use std::collections::{BTreeMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::sync::atomic::Ordering::SeqCst;
use std::sync::atomic::{AtomicBool, AtomicUsize};
use std::sync::Arc;

use super::compute::*;
use super::context::*;
use super::mutate::ApplyResult;
use crate::defs::*;
use crate::errors::*;
use crate::interrupt::is_interrupted;
use crate::log::{self, Log, Logger};
use crate::replica::{Condemn, NullTransfer, Replica, ReplicaDirectory};
use crate::rules::engine::{DirEngineBuilder, FileEngine};

/// The state for processing a single directory.
///
/// This is used to coordinate actions that take place after processing of all
/// children of a directory has completed. Note that there is a certain amount
/// of spooky action at a distance here.
pub struct DirState {
    /// Whether all operations within this directory have succeeded.
    pub success: AtomicBool,
    /// Whether the directory is believed empty on all replicas. This is set to
    /// true by processing of the directory proper if all files either
    /// reconcile to nothing or are recursive deletes; recursive deletes on
    /// completion clear their parent directory's `empty` flag if they did not
    /// end up removing their directory.
    empty: AtomicBool,
    /// The number of direct children of this directory (plus the directory
    /// itself) which have not completed processing.
    pending: AtomicUsize,
    /// id of a task in the interface's task map to run when `pending` reaches
    /// zero.
    ///
    /// This is not really initialised until processing the directory itself
    /// completes; however, it is still set to a real task before `pending`
    /// reaches zero.
    on_complete: AtomicUsize,
    /// If true, don't assert that `pending` is actually zero when the DirState
    /// gets dropped.
    quiet: bool,
}

impl DirState {
    fn fail(&self, _why: &str) {
        self.success.store(false, SeqCst);
    }
}

impl Drop for DirState {
    fn drop(&mut self) {
        debug_assert!(
            self.quiet || 0 == self.pending.load(SeqCst),
            "DirState dropped while it still had a pending \
                       count of {}",
            self.pending.load(SeqCst)
        );
    }
}

pub type DirStateRef = Arc<DirState>;

impl<
        CLI: Replica,
        ANC: Replica + NullTransfer + Condemn,
        SRV: Replica<TransferIn = CLI::TransferOut, TransferOut = CLI::TransferIn>,
    > Context<CLI, ANC, SRV>
{
    /// Processes a single file.
    ///
    /// That is, this reads the current file, decides what to do with it, then
    /// applies that resolution. If the file is a directory and we are to recurse
    /// into it, queues a task to do so.
    ///
    /// The `success` flag on `dirstate` is cleared if any operation fails. `empty`
    /// on `dirstate` is cleared if the result state is clean but the file still
    /// exists.
    fn process_file(
        &self,
        dir: &mut <Self as ContextExt>::Dir,
        dir_path: &OsStr,
        name: &OsStr,
        dirstate: &DirStateRef,
    ) {
        let mut cli = dir.cli.files.get(name).cloned();
        let anc = dir.anc.files.get(name).cloned();
        let srv = dir.srv.files.get(name).cloned();

        let rules = dir.rules.file(File(name, cli.as_ref().or(
        anc.as_ref()).or(srv.as_ref()).expect(
        "Attempted to reconcile a file that doesn't exist in any replica.")));

        if let Some(ref mut cli) = cli {
            if !rules.trust_client_unix_mode() {
                if let Some(other) = srv.as_ref().or(anc.as_ref()) {
                    cli.transrich_unix_mode(other);
                }
            }
        }

        let (recon, conflict) = choose_reconciliation(
            cli.as_ref(),
            anc.as_ref(),
            srv.as_ref(),
            rules.sync_mode(),
        );

        self.log.log(
            if conflict > Conflict::NoConflict {
                log::WARN
            } else {
                log::INFO
            },
            &Log::Inspect(dir_path, name, recon, conflict),
        );

        let res = self.apply_reconciliation(
            dir,
            dir_path,
            name,
            recon,
            cli.as_ref(),
            srv.as_ref(),
        );

        match res {
            ApplyResult::Clean(false) => {
                dirstate.empty.fetch_and(
                    !dir.cli.files.contains_key(name)
                        && !dir.anc.files.contains_key(name)
                        && !dir.srv.files.contains_key(name),
                    SeqCst,
                );
            }
            ApplyResult::Fail => dirstate.fail("ApplyResult::Fail"),
            ApplyResult::Clean(true) => {
                dirstate.empty.store(false, SeqCst);
                self.recurse_into_dir(
                    dir,
                    dir_path,
                    name,
                    rules,
                    dirstate.clone(),
                );
            }
            ApplyResult::RecursiveDelete(side, mode) => self.recursive_delete(
                dir,
                dir_path,
                name,
                rules,
                dirstate.clone(),
                side,
                mode,
            ),
        }
    }
}

/// Reads a directory out of a replica and builds the initial name->data map.
///
/// If an error occurs, it is logged and returned (so that the caller can use
/// `?`).
fn read_dir_contents<R: Replica, LOG: Logger>(
    r: &R,
    dir: &mut R::Directory,
    dir_path: &OsStr,
    mut rb: Option<&mut DirEngineBuilder>,
    log: &LOG,
    side: log::ReplicaSide,
) -> Result<BTreeMap<OsString, FileData>> {
    let mut ret = BTreeMap::new();
    let list = match r.list(dir) {
        Ok(l) => l,
        Err(err) => {
            log.log(
                err.level(),
                &Log::Error(side, dir_path, log::ErrorOperation::List, &err),
            );
            return Err(err);
        }
    };
    for (name, value) in list {
        rb.as_mut().map(|r| r.contains(File(&name, &value)));
        ret.insert(name, value);
    }
    Ok(ret)
}

impl<
        CLI: Replica,
        ANC: Replica + NullTransfer + Condemn,
        SRV: Replica<TransferIn = CLI::TransferOut, TransferOut = CLI::TransferIn>,
    > Context<CLI, ANC, SRV>
{
    /// Processes a directory.
    ///
    /// Reads the contents of all three replicas, constructs the directory context,
    /// calls `process_file()` on each name, then invokes `on_complete_supplier` to
    /// obtain the task to run when the directory is fully complete, then
    /// decrements the pending by 1 (completing the directory if no children were
    /// spawned).
    ///
    /// This may return Err if it was unable to set the context up at all. Any
    /// errors have already been logged; this is simply an artefact of using `?`
    /// to simplify control flow.
    fn process_dir_impl<
        F: FnOnce(<Self as ContextExt>::Dir, DirStateRef) -> Task<Self>,
    >(
        &self,
        mut cli_dir: CLI::Directory,
        mut anc_dir: ANC::Directory,
        mut srv_dir: SRV::Directory,
        mut rules_builder: DirEngineBuilder,
        on_complete_supplier: F,
    ) -> Result<()> {
        self.check_stop()?;

        let dir_path = cli_dir.full_path().to_owned();

        let cli_files = read_dir_contents(
            &self.cli,
            &mut cli_dir,
            &dir_path,
            Some(&mut rules_builder),
            &self.log,
            log::ReplicaSide::Client,
        )?;
        let anc_files = read_dir_contents(
            &self.anc,
            &mut anc_dir,
            &dir_path,
            None::<&mut DirEngineBuilder>,
            &self.log,
            log::ReplicaSide::Ancestor,
        )?;
        let srv_files = read_dir_contents(
            &self.srv,
            &mut srv_dir,
            &dir_path,
            Some(&mut rules_builder),
            &self.log,
            log::ReplicaSide::Server,
        )?;
        let rules = rules_builder.build();

        let mut dir = DirContext {
            cli: SingleDirContext {
                dir: cli_dir,
                files: cli_files,
            },
            anc: SingleDirContext {
                dir: anc_dir,
                files: anc_files,
            },
            srv: SingleDirContext {
                dir: srv_dir,
                files: srv_files,
            },
            todo: Default::default(),
            rules: rules,
        };

        let dirstate = Arc::new(DirState {
            success: AtomicBool::new(true),
            empty: AtomicBool::new(true),
            pending: AtomicUsize::new(1),
            on_complete: AtomicUsize::new(0),
            quiet: false,
        });

        {
            let mut names = HashSet::new();
            for name in dir.cli.files.keys() {
                names.insert(name);
            }
            for name in dir.anc.files.keys() {
                names.insert(name);
            }
            for name in dir.srv.files.keys() {
                names.insert(name);
            }
            for name in names {
                dir.todo.push(Reversed(name.to_owned()));
            }
        }

        while let Some(Reversed(name)) = dir.todo.pop() {
            self.process_file(&mut dir, &dir_path, &name, &dirstate);
        }

        dirstate.on_complete.store(
            self.tasks.put(on_complete_supplier(dir, dirstate.clone())),
            SeqCst,
        );
        self.finish_task_in_dir(&dirstate, &dirstate);
        Ok(())
    }

    pub fn should_stop(&self) -> bool {
        is_interrupted()
            || self.cli.is_fatal()
            || self.anc.is_fatal()
            || self.srv.is_fatal()
    }

    pub fn check_stop(&self) -> Result<()> {
        if self.should_stop() {
            Err(ErrorKind::ReconciliationStopped.into())
        } else {
            Ok(())
        }
    }

    /// Wraps process_dir_impl() to return whether the result was successful or
    /// not.
    fn process_dir<
        F: FnOnce(<Self as ContextExt>::Dir, DirStateRef) -> Task<Self>,
    >(
        &self,
        cli_dir: CLI::Directory,
        anc_dir: ANC::Directory,
        srv_dir: SRV::Directory,
        rules_builder: DirEngineBuilder,
        on_complete_suppvier: F,
    ) -> bool {
        self.process_dir_impl(
            cli_dir,
            anc_dir,
            srv_dir,
            rules_builder,
            on_complete_suppvier,
        )
        .is_ok()
    }
}

/// Calls `chdir()` on `r` and `parent` using `name`.
///
/// If an error occurs, it is logged and `None` is returned.
fn try_chdir<R: Replica, LOG: Logger>(
    r: &R,
    parent: &R::Directory,
    parent_name: &OsStr,
    name: &OsStr,
    log: &LOG,
    side: log::ReplicaSide,
) -> Option<R::Directory> {
    match r.chdir(parent, name) {
        Ok(dir) => Some(dir),
        Err(error) => {
            log.log(
                error.level(),
                &Log::Error(
                    side,
                    parent_name,
                    log::ErrorOperation::Chdir(name),
                    &error,
                ),
            );
            None
        }
    }
}

impl<
        CLI: Replica,
        ANC: Replica + NullTransfer + Condemn,
        SRV: Replica<TransferIn = CLI::TransferOut, TransferOut = CLI::TransferIn>,
    > Context<CLI, ANC, SRV>
{
    /// Queues a task to recurse into the given directory.
    ///
    /// When the subdirectory completes, it is marked clean if its `success` flag
    /// is set. The flags from the subdirectory's state are ANDed into `state`.
    ///
    /// If switching into the subdirectory fails, `state.success` is cleared and no
    /// task is enqueued. If `process_dir` returns false, `state.success` will be
    /// cleared.
    fn recurse_into_dir(
        &self,
        dir: &mut <Self as ContextExt>::Dir,
        parent_name: &OsStr,
        name: &OsStr,
        file_rules: FileEngine,
        state: DirStateRef,
    ) {
        match (
            try_chdir(
                &self.cli,
                &dir.cli.dir,
                parent_name,
                name,
                &self.log,
                log::ReplicaSide::Client,
            ),
            try_chdir(
                &self.anc,
                &dir.anc.dir,
                parent_name,
                name,
                &self.log,
                log::ReplicaSide::Ancestor,
            ),
            try_chdir(
                &self.srv,
                &dir.srv.dir,
                parent_name,
                name,
                &self.log,
                log::ReplicaSide::Server,
            ),
        ) {
            (Some(cli_dir), Some(anc_dir), Some(srv_dir)) => {
                if self.cli.is_dir_dirty(&cli_dir)
                    || self.srv.is_dir_dirty(&srv_dir)
                {
                    self.recurse_and_then(
                        cli_dir,
                        anc_dir,
                        srv_dir,
                        file_rules,
                        state,
                        |this, dir, _| this.mark_both_clean(&dir),
                    )
                }
            }

            _ => state.fail("recurse_into_dir chdir failed"),
        }
    }

    /// Queues a task to process the directory identified by
    /// `(cli_dir,anc_dir,srv_dir)`.
    ///
    /// If initialising the directory context fails, `state.success` is cleared and
    /// the current directory's completion queued if this was the last task.
    /// Otherwise, when processing the subdirectory is complete, `on_success` is
    /// invoked if the subdirectory's `success` flag is set, and the result of that
    /// function ANDed with the subdirectory's `success` flag. The flags of the
    /// subdirectory state are ANDed into `state`, and `state`'s completion queued
    /// if this was the last task.
    fn recurse_and_then<
        F: FnOnce(&Self, <Self as ContextExt>::Dir, &DirStateRef) -> bool
            + Send
            + 'static,
    >(
        &self,
        cli_dir: CLI::Directory,
        anc_dir: ANC::Directory,
        srv_dir: SRV::Directory,
        file_rules: FileEngine,
        state: DirStateRef,
        on_success: F,
    ) {
        state.pending.fetch_add(1, SeqCst);

        self.work.push(task(move |this| {
            let state2 = state.clone();
            let success = Self::process_dir(
                this,
                cli_dir,
                anc_dir,
                srv_dir,
                file_rules.subdir(),
                |dir, subdirstate| {
                    task(move |this| {
                        if subdirstate.success.load(SeqCst) {
                            if !on_success(this, dir, &subdirstate) {
                                subdirstate
                                    .fail("recurse_and_then on_success failed");
                            }
                        }
                        this.finish_task_in_dir(&state, &subdirstate);
                    })
                },
            );
            if !success {
                state2.fail("process_dir failed early");
                // process_dir() didn't create the task to decrement
                // `pending`, so we need to do that now.
                this.finish_task_in_dir(&state2, &state2);
            }
        }));
    }

    fn mark_both_clean(&self, dir: &<Self as ContextExt>::Dir) -> bool {
        let dir_path = dir.cli.dir.full_path();

        mark_clean(
            &self.cli,
            &dir.cli.dir,
            &self.log,
            log::ReplicaSide::Client,
            dir_path,
        ) && mark_clean(
            &self.srv,
            &dir.srv.dir,
            &self.log,
            log::ReplicaSide::Server,
            dir_path,
        )
    }
}

fn mark_clean<R: Replica, L: Logger>(
    r: &R,
    dir: &R::Directory,
    log: &L,
    side: log::ReplicaSide,
    dir_path: &OsStr,
) -> bool {
    match r.set_dir_clean(dir) {
        Ok(clean) => clean,
        Err(error) => {
            log.log(
                error.level(),
                &Log::Error(
                    side,
                    dir_path,
                    log::ErrorOperation::MarkClean,
                    &error,
                ),
            );
            false
        }
    }
}

impl<
        CLI: Replica,
        ANC: Replica + NullTransfer + Condemn,
        SRV: Replica<TransferIn = CLI::TransferOut, TransferOut = CLI::TransferIn>,
    > Context<CLI, ANC, SRV>
{
    /// Finishes a subtask of a directory.
    ///
    /// The flags of `state` are ANDed with those from `substate`. `state.pending`
    /// is decremented; if it reaches zero, the completion task is queued.
    fn finish_task_in_dir(&self, state: &DirStateRef, substate: &DirStateRef) {
        state
            .success
            .fetch_and(substate.success.load(SeqCst), SeqCst);
        state.empty.fetch_and(substate.empty.load(SeqCst), SeqCst);
        if 1 == state.pending.fetch_sub(1, SeqCst) {
            self.work
                .push(self.tasks.get(state.on_complete.load(SeqCst)));
        }
    }

    /// Queues a task to descend into a directory for a probable recursive delete.
    ///
    /// This is basically like `recurse_into_dir`, except that the replica opposite
    /// `side` is given a synthetic directory using `mode`, as well as the ancestor
    /// if necessary; on success, if all subtasks reported empty, the directories
    /// on all replicas are removed. Otherwise, on success, the directory is simply
    /// marked clean.
    fn recursive_delete(
        &self,
        dir: &mut <Self as ContextExt>::Dir,
        parent_name: &OsStr,
        name: &OsStr,
        file_rules: FileEngine,
        state: DirStateRef,
        side: ReconciliationSide,
        mode: FileMode,
    ) {
        fn chdir_or_synth<R: Replica, LOG: Logger>(
            r: &R,
            dir: &mut R::Directory,
            this_side: ReconciliationSide,
            phys_side: ReconciliationSide,
            log: &LOG,
            parent_name: &OsStr,
            name: &OsStr,
            mode: FileMode,
        ) -> Option<R::Directory> {
            if this_side == phys_side {
                try_chdir(r, dir, parent_name, name, log, this_side.into())
            } else {
                Some(r.synthdir(dir, name, mode))
            }
        }

        let cli_dir = chdir_or_synth(
            &self.cli,
            &mut dir.cli.dir,
            ReconciliationSide::Client,
            side,
            &self.log,
            parent_name,
            name,
            mode,
        );
        let anc_dir =
            self.anc.chdir(&dir.anc.dir, name).ok().unwrap_or_else(|| {
                self.anc.synthdir(&mut dir.anc.dir, name, mode)
            });
        let srv_dir = chdir_or_synth(
            &self.srv,
            &mut dir.srv.dir,
            ReconciliationSide::Server,
            side,
            &self.log,
            parent_name,
            name,
            mode,
        );

        match (cli_dir, anc_dir, srv_dir) {
            (Some(cli_dir), anc_dir, Some(srv_dir)) => {
                self.log.log(
                    log::EDIT,
                    &Log::RecursiveDelete(side.into(), cli_dir.full_path()),
                );

                self.recurse_and_then(
                    cli_dir,
                    anc_dir,
                    srv_dir,
                    file_rules,
                    state,
                    |this, mut dir, substate| {
                        this.delete_or_mark_clean(&mut dir, substate)
                    },
                );
            }

            _ => state.fail("recursive_delete chdir failed"),
        }
    }
}

fn try_rmdir<R: Replica, LOG: Logger>(
    r: &R,
    dir: &mut R::Directory,
    log: &LOG,
    dir_path: &OsStr,
    side: log::ReplicaSide,
) -> bool {
    log.log(log::EDIT, &Log::Rmdir(side, dir_path));
    match r.rmdir(dir) {
        Ok(_) => true,
        Err(error) => {
            log.log(
                error.level(),
                &Log::Error(side, dir_path, log::ErrorOperation::Rmdir, &error),
            );
            false
        }
    }
}

impl<
        CLI: Replica,
        ANC: Replica + NullTransfer + Condemn,
        SRV: Replica<TransferIn = CLI::TransferOut, TransferOut = CLI::TransferIn>,
    > Context<CLI, ANC, SRV>
{
    fn delete_or_mark_clean(
        &self,
        dir: &mut <Self as ContextExt>::Dir,
        state: &DirStateRef,
    ) -> bool {
        let dir_path = dir.cli.dir.full_path().to_owned();
        if state.empty.load(SeqCst) {
            try_rmdir(
                &self.anc,
                &mut dir.anc.dir,
                &self.log,
                &dir_path,
                log::ReplicaSide::Ancestor,
            ) && try_rmdir(
                &self.cli,
                &mut dir.cli.dir,
                &self.log,
                &dir_path,
                log::ReplicaSide::Client,
            ) && try_rmdir(
                &self.srv,
                &mut dir.srv.dir,
                &self.log,
                &dir_path,
                log::ReplicaSide::Server,
            )
        } else {
            self.mark_both_clean(dir)
        }
    }

    /// Enqueues a task to synchronise the replicas of the given interface,
    /// starting at their root directories.
    ///
    /// If the task is successfully enqueued, a reference to the root directory
    /// state is returned. When all tasks have cleared, the state can be inspected
    /// to see whether any errors occurred. Note that `pending` on the root state
    /// will never reach 0.
    ///
    /// This does not itself cause anything to be executed.
    pub fn start_root(&self) -> Result<DirStateRef> {
        let root_state = Arc::new(DirState {
            success: AtomicBool::new(true),
            empty: AtomicBool::new(true),
            // Never gets decremented to 0
            pending: AtomicUsize::new(1),
            on_complete: AtomicUsize::new(0),
            quiet: true,
        });
        self.recurse_and_then(
            self.cli.root()?,
            self.anc.root()?,
            self.srv.root()?,
            self.root_rules.clone(),
            root_state.clone(),
            |this, dir, _| this.mark_both_clean(&dir),
        );
        Ok(root_state)
    }
}

#[cfg(test)]
mod test {
    use std::collections::{HashMap, HashSet};
    use std::ffi::OsStr;
    use std::iter::Iterator;
    use std::sync::atomic::Ordering::SeqCst;
    use std::sync::Arc;

    use proptest;
    use proptest::strategy::{BoxedStrategy, Singleton, Strategy};

    use super::super::mutate::test::*;
    use super::*;
    use crate::memory_replica::*;
    use crate::rules::engine::DirEngine;
    use crate::rules::{HalfSyncMode, SyncMode, SyncModeSetting};

    // Structs which describe a file tree, used for initialising and verifying
    // the replicas for tests.
    #[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
    struct En(&'static str, FsFile, FsFile, FsFile, Vec<En>);
    type FsFile = (FsFileE, Faults);
    #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
    enum FsFileE {
        Nil,
        Reg(FileMode, u8),
        Dir(FileMode),
    }
    use self::FsFileE::*;
    #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
    struct Faults(u8);
    const Z: Faults = Faults(0);
    const F_CR: u8 = 1;
    const F_UP: u8 = 2;
    const F_RM: u8 = 4;
    const F_MV: u8 = 8;
    const F_CHDIR: u8 = 16;
    const F_LS: u8 = 32;

    fn init_replica<F: Fn(&En) -> FsFile>(
        replica: &MemoryReplica,
        fs: &Vec<En>,
        slot: F,
    ) {
        fn populate_dir<F: Fn(&En) -> FsFile>(
            replica: &MemoryReplica,
            dir: &mut DirHandle,
            ls: &Vec<En>,
            slot: &F,
        ) {
            let dirname = dir.full_path().to_owned();

            for en in ls {
                let (f, faults) = slot(en);
                let name = OsStr::new(en.0).to_owned();
                match f {
                    Nil => (),
                    Reg(mode, hash) => {
                        replica
                            .create(
                                dir,
                                File(
                                    &name,
                                    &FileData::Regular(mode, 0, 0, [hash; 32]),
                                ),
                                Some([hash; 32]),
                            )
                            .unwrap();
                    }
                    Dir(mode) => {
                        replica
                            .create(
                                dir,
                                File(&name, &FileData::Directory(mode)),
                                Some([0; 32]),
                            )
                            .unwrap();

                        let mut subdir = replica.chdir(dir, &name).unwrap();
                        populate_dir(replica, &mut subdir, &en.4, slot);
                    }
                }

                let mut to_fault = vec![];
                if 0 != faults.0 & F_CR {
                    to_fault.push(Op::Create(dirname.clone(), name.clone()));
                }
                if 0 != faults.0 & F_UP {
                    to_fault.push(Op::Update(dirname.clone(), name.clone()));
                }
                if 0 != faults.0 & F_RM {
                    to_fault.push(Op::Remove(dirname.clone(), name.clone()));
                }
                if 0 != faults.0 & F_MV {
                    to_fault.push(Op::Rename(dirname.clone(), name.clone()));
                }
                if 0 != faults.0 & F_LS {
                    to_fault.push(Op::List(catpath(&dirname, &name)));
                }
                if 0 != faults.0 & F_CHDIR {
                    to_fault.push(Op::Chdir(catpath(&dirname, &name)));
                }

                for tf in to_fault {
                    replica
                        .data()
                        .faults
                        .insert(tf, Box::new(|_| simple_error()));
                }
            }
        }

        populate_dir(&replica, &mut replica.root().unwrap(), fs, &slot);
    }

    fn init(fs: &Vec<En>) -> Fixture {
        let fx = Fixture::new();
        init_replica(&fx.client, fs, |t| t.1);
        init_replica(&fx.ancestor, fs, |t| t.2);
        init_replica(&fx.server, fs, |t| t.3);
        fx
    }

    fn verify_replica<F: Fn(&En) -> FsFile>(
        name: &str,
        replica: &MemoryReplica,
        fs: &Vec<En>,
        slot: F,
    ) {
        fn verify_dir<F: Fn(&En) -> FsFile>(
            name: &str,
            replica: &MemoryReplica,
            dir: &mut DirHandle,
            fs: &Vec<En>,
            slot: &F,
        ) {
            let mut files = HashMap::new();
            let dirname = dir.full_path().to_str().unwrap().to_owned();
            for (name, data) in replica.list(dir).unwrap() {
                files.insert(name.to_str().unwrap().to_owned(), data);
            }

            for en in fs {
                let data = files.remove(en.0);
                let data_en = match data {
                    Some(FileData::Regular(mode, _, _, hash)) => {
                        assert_eq!([hash[0]; 32], hash);
                        Reg(mode, hash[0])
                    }
                    Some(FileData::Directory(mode)) => Dir(mode),
                    None => Nil,
                    _ => panic!(
                        "{}: {}/{} unexpected file type: {:?}",
                        name, dirname, en.0, &data
                    ),
                };
                if data_en != slot(en).0 {
                    panic!(
                        "{}: {}/{} expected {:?}, got {:?}",
                        name,
                        dirname,
                        en.0,
                        slot(en).0,
                        data_en
                    );
                }

                if let Dir(_) = slot(en).0 {
                    verify_dir(
                        name,
                        replica,
                        &mut replica.chdir(dir, OsStr::new(en.0)).unwrap(),
                        &en.4,
                        slot,
                    );
                }
            }

            if let Some(unexpected) = files.keys().next() {
                panic!(
                    "{}: {}/{} not expected to exist, but it does",
                    name, dirname, unexpected
                );
            }
        }

        // Clear all faults so we can walk the replica
        replica.data().faults.clear();

        verify_dir(name, replica, &mut replica.root().unwrap(), fs, &slot);
    }

    fn verify(fx: &Fixture, fs: &Vec<En>) {
        verify_replica("client", &fx.client, fs, |t| t.1);
        verify_replica("server", &fx.server, fs, |t| t.3);
        verify_replica("ancestor", &fx.ancestor, fs, |t| t.2);
    }

    fn signature(replica: &MemoryReplica) -> String {
        fn dir_sig(
            dst: &mut String,
            replica: &MemoryReplica,
            dir: &mut DirHandle,
        ) {
            let mut contents = replica.list(dir).unwrap();
            contents.sort_by(|&(ref a, _), &(ref b, _)| a.cmp(b));

            for (ix, (name, child)) in contents.into_iter().enumerate() {
                if ix > 0 {
                    dst.push_str("; ");
                }
                dst.push_str(name.to_str().unwrap());
                dst.push_str(" = ");
                match child {
                    FileData::Regular(mode, _, _, hash) => {
                        dst.push_str(&format!("reg {}, {}", mode, hash[0]))
                    }

                    FileData::Directory(mode) => {
                        dst.push_str(&format!("dir {} (", mode));
                        dir_sig(
                            dst,
                            replica,
                            &mut replica.chdir(dir, &name).unwrap(),
                        );
                        dst.push_str(")");
                    }

                    _ => panic!("Unexpected file type"),
                }
            }
        }

        // Clear all faults so we can walk the replica
        replica.data().faults.clear();

        let mut res = String::new();
        dir_sig(&mut res, replica, &mut replica.root().unwrap());
        res
    }

    fn run_once(fx: &mut Fixture) -> DirState {
        fx.with_context(|context| {
            let dsr = context.start_root().unwrap();
            context.run_work();
            Arc::try_unwrap(dsr).ok().unwrap()
        })
    }

    fn run_full(fx: &mut Fixture) {
        let res = run_once(fx);
        if !res.success.load(SeqCst) {
            assert!(
                !fx.client.data().faults.is_empty()
                    || !fx.ancestor.data().faults.is_empty()
                    || !fx.server.data().faults.is_empty(),
                "Result had error even though there were no faults"
            );
            // Clear all faults and let it run again
            fx.client.data().faults.clear();
            fx.ancestor.data().faults.clear();
            fx.server.data().faults.clear();

            let res2 = run_once(fx);
            assert!(
                res2.success.load(SeqCst),
                "After clearing faults and rerunning, \
                     errors still occurred"
            );
        }
    }

    trait IntoRules {
        fn into_rules(self) -> DirEngine;
    }
    impl<'a> IntoRules for &'a str {
        fn into_rules(self) -> DirEngine {
            constant_rules(self.parse().unwrap(), true)
        }
    }
    impl<'a> IntoRules for (&'a str, bool) {
        fn into_rules(self) -> DirEngine {
            constant_rules(self.0.parse().unwrap(), self.1)
        }
    }

    fn test_single<R: IntoRules>(input: &Vec<En>, rules: R, output: &Vec<En>) {
        let mut fx = init(input);
        fx.rules = rules.into_rules();
        run_full(&mut fx);
        verify(&fx, output);

        let cli_sig_a = signature(&fx.client);
        let anc_sig_a = signature(&fx.ancestor);
        let srv_sig_a = signature(&fx.server);

        fx.client.mark_all_dirty();
        fx.server.mark_all_dirty();
        run_full(&mut fx);

        let cli_sig_b = signature(&fx.client);
        let anc_sig_b = signature(&fx.ancestor);
        let srv_sig_b = signature(&fx.server);

        assert!(
            cli_sig_a == cli_sig_b,
            "Client reconciliation not idempotent;\n\
                 A: {}\n\
                 B: {}",
            cli_sig_a,
            cli_sig_b
        );
        assert!(
            srv_sig_a == srv_sig_b,
            "Server reconciliation not idempotent;\n\
                 A: {}\n\
                 B: {}",
            srv_sig_a,
            srv_sig_b
        );
        assert!(
            anc_sig_a == anc_sig_b,
            "Ancestor reconciliation not idempotent;\n\
                 A: {}\n\
                 B: {}",
            anc_sig_a,
            anc_sig_b
        );
    }

    #[test]
    fn sync_empty() {
        test_single(&vec![], "---/---", &vec![]);
    }

    #[test]
    fn sync_flat() {
        test_single(
            &vec![
                En("foo", (Reg(7, 1), Z), (Nil, Z), (Nil, Z), vec![]),
                En("bar", (Nil, Z), (Nil, Z), (Reg(6, 2), Z), vec![]),
                En("baz", (Nil, Z), (Reg(7, 2), Z), (Reg(7, 2), Z), vec![]),
            ],
            "cud/cud",
            &vec![
                En(
                    "foo",
                    (Reg(7, 1), Z),
                    (Reg(7, 1), Z),
                    (Reg(7, 1), Z),
                    vec![],
                ),
                En(
                    "bar",
                    (Reg(6, 2), Z),
                    (Reg(6, 2), Z),
                    (Reg(6, 2), Z),
                    vec![],
                ),
            ],
        );
    }

    #[test]
    fn sync_recursive() {
        test_single(
            &vec![
                En(
                    "d1",
                    (Dir(7), Z),
                    (Nil, Z),
                    (Nil, Z),
                    vec![En("f1a", (Reg(7, 1), Z), (Nil, Z), (Nil, Z), vec![])],
                ),
                En(
                    "orp",
                    (Nil, Z),
                    (Dir(7), Z),
                    (Nil, Z),
                    vec![En("o1", (Nil, Z), (Reg(7, 2), Z), (Nil, Z), vec![])],
                ),
                En(
                    "d2",
                    (Nil, Z),
                    (Nil, Z),
                    (Dir(6), Z),
                    vec![En("f2a", (Nil, Z), (Nil, Z), (Reg(6, 3), Z), vec![])],
                ),
            ],
            "cud/cud",
            &vec![
                En(
                    "d1",
                    (Dir(7), Z),
                    (Dir(7), Z),
                    (Dir(7), Z),
                    vec![En(
                        "f1a",
                        (Reg(7, 1), Z),
                        (Reg(7, 1), Z),
                        (Reg(7, 1), Z),
                        vec![],
                    )],
                ),
                En(
                    "d2",
                    (Dir(6), Z),
                    (Dir(6), Z),
                    (Dir(6), Z),
                    vec![En(
                        "f2a",
                        (Reg(6, 3), Z),
                        (Reg(6, 3), Z),
                        (Reg(6, 3), Z),
                        vec![],
                    )],
                ),
            ],
        );
    }

    #[test]
    fn sync_recursive_delete_complete() {
        test_single(
            &vec![En(
                "a",
                (Dir(7), Z),
                (Dir(7), Z),
                (Nil, Z),
                vec![
                    En("af", (Reg(7, 1), Z), (Reg(7, 1), Z), (Nil, Z), vec![]),
                    En(
                        "b",
                        (Dir(7), Z),
                        (Dir(7), Z),
                        (Nil, Z),
                        vec![En(
                            "bf",
                            (Reg(7, 2), Z),
                            (Reg(7, 2), Z),
                            (Nil, Z),
                            vec![],
                        )],
                    ),
                ],
            )],
            "cud/cud",
            &vec![],
        );
    }

    #[test]
    fn sync_recursive_delete_interrupted() {
        test_single(
            &vec![En(
                "a",
                (Dir(7), Z),
                (Dir(7), Z),
                (Nil, Z),
                vec![
                    En("af", (Reg(7, 1), Z), (Reg(7, 1), Z), (Nil, Z), vec![]),
                    En(
                        "b",
                        (Dir(7), Z),
                        (Dir(7), Z),
                        (Nil, Z),
                        vec![En(
                            "bf",
                            (Reg(7, 2), Z),
                            (Reg(7, 2), Z),
                            (Nil, Z),
                            vec![],
                        )],
                    ),
                    En("af2", (Reg(7, 2), Z), (Nil, Z), (Nil, Z), vec![]),
                ],
            )],
            "cud/cud",
            &vec![En(
                "a",
                (Dir(7), Z),
                (Dir(7), Z),
                (Dir(7), Z),
                vec![En(
                    "af2",
                    (Reg(7, 2), Z),
                    (Reg(7, 2), Z),
                    (Reg(7, 2), Z),
                    vec![],
                )],
            )],
        );
    }

    #[test]
    fn sync_edit_conflict() {
        test_single(
            &vec![En(
                "foo",
                (Reg(7, 1), Z),
                (Reg(7, 2), Z),
                (Reg(7, 3), Z),
                vec![],
            )],
            "cud/cud",
            &vec![
                En(
                    "foo",
                    (Reg(7, 1), Z),
                    (Reg(7, 1), Z),
                    (Reg(7, 1), Z),
                    vec![],
                ),
                En(
                    "foo~1",
                    (Reg(7, 3), Z),
                    (Reg(7, 3), Z),
                    (Reg(7, 3), Z),
                    vec![],
                ),
            ],
        );
    }

    #[test]
    fn sync_replace_dir_with_reg() {
        test_single(
            &vec![En(
                "foo",
                (Dir(7), Z),
                (Dir(7), Z),
                (Reg(6, 1), Z),
                vec![En("a", (Reg(6, 2), Z), (Reg(6, 2), Z), (Nil, Z), vec![])],
            )],
            "cud/cud",
            &vec![En(
                "foo",
                (Reg(6, 1), Z),
                (Reg(6, 1), Z),
                (Reg(6, 1), Z),
                vec![],
            )],
        );
    }

    #[test]
    fn sync_no_panic_on_failed_chdir() {
        test_single(
            &vec![En(
                "C8~1",
                (Nil, Z),
                (Nil, Z),
                (Dir(0), Faults(F_CHDIR)),
                vec![],
            )],
            "---/---",
            &vec![En("C8~1", (Nil, Z), (Nil, Z), (Dir(0), Z), vec![])],
        );
    }

    #[test]
    fn sync_no_panic_on_dir_that_fails_to_chdir_after_creation() {
        test_single(
            &vec![En(
                "D3",
                (Nil, Faults(16)),
                (Dir(0), Z),
                (Dir(0), Z),
                vec![En("D4", (Nil, Z), (Nil, Z), (Dir(0), Z), vec![])],
            )],
            "cud/cud",
            &vec![En(
                "D3",
                (Dir(0), Z),
                (Dir(0), Z),
                (Dir(0), Z),
                vec![En("D4", (Dir(0), Z), (Dir(0), Z), (Dir(0), Z), vec![])],
            )],
        );
    }

    #[test]
    fn sync_no_trust_client_unix_mode_steady_state() {
        test_single(
            &vec![
                En("foo", (Dir(0), Z), (Dir(7), Z), (Dir(7), Z), vec![]),
                En(
                    "bar",
                    (Reg(0, 9), Z),
                    (Reg(6, 9), Z),
                    (Reg(6, 9), Z),
                    vec![],
                ),
            ],
            ("cud/cud", false),
            &vec![
                En("foo", (Dir(0), Z), (Dir(7), Z), (Dir(7), Z), vec![]),
                En(
                    "bar",
                    (Reg(0, 9), Z),
                    (Reg(6, 9), Z),
                    (Reg(6, 9), Z),
                    vec![],
                ),
            ],
        );
    }

    #[test]
    fn sync_no_trust_client_unix_mode_propagates_across_changes() {
        test_single(
            &vec![En(
                "bar",
                (Reg(0, 9), Z),
                (Reg(6, 0), Z),
                (Reg(6, 0), Z),
                vec![],
            )],
            ("cud/cud", false),
            &vec![En(
                "bar",
                (Reg(0, 9), Z),
                (Reg(6, 9), Z),
                (Reg(6, 9), Z),
                vec![],
            )],
        );
    }

    prop_compose! {
        fn arb_fs_file_e()(exists in proptest::bool::ANY,
                           regular in proptest::bool::ANY,
                           mode in proptest::bits::u32::between(0, 9),
                           content in 0u8..16u8) -> FsFileE {
            if !exists {
                Nil
            } else if !regular {
                Dir(mode)
            } else {
                Reg(mode, content)
            }
        }
    }

    prop_compose! {
        fn arb_fs_file()(e in arb_fs_file_e(), faults in arb_faults())
                         -> FsFile {
            (e, faults)
        }
    }

    static NAMES: [&'static str; 200] = [
        "A0", "A0", "A2", "A3", "A4", "A5", "A6", "A7", "A8", "A9", "B0", "B0",
        "B2", "B3", "B4", "B5", "B6", "B7", "B8", "B9", "C0", "C0", "C2", "C3",
        "C4", "C5", "C6", "C7", "C8", "C9", "D0", "D0", "D2", "D3", "D4", "D5",
        "D6", "D7", "D8", "D9", "E0", "E0", "E2", "E3", "E4", "E5", "E6", "E7",
        "E8", "E9", "F0", "F0", "F2", "F3", "F4", "F5", "F6", "F7", "F8", "F9",
        "G0", "G0", "G2", "G3", "G4", "G5", "G6", "G7", "G8", "G9", "H0", "H0",
        "H2", "H3", "H4", "H5", "H6", "H7", "H8", "H9", "I0", "I0", "I2", "I3",
        "I4", "I5", "I6", "I7", "I8", "I9", "J0", "J0", "J2", "J3", "J4", "J5",
        "J6", "J7", "J8", "J9", "A0~1", "A0~1", "A2~1", "A3~1", "A4~1", "A5~1",
        "A6~1", "A7~1", "A8~1", "A9~1", "B0~1", "B0~1", "B2~1", "B3~1", "B4~1",
        "B5~1", "B6~1", "B7~1", "B8~1", "B9~1", "C0~1", "C0~1", "C2~1", "C3~1",
        "C4~1", "C5~1", "C6~1", "C7~1", "C8~1", "C9~1", "D0~1", "D0~1", "D2~1",
        "D3~1", "D4~1", "D5~1", "D6~1", "D7~1", "D8~1", "D9~1", "E0~1", "E0~1",
        "E2~1", "E3~1", "E4~1", "E5~1", "E6~1", "E7~1", "E8~1", "E9~1", "F0~1",
        "F0~1", "F2~1", "F3~1", "F4~1", "F5~1", "F6~1", "F7~1", "F8~1", "F9~1",
        "G0~1", "G0~1", "G2~1", "G3~1", "G4~1", "G5~1", "G6~1", "G7~1", "G8~1",
        "G9~1", "H0~1", "H0~1", "H2~1", "H3~1", "H4~1", "H5~1", "H6~1", "H7~1",
        "H8~1", "H9~1", "I0~1", "I0~1", "I2~1", "I3~1", "I4~1", "I5~1", "I6~1",
        "I7~1", "I8~1", "I9~1", "J0~1", "J0~1", "J2~1", "J3~1", "J4~1", "J5~1",
        "J6~1", "J7~1", "J8~1", "J9~1",
    ];

    fn arb_en() -> BoxedStrategy<En> {
        fn en() -> BoxedStrategy<En> {
            (
                (0..NAMES.len()).prop_map(|ix| NAMES[ix]),
                arb_fs_file(),
                arb_fs_file(),
                arb_fs_file(),
            )
                .prop_map(|(name, a, b, c)| En(name, a, b, c, vec![]))
                .boxed()
        }

        en().prop_recursive(4, 32, 4, |child| {
            (en(), proptest::collection::vec(child, 1..4))
                .prop_map(|(mut en, children)| {
                    en.4 = children;
                    en
                })
                .boxed()
        })
        .boxed()
    }

    fn arb_sync_mode_setting() -> BoxedStrategy<SyncModeSetting> {
        prop_oneof![
            Singleton(SyncModeSetting::Off),
            Singleton(SyncModeSetting::On),
            Singleton(SyncModeSetting::Force),
        ]
        .boxed()
    }

    prop_compose! {
        fn arb_half_sync_mode()(
            create in arb_sync_mode_setting(),
            update in arb_sync_mode_setting(),
            delete in arb_sync_mode_setting(),
        ) -> HalfSyncMode {
            HalfSyncMode { create, update, delete }
        }
    }

    prop_compose! {
        fn arb_sync_mode()(
            inbound in arb_half_sync_mode(),
            outbound in arb_half_sync_mode(),
        ) -> SyncMode {
            SyncMode { inbound, outbound }
        }
    }

    prop_compose! {
        fn arb_faults()(
            bits in proptest::bits::u8::between(0, 6)
        ) -> Faults {
            Faults(bits)
        }
    }

    fn names_unique(fs: &Vec<En>) -> bool {
        let mut names = HashSet::new();
        for en in fs {
            if !names.insert(en.0) {
                return false;
            }
            if !names_unique(&en.4) {
                return false;
            }
        }
        return true;
    }

    fn arb_fs() -> BoxedStrategy<Vec<En>> {
        proptest::collection::vec(arb_en(), 0..10)
            .prop_filter("Name uniqueness".to_owned(), names_unique)
            .boxed()
    }

    proptest! {
        #![proptest_config(proptest::test_runner::Config {
            cases: 16384,
            .. proptest::test_runner::Config::default()
        })]

        #[test]
        fn sync_converges_after_success(
            ref fs in arb_fs(), mode in arb_sync_mode()
        ) {
            let mut fx = init(&fs);
            fx.rules = constant_rules(mode, true);

            run_full(&mut fx);

            let cli_sig_a = signature(&fx.client);
            let anc_sig_a = signature(&fx.ancestor);
            let srv_sig_a = signature(&fx.server);

            fx.client.mark_all_dirty();
            fx.server.mark_all_dirty();
            run_full(&mut fx);

            let cli_sig_b = signature(&fx.client);
            let anc_sig_b = signature(&fx.ancestor);
            let srv_sig_b = signature(&fx.server);

            assert_eq!(cli_sig_a, cli_sig_b);
            assert_eq!(srv_sig_a, srv_sig_b);
            assert_eq!(anc_sig_a, anc_sig_b);
        }

        #[test]
        fn unforced_symmetric_sync_never_loses_data_and_makes_both_sides_identical(
            ref fs in arb_fs()
        ) {
            fn files_in_dir(dst: &mut HashSet<u8>, replica: &MemoryReplica,
                            dir: &mut DirHandle) {
                for (name, data) in replica.list(dir).unwrap() {
                    match data {
                        FileData::Regular(_,_,_,hash) => {
                            dst.insert(hash[0]);
                        },
                        FileData::Directory(_) =>
                            files_in_dir(dst, replica,
                                         &mut replica.chdir(dir, &name).unwrap()),
                        _ => panic!("Unexpected file data: {:?}", data),
                    }
                }
            }

            fn files_in_replica(dst: &mut HashSet<u8>, replica: &MemoryReplica) {
                replica.data().faults.clear();
                files_in_dir(dst, replica, &mut replica.root().unwrap());
            }

            fn files_in_fx(fx: &Fixture) -> (HashSet<u8>,HashSet<u8>) {
                let mut max = HashSet::new();
                files_in_replica(&mut max, &fx.client);
                files_in_replica(&mut max, &fx.server);
                let mut anc = HashSet::new();
                files_in_replica(&mut anc, &fx.ancestor);
                let mut min = max.clone();
                for a in anc { min.remove(&a); }
                (min, max)
            }

            fn files_in_slot<F : Fn (&En) -> FsFile>(
                dst: &mut HashSet<u8>, fs: &Vec<En>,
                slot: &F)
            {
                for en in fs {
                    match slot(en).0 {
                        Nil => (),
                        Reg(_, hash) => {
                            dst.insert(hash);
                        },
                        Dir(_) => files_in_slot(dst, &en.4, slot),
                    }
                }
            }

            fn files_in_fs(fs: &Vec<En>) -> (HashSet<u8>, HashSet<u8>) {
                let mut max = HashSet::new();
                let mut anc_hs = HashSet::new();
                files_in_slot(&mut max, fs, &|en| en.1);
                files_in_slot(&mut max, fs, &|en| en.3);
                // Files in the ancestor could be deleted by normal syncing, so
                // exclude them.
                files_in_slot(&mut anc_hs, fs, &|en| en.2);
                let mut min = max.clone();
                for ah in anc_hs { min.remove(&ah); }
                (max, min)
            }

            let mut fx = init(&fs);
            fx.rules = constant_rules("cud/cud".parse().unwrap(), true);

            let (orig_files_max, orig_files_min) = files_in_fs(&fs);
            run_full(&mut fx);

            let (_, result_files) = files_in_fx(&fx);

            if !result_files.is_superset(&orig_files_min) ||
               !result_files.is_subset(&orig_files_max)
            {
                panic!("Data lost or created during syncing\n\
                        Max: {:?}\n\
                        Min: {:?}\n\
                        Res: {:?}", orig_files_max, orig_files_min,
                       result_files);
            }

            let cli_sig = signature(&fx.client);
            let srv_sig = signature(&fx.server);
            if cli_sig != srv_sig {
                panic!("Client and server do not match.\n\
                        Cli: {}\n\
                        Srv: {}", cli_sig, srv_sig);
            }
        }

        #[test]
        fn mirror_to_server_never_changes_client_and_makes_server_like_client(
            ref fs in arb_fs()
        ) {
            // Need to construct a separate fixture since taking the signature
            // clears the faults.
            let orig_cli_sig = signature(&init(&fs).client);

            let mut fx = init(&fs);
            fx.rules = constant_rules("---/CUD".parse().unwrap(), true);

            run_full(&mut fx);

            let new_cli_sig = signature(&fx.client);
            let srv_sig = signature(&fx.server);

            if orig_cli_sig != new_cli_sig {
                panic!("Client modified!\n\
                        Orig: {}\n\
                        New : {}", orig_cli_sig, new_cli_sig);
            }
            if orig_cli_sig != srv_sig {
                panic!("Server doesn't match the client\n\
                        Cli: {}\n\
                        Srv: {}", orig_cli_sig, srv_sig);
            }
        }
    }
}
