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

use std::collections::{BTreeMap,HashSet};
use std::ffi::{CStr,CString};
use std::sync::atomic::{AtomicBool,AtomicUsize};
use std::sync::atomic::Ordering::SeqCst;
use std::sync::Arc;

use defs::*;
use log::{self,Log,Logger};
use replica::{Replica,ReplicaDirectory,Result};
use rules::*;
use super::context::*;
use super::compute::*;
use super::mutate::{ApplyResult,apply_reconciliation};

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

impl Drop for DirState {
    fn drop(&mut self) {
        debug_assert!(self.quiet || 0 == self.pending.load(SeqCst),
                      "DirState dropped while it still had a pending \
                       count of {}", self.pending.load(SeqCst));
    }
}

pub type DirStateRef = Arc<DirState>;

/// Processes a single file.
///
/// That is, this reads the current file, decides what to do with it, then
/// applies that resolution. If the file is a directory and we are to recurse
/// into it, queues a task to do so.
///
/// The `success` flag on `dirstate` is cleared if any operation fails. `empty`
/// on `dirstate` is cleared if the result state is clean but the file still
/// exists.
fn process_file<I : Interface>(
    i: &I,
    dir: &mut DirContext<I>,
    dir_path: &CStr, name: &CStr,
    dirstate: &DirStateRef)
{
    let cli = dir.cli.files.get(name).cloned();
    let anc = dir.anc.files.get(name).cloned();
    let srv = dir.srv.files.get(name).cloned();
    let rules = dir.rules.file(name, cli.as_ref().or(
            anc.as_ref()).or(srv.as_ref()));
    let (recon, conflict) = choose_reconciliation(
        cli.as_ref(), anc.as_ref(), srv.as_ref(),
        rules.sync_mode());

    i.log().log(
        if conflict > Conflict::NoConflict { log::WARN } else { log::INFO },
        &Log::Inspect(dir_path, name, recon, conflict));

    let res = apply_reconciliation(i, dir, dir_path, name, recon,
                                   cli.as_ref(), srv.as_ref());

    match res {
        ApplyResult::Clean(false) => {
            dirstate.empty.fetch_and(!dir.cli.files.contains_key(name) &&
                                     !dir.anc.files.contains_key(name) &&
                                     !dir.srv.files.contains_key(name),
                                     SeqCst);
        },
        ApplyResult::Fail => dirstate.success.store(false, SeqCst),
        ApplyResult::Clean(true) => {
            dirstate.empty.store(false, SeqCst);
            if i.cli().is_dir_dirty(&dir.cli.dir) ||
                i.srv().is_dir_dirty(&dir.srv.dir)
            {
                recurse_into_dir(i, dir, dir_path, name, rules,
                                 dirstate.clone());
            }
        },
        ApplyResult::RecursiveDelete(side, mode) =>
            recursive_delete(i, dir, dir_path, name, rules,
                             dirstate.clone(), side, mode),
    }
}

/// Reads a directory out of a replica and builds the initial name->data map.
///
/// If an error occurs, it is logged and returned (so that the caller can use
/// `try!`).
fn read_dir_contents<R : Replica, RB : DirRulesBuilder, LOG : Logger>(
    r: &R, dir: &mut R::Directory, dir_path: &CStr,
    mut rb: Option<&mut RB>, log: &LOG, side : log::ReplicaSide)
    -> Result<BTreeMap<CString,FileData>>
{
    let mut ret = BTreeMap::new();
    let list = match r.list(dir) {
        Ok(l) => l,
        Err(err) => {
            log.log(log::ERROR, &Log::Error(side, dir_path,
                                            log::ErrorOperation::List, &*err));
            return Err(err)
        }
    };
    for (name, value) in list {
        rb.as_mut().map(|r| r.contains(File(&name, &value)));
        ret.insert(name, value);
    }
    Ok(ret)
}

/// Processes a directory.
///
/// Reads the contents of all three replicas, constructs the directory context,
/// calls `process_file()` on each name, then invokes `on_complete_supplier` to
/// obtain the task to run when the directory is fully complete, then
/// decrements the pending by 1 (completing the directory if no children were
/// spawned).
///
/// This may return Err if it was unable to set the context up at all. Any
/// errors have already been logged; this is simply an artefact of using `try!`
/// to simplify control flow.
fn process_dir_impl<I : Interface,
                    F : FnOnce(DirContext<I>,DirStateRef) -> Task<I>>(
    i: &I,
    mut cli_dir: I::CliDir,
    mut anc_dir: I::AncDir,
    mut srv_dir: I::SrvDir,
    mut rules_builder: <I::Rules as DirRules>::Builder,
    on_complete_supplier: F) -> Result<()>
{
    let dir_path = cli_dir.full_path().to_owned();

    let cli_files = try!(read_dir_contents(
        i.cli(), &mut cli_dir, &dir_path, Some(&mut rules_builder), i.log(),
        log::ReplicaSide::Client));
    let anc_files = try!(read_dir_contents(
        i.anc(), &mut anc_dir, &dir_path,
        None::<&mut <I::Rules as DirRules>::Builder>, i.log(),
        log::ReplicaSide::Ancestor));
    let srv_files = try!(read_dir_contents(
        i.srv(), &mut srv_dir, &dir_path, Some(&mut rules_builder), i.log(),
        log::ReplicaSide::Server));
    let rules = rules_builder.build();

    let mut dir = DirContext::<I> {
        cli: SingleDirContext { dir: cli_dir, files: cli_files },
        anc: SingleDirContext { dir: anc_dir, files: anc_files },
        srv: SingleDirContext { dir: srv_dir, files: srv_files },
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
        process_file(i, &mut dir, &dir_path, &name, &dirstate);
    }

    dirstate.on_complete.store(i.tasks().put(
        on_complete_supplier(dir, dirstate.clone())), SeqCst);
    finish_task_in_dir(i, &dirstate, &dirstate);
    Ok(())
}

/// Wraps process_dir_impl() to return whether the result was successful or
/// not.
fn process_dir<I : Interface,
               F : FnOnce (DirContext<I>, DirStateRef) -> Task<I>>(
    i: &I,
    cli_dir: I::CliDir,
    anc_dir: I::AncDir,
    srv_dir: I::SrvDir,
    rules_builder: <I::Rules as DirRules>::Builder,
    on_complete_suppvier: F) -> bool
{
    process_dir_impl(i, cli_dir, anc_dir, srv_dir, rules_builder,
                     on_complete_suppvier).is_ok()
}

/// Calls `chdir()` on `r` and `parent` using `name`.
///
/// If an error occurs, it is logged and `None` is returned.
fn try_chdir<R : Replica, LOG : Logger>(r: &R, parent: &R::Directory,
                                        parent_name: &CStr,
                                        name: &CStr,
                                        log: &LOG, side: log::ReplicaSide)
                                        -> Option<R::Directory> {
    match r.chdir(parent, name) {
        Ok(dir) => Some(dir),
        Err(error) => {
            log.log(log::ERROR, &Log::Error(
                side, parent_name, log::ErrorOperation::Chdir(name), &*error));
            None
        }
    }
}

/// Queues a task to recurse into the given directory.
///
/// When the subdirectory completes, it is marked clean if its `success` flag
/// is set. The flags from the subdirectory's state are ANDed into `state`.
///
/// If switching into the subdirectory fails, `state.success` is cleared and no
/// task is enqueued. If `process_dir` returns false, `state.success` will be
/// cleared.
fn recurse_into_dir<I : Interface>(
    i: &I,
    dir: &mut DirContext<I>,
    parent_name: &CStr, name: &CStr,
    file_rules: <I::Rules as DirRules>::FileRules,
    state: DirStateRef)
{
    match (try_chdir(i.cli(), &dir.cli.dir, parent_name, name,
                     i.log(), log::ReplicaSide::Client),
           try_chdir(i.anc(), &dir.anc.dir, parent_name, name,
                     i.log(), log::ReplicaSide::Ancestor),
           try_chdir(i.srv(), &dir.srv.dir, parent_name, name,
                     i.log(), log::ReplicaSide::Server)) {
        (Some(cli_dir), Some(anc_dir), Some(srv_dir)) =>
            recurse_and_then(i, cli_dir, anc_dir, srv_dir, file_rules, state,
                             |i, dir, _| mark_both_clean(i, &dir)),

        _ => state.success.store(false, SeqCst),
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
fn recurse_and_then<I : Interface,
                    F : FnOnce (&I, DirContext<I>, &DirStateRef)
                                -> bool + 'static>(
    i: &I, cli_dir: I::CliDir, anc_dir: I::AncDir, srv_dir: I::SrvDir,
    file_rules: <I::Rules as DirRules>::FileRules,
    state: DirStateRef,
    on_success: F)
{
    state.pending.fetch_add(1, SeqCst);

    i.work().push(task(move |i| {
        let state2 = state.clone();
        let success = process_dir(
            i, cli_dir, anc_dir, srv_dir,
            file_rules.subdir(),
            |dir, subdirstate| task(move |i| {
                if subdirstate.success.load(SeqCst) {
                    subdirstate.success.fetch_and(
                        on_success(i, dir, &subdirstate), SeqCst);
                }
                finish_task_in_dir(i, &state, &subdirstate);
            }));
        if !success {
            state2.success.store(false, SeqCst);
            // process_dir() didn't create the task to decrement
            // `pending`, so we need to do that now.
            finish_task_in_dir(i, &state2, &state2);
        }
    }));
}

fn mark_both_clean<I : Interface>(i: &I, dir: &DirContext<I>)
                                  -> bool {
    let dir_path = dir.cli.dir.full_path();

    mark_clean(i.cli(), &dir.cli.dir, i.log(),
               log::ReplicaSide::Client, dir_path) &&
    mark_clean(i.srv(), &dir.srv.dir, i.log(),
               log::ReplicaSide::Server, dir_path)
}

fn mark_clean<R : Replica, L : Logger>(
    r: &R, dir: &R::Directory,
    log: &L, side: log::ReplicaSide, dir_path: &CStr) -> bool
{
    match r.set_dir_clean(dir) {
        Ok(clean) => clean,
        Err(error) => {
            log.log(log::ERROR, &Log::Error(
                side, dir_path, log::ErrorOperation::MarkClean, &*error));
            false
        }
    }
}

/// Finishes a subtask of a directory.
///
/// The flags of `state` are ANDed with those from `substate`. `state.pending`
/// is decremented; if it reaches zero, the completion task is queued.
fn finish_task_in_dir<I : Interface>(i: &I, state: &DirStateRef,
                                     substate: &DirStateRef) {
    state.success.fetch_and(substate.success.load(SeqCst), SeqCst);
    state.empty.fetch_and(substate.empty.load(SeqCst), SeqCst);
    if 1 == state.pending.fetch_sub(1, SeqCst) {
        i.work().push(i.tasks().get(state.on_complete.load(SeqCst)));
    }
}

/// Queues a task to descend into a directory for a probable recursive delete.
///
/// This is basically like `recurse_into_dir`, except that the replica opposite
/// `side` is given a synthetic directory using `mode`, as well as the ancestor
/// if necessary; on success, if all subtasks reported empty, the directories
/// on all replicas are removed. Otherwise, on success, the directory is simply
/// marked clean.
fn recursive_delete<I : Interface>(
    i: &I, dir: &mut DirContext<I>,
    parent_name: &CStr, name: &CStr,
    file_rules: <I::Rules as DirRules>::FileRules,
    state: DirStateRef,
    side: ReconciliationSide, mode: FileMode)
{
    fn chdir_or_synth<R : Replica, LOG : Logger>(
        r: &R, dir: &mut R::Directory,
        this_side: ReconciliationSide, phys_side: ReconciliationSide,
        log: &LOG,
        parent_name: &CStr, name: &CStr, mode: FileMode)
        -> Option<R::Directory>
    {
        if this_side == phys_side {
            try_chdir(r, dir, parent_name, name, log, this_side.into())
        } else {
            Some(r.synthdir(dir, name, mode))
        }
    }

    let cli_dir = chdir_or_synth(i.cli(), &mut dir.cli.dir,
                                 ReconciliationSide::Client, side,
                                 i.log(), parent_name, name, mode);
    let anc_dir = i.anc().chdir(&dir.anc.dir, name).ok().unwrap_or_else(
        || i.anc().synthdir(&mut dir.anc.dir, name, mode));
    let srv_dir = chdir_or_synth(i.srv(), &mut dir.srv.dir,
                                 ReconciliationSide::Server, side,
                                 i.log(), parent_name, name, mode);

    match (cli_dir, anc_dir, srv_dir) {
        (Some(cli_dir), anc_dir, Some(srv_dir)) =>
            recurse_and_then(i, cli_dir, anc_dir, srv_dir, file_rules, state,
                             |i, mut dir, substate| delete_or_mark_clean(
                                 i, &mut dir, substate)),

        _ => state.success.store(false, SeqCst),
    }
}

fn try_rmdir<R : Replica, LOG : Logger>(r: &R, dir: &mut R::Directory,
                                        log: &LOG, dir_path: &CStr,
                                        side: log::ReplicaSide) -> bool {
    log.log(log::EDIT, &Log::Rmdir(side, dir_path));
    match r.rmdir(dir) {
        Ok(_) => true,
        Err(error) => {
            log.log(log::ERROR, &Log::Error(
                side, dir_path, log::ErrorOperation::Rmdir, &*error));
            false
        }
    }
}

fn delete_or_mark_clean<I : Interface>(i: &I, dir: &mut DirContext<I>,
                                       state: &DirStateRef) -> bool {
    let dir_path = dir.cli.dir.full_path().to_owned();
    if state.empty.load(SeqCst) {
        try_rmdir(i.anc(), &mut dir.anc.dir, i.log(), &dir_path,
                  log::ReplicaSide::Ancestor) &&
        try_rmdir(i.cli(), &mut dir.cli.dir, i.log(), &dir_path,
                  log::ReplicaSide::Client) &&
        try_rmdir(i.srv(), &mut dir.srv.dir, i.log(), &dir_path,
                  log::ReplicaSide::Server)
    } else {
        mark_both_clean(i, dir)
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
pub fn start_root<I : Interface>(i: &I)
                                 -> Result<DirStateRef> {
    let root_state = Arc::new(DirState {
        success: AtomicBool::new(true),
        empty: AtomicBool::new(true),
        // Never gets decremented to 0
        pending: AtomicUsize::new(1),
        on_complete: AtomicUsize::new(0),
        quiet: true,
    });
    recurse_and_then(i, try!(i.cli().root()),
                     try!(i.anc().root()),
                     try!(i.srv().root()),
                     i.root_rules(), root_state.clone(),
                     |i, dir, _| mark_both_clean(i, &dir));
    Ok(root_state)
}

#[cfg(test)]
mod test {
    use std::collections::HashMap;
    use std::ffi::CString;
    use std::sync::Arc;
    use std::sync::atomic::Ordering::SeqCst;

    use defs::*;
    use replica::*;
    use memory_replica::*;
    use super::super::context::*;
    use super::super::mutate::test::*;
    use super::*;

    // Structs which describe a file tree, used for initialising and verifying
    // the replicas for tests.
    #[derive(Clone,Debug,PartialEq,Eq,PartialOrd,Ord)]
    struct En(&'static str, FsFile, FsFile, FsFile, Vec<En>);
    type FsFile = (FsFileE, Faults);
    #[derive(Clone,Copy,Debug,PartialEq,Eq,PartialOrd,Ord)]
    enum FsFileE {
        Nil,
        Reg(FileMode, u8),
        Dir(FileMode),
    }
    use self::FsFileE::*;
    type Faults = u8;
    const F_CR : Faults = 1;
    const F_UP : Faults = 2;
    const F_RM : Faults = 4;
    const F_MV : Faults = 8;
    const F_CHDIR : Faults = 16;
    const F_LS : Faults = 32;

    fn init_replica<F : Fn (&En) -> FsFile>(replica: &MemoryReplica,
                                            fs: &Vec<En>, slot: F) {
        fn populate_dir<F : Fn(&En) -> FsFile>(replica: &MemoryReplica,
                                               dir: &mut DirHandle,
                                               ls: &Vec<En>,
                                               slot: &F) {
            let dirname = dir.full_path().to_owned();

            for en in ls {
                let (f, faults) = slot(en);
                let name = CString::new(en.0).unwrap();
                match f {
                    Nil => (),
                    Reg(mode, hash) => {
                        replica.create(
                            dir, File(&name, &FileData::Regular(
                                mode, 0, 0, [hash;32])),
                            Ok([hash;32])).unwrap();
                    },
                    Dir(mode) => {
                        replica.create(
                            dir, File(&name, &FileData::Directory(mode)),
                            Ok([0;32])).unwrap();

                        let mut subdir = replica.chdir(dir, &name).unwrap();
                        populate_dir(replica, &mut subdir, &en.4, slot);
                    },
                }

                let mut to_fault = vec![];
                if 0 != faults & F_CR {
                    to_fault.push(Op::Create(
                        dirname.clone(), name.clone()));
                }
                if 0 != faults & F_UP {
                    to_fault.push(Op::Update(
                        dirname.clone(), name.clone()));
                }
                if 0 != faults & F_RM {
                    to_fault.push(Op::Remove(
                        dirname.clone(), name.clone()));
                }
                if 0 != faults & F_MV {
                    to_fault.push(Op::Rename(
                        dirname.clone(), name.clone()));
                }
                if 0 != faults & F_LS {
                    to_fault.push(Op::List(catpath(&dirname, &name)));
                }
                if 0 != faults & F_CHDIR {
                    to_fault.push(Op::Chdir(catpath(&dirname, &name)));
                }

                for tf in to_fault {
                    replica.data().faults.insert(
                        tf, Box::new(|_| simple_error()));
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

    fn verify_replica<F : Fn (&En) -> FsFile>(name: &str,
                                              replica: &MemoryReplica,
                                              fs: &Vec<En>, slot: F) {
        fn verify_dir<F : Fn (&En) -> FsFile>(name: &str,
                                              replica: &MemoryReplica,
                                              dir: &mut DirHandle,
                                              fs: &Vec<En>, slot: &F) {
            let mut files = HashMap::new();
            let dirname = dir.full_path().to_str().unwrap().to_owned();
            for (name, data) in replica.list(dir).unwrap() {
                files.insert(name.to_str().unwrap().to_owned(), data);
            }

            for en in fs {
                let data = files.remove(en.0);
                let data_en = match data {
                    Some(FileData::Regular(mode, _, _, hash)) => {
                        assert_eq!([hash[0];32], hash);
                        Reg(mode, hash[0])
                    },
                    Some(FileData::Directory(mode)) => Dir(mode),
                    None => Nil,
                    _ => panic!("{}: {}/{} unexpected file type: {:?}",
                                name, dirname, en.0, &data),
                };
                if data_en != slot(en).0 {
                    panic!("{}: {}/{} expected {:?}, got {:?}",
                           name, dirname, en.0, slot(en).0, data_en);
                }

                if let Dir(_) = slot(en).0 {
                    verify_dir(name, replica,
                               &mut replica.chdir(
                                   dir, &CString::new(en.0).unwrap()).unwrap(),
                               &en.4, slot);
                }
            }

            if let Some(unexpected) = files.keys().next() {
                panic!("{}: {}/{} not expected to exist, but it does",
                       name, dirname, unexpected);
            }
        }

        verify_dir(name, replica, &mut replica.root().unwrap(),
                   fs, &slot);
    }

    fn verify(fx: &Fixture, fs: &Vec<En>) {
        verify_replica("client", &fx.client, fs, |t| t.1);
        verify_replica("server", &fx.server, fs, |t| t.3);
        verify_replica("ancestor", &fx.ancestor, fs, |t| t.2);
    }

    fn run_once(fx: &Fixture) -> DirState {
        let context = fx.context();
        let dsr = start_root(&context).unwrap();
        context.run_work();
        Arc::try_unwrap(dsr).ok().unwrap()
    }

    fn run_full(fx: &Fixture) {
        let res = run_once(fx);
        if !res.success.load(SeqCst) {
            assert!(!fx.client.data().faults.is_empty() ||
                    !fx.ancestor.data().faults.is_empty() ||
                    !fx.server.data().faults.is_empty(),
                    "Result had error even though there were no faults");
            // Clear all faults and let it run again
            fx.client.data().faults.clear();
            fx.ancestor.data().faults.clear();
            fx.server.data().faults.clear();

            let res2 = run_once(fx);
            assert!(res2.success.load(SeqCst),
                    "After clearing faults and rerunning, \
                     errors still occurred");
        }

        // TODO: Test that another pass is idempotent
    }

    fn test_single(input: &Vec<En>, mode: &str, output: &Vec<En>) {
        let mut fx = init(input);
        fx.mode = mode.parse().unwrap();
        run_full(&fx);
        verify(&fx, output);
    }

    #[test]
    fn sync_empty() {
        test_single(&vec![], "---/---", &vec![]);
    }

    #[test]
    fn sync_flat() {
        test_single(&vec![
            En("foo", (Reg(7,1),0), (Nil,0),      (Nil,0),      vec![]),
            En("bar", (Nil,0),      (Nil,0),      (Reg(6,2),0), vec![]),
            En("baz", (Nil,0),      (Reg(7,2),0), (Reg(7,2),0), vec![]),
        ], "cud/cud", &vec![
            En("foo", (Reg(7,1),0), (Reg(7,1),0), (Reg(7,1),0), vec![]),
            En("bar", (Reg(6,2),0), (Reg(6,2),0), (Reg(6,2),0), vec![]),
        ]);
    }

    #[test]
    fn sync_recursive() {
        test_single(&vec![
            En("d1" , (Dir(7),0),   (Nil,0),      (Nil,0),      vec![
                En("f1a", (Reg(7,1),0), (Nil,0),      (Nil,0),      vec![]),
            ]),
            En("orp", (Nil,0),      (Dir(7),0),   (Nil,0),      vec![
                En("o1" , (Nil,0),      (Reg(7,2),0), (Nil,0),      vec![]),
            ]),
            En("d2" , (Nil,0),      (Nil,0),      (Dir(6),0),   vec![
                En("f2a", (Nil,0),      (Nil,0),      (Reg(6,3),0), vec![]),
            ]),
        ], "cud/cud", &vec![
            En("d1" , (Dir(7),0),   (Dir(7),0),   (Dir(7),0),   vec![
                En("f1a", (Reg(7,1),0), (Reg(7,1),0), (Reg(7,1),0), vec![]),
            ]),
            En("d2" , (Dir(6),0),   (Dir(6),0),   (Dir(6),0),   vec![
                En("f2a", (Reg(6,3),0), (Reg(6,3),0), (Reg(6,3),0), vec![]),
            ]),
        ]);
    }

    #[test]
    fn sync_recursive_delete_complete() {
        test_single(&vec![
            En("a", (Dir(7),0), (Dir(7),0), (Nil,0), vec![
                En("af", (Reg(7,1),0), (Reg(7,1), 0), (Nil,0), vec![]),
                En("b", (Dir(7),0), (Dir(7),0), (Nil,0), vec![
                    En("bf", (Reg(7,2),0), (Reg(7,2), 0), (Nil,0), vec![]),
                ]),
            ]),
        ], "cud/cud", &vec![
        ]);
    }

    #[test]
    fn sync_recursive_delete_interrupted() {
        test_single(&vec![
            En("a", (Dir(7),0), (Dir(7),0), (Nil,0), vec![
                En("af", (Reg(7,1),0), (Reg(7,1), 0), (Nil,0), vec![]),
                En("b", (Dir(7),0), (Dir(7),0), (Nil,0), vec![
                    En("bf", (Reg(7,2),0), (Reg(7,2), 0), (Nil,0), vec![]),
                ]),
                En("af2", (Reg(7,2),0), (Nil,0), (Nil,0), vec![]),
            ]),
        ], "cud/cud", &vec![
            En("a", (Dir(7),0), (Dir(7),0), (Dir(7),0), vec![
                En("af2", (Reg(7,2),0), (Reg(7,2),0), (Reg(7,2),0), vec![]),
            ]),
        ]);
    }

    #[test]
    fn sync_edit_conflict() {
        test_single(&vec![
            En("foo", (Reg(7,1),0), (Reg(7,2),0), (Reg(7,3),0), vec![]),
        ], "cud/cud", &vec![
            En("foo",   (Reg(7,1),0), (Reg(7,1),0), (Reg(7,1),0), vec![]),
            En("foo~1", (Reg(7,3),0), (Reg(7,3),0), (Reg(7,3),0), vec![]),
        ]);
    }

    #[test]
    fn sync_replace_dir_with_reg() {
        test_single(&vec![
            En("foo", (Dir(7),0), (Dir(7),0), (Reg(6,1),0), vec![
                En("a", (Reg(6,2),0), (Reg(6,2),0), (Nil,0), vec![]),
            ]),
        ], "cud/cud", &vec![
            En("foo", (Reg(6,1),0), (Reg(6,1),0), (Reg(6,1),0), vec![]),
        ]);
    }
}
