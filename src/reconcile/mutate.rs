//-
// Copyright (c) 2016, 2017, Jason Lingle
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

use std::collections::BTreeMap;
use std::ffi::{OsStr,OsString};
use std::result;

use defs::*;
use errors::*;
use log;
use log::{Logger,ReplicaSide,ErrorOperation,Log};
use replica::{Replica, NullTransfer};
use super::compute::{Reconciliation,ReconciliationSide,gen_alternate_name};
use super::compute::SplitAncestorState;
use super::context::*;

/// Updates the ancestor replica so that the file identified by `name` is
/// changed from the state `old` to the state `new`.
///
/// On success, `files` is updated accordinly.
///
/// This automatically handles issues like needing to recursively remove
/// directories if `old` is an existing directory and `new` is not.
fn replace_ancestor<A : Replica + NullTransfer>(
    replica: &A, in_dir: &mut A::Directory,
    files: &mut BTreeMap<OsString,FileData>,
    name: &OsStr,
    old: Option<&FileData>, new: Option<&FileData>)
    -> Result<()>
{
    fn remove_recursively<A : Replica + NullTransfer>(
        replica: &A, dir: &mut A::Directory)
        -> Result<()>
    {
        for (name, fd) in try!(replica.list(dir)) {
            if let FileData::Directory(_) = fd {
                try!(remove_recursively(
                    replica, &mut try!(replica.chdir(dir, &name))));
            }
            try!(replica.remove(dir, File(&name, &fd)));
        }
        Ok(())
    }

    // We can't use == because of different lifetime parameters, apparently.
    if old.eq(&new) {
        return Ok(())
    }

    // If old is a directory and new isn't, recursively remove old
    if is_dir(old) && !is_dir(new) {
        try!(remove_recursively(
            replica, &mut try!(replica.chdir(&*in_dir, name))));
    }

    match (old, new) {
        (None, None) => (),
        (Some(oldfd), None) => {
            try!(replica.remove(in_dir, File(name, oldfd)));
            files.remove(name).expect(
                "Deleted ancestor not in table?");
        },
        (None, Some(newfd)) => {
            try!(replica.create(in_dir, File(name, newfd),
                                A::null_transfer(newfd)));
            files.insert(name.to_owned(), newfd.clone())
                .map(|_| panic!("Inserted ancestor already in table?"));
        },
        (Some(oldfd), Some(newfd)) => {
            try!(replica.update(in_dir, name, oldfd, newfd,
                                A::null_transfer(newfd)));
            files.insert(name.to_owned(), newfd.clone())
                .expect("Updated ancestor not in table?");
        },
    }

    Ok(())
}

trait ResultToBoolExt {
    type Error;

    fn to_bool<F: FnOnce (Self::Error)>(self, f: F) -> bool;
}

impl<E> ResultToBoolExt for result::Result<(),E> {
    type Error = E;

    fn to_bool<F: FnOnce(E)>(self, f: F) -> bool {
        self.map(|_| true).unwrap_or_else(|error| {
            f(error);
            false
        })
    }
}

/// Like `replace_ancestor()`, but handles errors by sending them to the
/// logger.
///
/// Returns whether successful.
fn try_replace_ancestor<A : Replica + NullTransfer, LOG : Logger>(
    replica: &A, in_dir: &mut A::Directory,
    files: &mut BTreeMap<OsString,FileData>,
    dir_name: &OsStr, name: &OsStr,
    old: Option<&FileData>, new: Option<&FileData>,
    log: &LOG) -> bool
{
    replace_ancestor(replica, in_dir, files, name, old, new)
        .to_bool(|error| log.log(
            error.level(), &Log::Error(
                ReplicaSide::Ancestor, dir_name,
                ErrorOperation::Update(name), &error)))
}

/// Updates an end replica as necessary.
///
/// The state of the file identified by `name` within `dst_dir` will be
/// transitioned from state `old` to `new`, transferring from the same file in
/// `src_dir` in `src` if needed.
///
/// Logs about activities and errors are recorded, reporting `side` as the
/// replica being mutated.
///
/// On success, `dst_files` is updated according to the update.
///
/// Note that this does not handle removal or replacement of non-empty
/// directories itself; such operations will simply fail.
///
/// Returns the actual new state of the file on success.
fn replace_replica<DST : Replica,
                   SRC : Replica<TransferOut = DST::TransferIn>,
                   LOG : Logger>(
    dst: &DST, dst_dir: &mut DST::Directory,
    dst_files: &mut BTreeMap<OsString,FileData>,
    src: &SRC, src_dir: &SRC::Directory,
    dir_name: &OsStr, name: &OsStr,
    old: Option<&FileData>, new: Option<&FileData>,
    log: &LOG, side: ReplicaSide)
    -> Result<Option<FileData>>
{
    if old.eq(&new) {
        return Ok(old.cloned());
    }

    match (old, new) {
        (None, None) => Ok(None),
        (Some(oldfd), None) => {
            // Deletion.
            log.log(log::EDIT, &Log::Remove(side, dir_name, name, oldfd));
            match dst.remove(dst_dir, File(name, oldfd)) {
                Ok(_) => {
                    dst_files.remove(name).expect(
                        "Deleted file not in table?");
                    Ok(None)
                },
                Err(error) => {
                    log.log(error.level(), &Log::Error(
                        side, dir_name, ErrorOperation::Remove(name),
                        &error));
                    Err(error)
                }
            }
        },
        (Some(oldfd), Some(newfd)) => {
            // Update.
            log.log(log::EDIT, &Log::Update(side, dir_name, name,
                                            oldfd, newfd));
            match dst.update(dst_dir, name, oldfd, newfd,
                             src.transfer(src_dir, File(name, newfd))?) {
                Ok(r) => {
                    // We need to insert r, not newfd, because newfd may be
                    // out-of-date.
                    dst_files.insert(name.to_owned(), r.clone())
                        .expect("Updated file not in table?");
                    Ok(Some(r))
                },
                Err(error) => {
                    log.log(error.level(), &Log::Error(
                        side, dir_name, ErrorOperation::Update(name),
                        &error));
                    Err(error)
                }
            }
        },
        (None, Some(newfd)) => {
            // Insertion.
            log.log(log::EDIT, &Log::Create(side, dir_name, name, newfd));
            match dst.create(dst_dir, File(name, newfd),
                             src.transfer(src_dir, File(name, newfd))?) {
                Ok(r) => {
                    // We need to insert r, not newfd, because newfd may be
                    // out-of-date.
                    dst_files.insert(name.to_owned(), r.clone())
                        .map(|_| panic!("Inserted file already in table?"));
                    Ok(Some(r))
                },
                Err(error) => {
                    log.log(error.level(), &Log::Error(
                        side, dir_name, ErrorOperation::Create(name),
                        &error));
                    Err(error)
                }
            }
        }
    }
}

/// Renames a file on a replica.
///
/// The file identified by `old_name` is renamed to `new_name` within `dir` in
/// `replica`. When successful, `files` is updated accordingly.
///
/// Information and errors are sent on `log` using the given `side`.
///
/// Returns whether successful.
fn try_rename_replica<A : Replica, LOG : Logger>(
    replica: &A, dir: &mut A::Directory,
    files: &mut BTreeMap<OsString,FileData>,
    dir_name: &OsStr, old_name: &OsStr, new_name: &OsStr,
    log: &LOG, side: ReplicaSide) -> bool
{
    log.log(log::EDIT, &Log::Rename(
        side, dir_name, old_name, new_name));
    match replica.rename(dir, old_name, new_name) {
        Ok(_) => {
            let removed = files.remove(old_name).expect(
                "Renamed non-existent file?");
            files.insert(new_name.to_owned(), removed);
            true
        },
        Err(error) => {
            log.log(error.level(), &Log::Error(
                side, dir_name, ErrorOperation::Rename(old_name), &error));
            false
        }
    }
}

/// Return type from `apply_reconciliation()`.
#[derive(Clone,Copy,Debug,PartialEq,Eq)]
pub enum ApplyResult {
    /// Any changes needed have been applied. The field indicates whether the
    /// final file is a directory and should be entered recursively.
    Clean(bool),
    /// Changes have failed. Do not mark parent directories clean.
    Fail,
    /// A directory on the identified side is being (probably) recursively
    /// deleted. Recurse, creating a synthetic directory on the opposite end
    /// replica. After recursion, if the directories are empty, remove them in
    /// the ancestor and this end replica.
    ///
    /// Should synthetic directories be created, they shall use the mode
    /// indicated.
    RecursiveDelete(ReconciliationSide, FileMode),
}

fn apply_use<DST : Replica,
             ANC : Replica + NullTransfer,
             SRC : Replica<TransferOut = DST::TransferIn>,
             LOG : Logger>
    (dst: &DST, dst_dir: &mut DST::Directory,
     dst_files: &mut BTreeMap<OsString,FileData>,
     anc: &ANC, anc_dir: &mut ANC::Directory,
     anc_files: &mut BTreeMap<OsString,FileData>,
     src: &SRC, src_dir: &SRC::Directory,
     dir_name: &OsStr, name: &OsStr,
     old_dst: Option<&FileData>, old_src: Option<&FileData>,
     log: &LOG, side: ReconciliationSide)
    -> result::Result<Option<Option<FileData>>, ApplyResult>
{
    fn dir_mode(v: Option<&FileData>) -> FileMode {
        if let Some(&FileData::Directory(mode)) = v {
            mode
        } else {
            panic!("dir_mode() called on non-directory")
        }
    }

    // If we're deleting a directory, we need to recurse first and let
    // reconciliation empty it naturally.
    if is_dir(old_dst) && old_src.is_none() {
        // Ensure the ancestor is a directory matching the side we're going to
        // delete, so (a) we can chdir into it; (b) should we end up
        // propagating files out of the recursive "delete", there's something
        // to hold them.
        let old_anc = anc_files.get(name).cloned();
        if try_replace_ancestor(anc, anc_dir, anc_files, dir_name, name,
                                old_anc.as_ref(), old_dst, log) {
            Err(ApplyResult::RecursiveDelete(side, dir_mode(old_dst)))
        } else {
            Err(ApplyResult::Fail)
        }
    } else {
        // Other than the directory deletion case, the compute stage is
        // supposed to never give us Use(x) which replaces a directory with
        // a non-directory.
        assert!(is_dir(old_src) || !is_dir(old_dst));
        // Do the actual replacement.
        match replace_replica(
            dst, dst_dir, dst_files, src, src_dir,
            dir_name, name, old_dst, old_src,
            log, side.into())
        {
            Ok(r) => Ok(Some(r)),
            Err(_) => return Err(ApplyResult::Fail),
        }
    }
}

def_context_impl! {
/// Applies the determined reconciliation for one file.
///
/// Returns whether recursion is necessary to process this file, and if so,
/// what should happen afterwards.
///
/// Errors are passed down the context's logger.
pub fn apply_reconciliation(
    &self, dir: &mut dir_ctx!(),
    dir_name: &OsStr, name: &OsStr,
    recon: Reconciliation,
    old_cli: Option<&FileData>, old_srv: Option<&FileData>)
    -> ApplyResult
{
    use super::compute::Reconciliation::*;

    if self.should_stop() {
        return ApplyResult::Fail;
    }

    // Mutate the replicas as appropriate and determine what the new state of
    // the ancestor should be.
    //
    // There are several reasons we need to do the end replicas first:
    //
    // - This is the only way we can learn the *actual* files we sync; the
    //   hashes on the files we hold right now may be out of date.
    //
    // - If we crash after mutating the end replica but before the ancestor, we
    //   simply have out-of-date information in the ancestor, which at worst
    //   leads to spurious conflicts or resurrections. On the other hand,
    //   writing the ancestor first would result in *reversing* the
    //   reconciliation on the next run if mutating the end replica failed.
    //
    // If no ancestor is to be written, we put None here instead. This may
    // occur due to errors, because the reconciliation does not cause the
    // replicas to be in-sync, or due to some other special cases.
    //
    // Whether we write the ancestor also controls whether we recurse, simply
    // because there is no case in which recursion would be meaningful if
    // writing the ancestor isn't.
    let set_ancestor = match recon {
        // If already in-sync, we just need to ensure the ancestor matches.
        // Whether we choose old_cli or old_srv is mostly moot.
        InSync => Some(old_cli.cloned()),
        // For Unsync and Irreconcilable, the end replicas do not agree and
        // will not agree, so it doesn't make sense to change the ancestor at
        // all or to recurse.
        Unsync | Irreconcilable => None,
        // For the Use(x) cases, we propagate the change, and use the resulting
        // FileData for the ancestor.
        Use(ReconciliationSide::Client) =>
            match apply_use(&self.srv, &mut dir.srv.dir, &mut dir.srv.files,
                            &self.anc, &mut dir.anc.dir, &mut dir.anc.files,
                            &self.cli, &dir.cli.dir,
                            dir_name, name, old_srv, old_cli,
                            &self.log, ReconciliationSide::Server) {
                Ok(r) => r,
                Err(r) => return r,
            },
        Use(ReconciliationSide::Server) =>
            match apply_use(&self.cli, &mut dir.cli.dir, &mut dir.cli.files,
                            &self.anc, &mut dir.anc.dir, &mut dir.anc.files,
                            &self.srv, &dir.srv.dir,
                            dir_name, name, old_cli, old_srv,
                            &self.log, ReconciliationSide::Client) {
                Ok(r) => r,
                Err(r) => return r,
            },

        // The special Split case.
        //
        // We never write an ancestor external to this branch, because this
        // reconciliation doesn't actually reconcile anything. Rather, if the
        // renaming succeeds, it requeues both sides of the split for another
        // pass through reconciliation.
        Split(side, anc_state) => {
            let new_name = gen_alternate_name(name, |n| dir.name_in_use(n));

            // Instead of renaming the ancestor, effectively remove it, then
            // rename the end replica, and then recreate the ancestor if it was
            // to be renamed.
            //
            // For the "renaming the ancestor" case, we actually condemn the
            // old name, rename the end replica, then rename the ancestor and
            // uncondemn the old name; thus, effectively, the ancestor will
            // cease to exist if we crash in this process.
            //
            // This guarantees that at no point will the ancestor be associated
            // with a lone file in a way that could result in deleting an
            // unrelated file. That is, if we renamed the ancestor first, the
            // rename on the end replica could fail because something exists
            // with that name; but now when that file is next reconciled, it
            // looks like it was deleted, and the data is lost. But if we
            // renamed the end replica first, it would be event worse, since we
            // could leave the *possibly matching* ancestor associated with the
            // old file, causing the next sync to delete it incorrectly.
            let old_ancestor = dir.anc.files.get(name).cloned();

            let ok = match anc_state {
                SplitAncestorState::Delete => try_replace_ancestor(
                    &self.anc, &mut dir.anc.dir, &mut dir.anc.files,
                    dir_name, name, old_ancestor.as_ref(), None, &self.log),

                SplitAncestorState::Move =>
                    self.anc.condemn(&mut dir.anc.dir, name).to_bool(
                        |error|  self.log.log(error.level(), &Log::Error(
                            ReplicaSide::Ancestor, dir_name,
                            ErrorOperation::Access(name), &error))),
            }

            && match side {
                ReconciliationSide::Client => try_rename_replica(
                    &self.cli, &mut dir.cli.dir, &mut dir.cli.files,
                    dir_name, name, &new_name, &self.log, ReplicaSide::Client),

                ReconciliationSide::Server => try_rename_replica(
                    &self.srv, &mut dir.srv.dir, &mut dir.srv.files,
                    dir_name, name, &new_name, &self.log, ReplicaSide::Server),
            }

            && match anc_state {
                SplitAncestorState::Delete => true,

                SplitAncestorState::Move => {
                    (!dir.anc.files.contains_key(name) ||
                     try_rename_replica(
                         &self.anc, &mut dir.anc.dir, &mut dir.anc.files,
                         dir_name, name, &new_name, &self.log,
                         ReplicaSide::Ancestor))

                    && self.anc.uncondemn(&mut dir.anc.dir, name).to_bool(
                        |error| self.log.log(error.level(), &Log::Error(
                            ReplicaSide::Ancestor, dir_name,
                            ErrorOperation::Access(name), &error)))
                },
            };

            // If everything worked out, reprocess the now split files
            if ok {
                dir.todo.push(Reversed(name.to_owned()));
                dir.todo.push(Reversed(new_name));
            } else {
                return ApplyResult::Fail;
            }

            None
        },
    };

    // Check whether we should write back to the ancestor replica
    if let Some(new_ancestor) = set_ancestor {
        let old = dir.anc.files.get(name).cloned();
        // Try to update the ancestor.
        let ok = try_replace_ancestor(
            &self.anc, &mut dir.anc.dir, &mut dir.anc.files,
            dir_name, name, old.as_ref(), new_ancestor.as_ref(), &self.log);

        // Only recurse if writing the ancestor succeeded and the ancestor is a
        // directory. Testing the ancestor is sufficient to know that all three
        // branches are directories, since we only write the ancestor if it
        // agrees with the client and server replicas.
        if !ok {
            ApplyResult::Fail
        } else {
            ApplyResult::Clean(is_dir(new_ancestor.as_ref()))
        }
    } else {
        // Not syncing
        ApplyResult::Clean(false)
    }
} }

#[cfg(test)]
pub mod test {
    use std::collections::{BTreeMap,BinaryHeap};
    use std::ffi::{OsString,OsStr};
    use std::io::Write;
    use std::mem;

    use defs::*;
    use defs::test_helpers::*;
    use work_stack::WorkStack;
    use log::{PrintlnLogger,ReplicaSide,PrintWriter};
    use replica::{Replica,NullTransfer};
    use memory_replica::{self,MemoryReplica,DirHandle,simple_error};
    use rules::*;

    use super::super::compute::{Reconciliation,ReconciliationSide};
    use super::super::compute::{SplitAncestorState};
    use super::*;
    use super::{replace_ancestor,replace_replica,try_rename_replica};
    #[allow(unused_imports)] use super::super::context::*;

    #[derive(Clone,Debug)]
    pub struct ConstantRules(SyncMode);

    impl DirRules for ConstantRules {
        type Builder = Self;
        type FileRules = Self;

        fn file(&self, _: File) -> Self { self.clone() }
    }

    impl FileRules for ConstantRules {
        type DirRules = Self;

        fn sync_mode(&self) -> SyncMode { self.0 }
        fn subdir(self) -> Self { self }
    }

    impl DirRulesBuilder for ConstantRules {
        type DirRules = Self;

        fn contains(&mut self, _: File) { }
        fn build(self) -> Self { self }
    }

    pub type TContext<W> =
        Context<MemoryReplica, MemoryReplica, MemoryReplica,
                PrintlnLogger<W>, ConstantRules>;
    pub type TDirContext =
        DirContext<<MemoryReplica as Replica>::Directory,
                   <MemoryReplica as Replica>::Directory,
                   <MemoryReplica as Replica>::Directory,
                   ConstantRules>;

    pub struct Fixture<W> {
        pub client: MemoryReplica,
        pub ancestor: MemoryReplica,
        pub server: MemoryReplica,
        pub logger: PrintlnLogger<W>,
        pub mode: SyncMode,
    }

    impl<W : Write> Fixture<W> {
        pub fn new(out: W) -> Self {
            Fixture {
                client: MemoryReplica::empty(),
                ancestor: MemoryReplica::empty(),
                server: MemoryReplica::empty(),
                logger: PrintlnLogger::new(out),
                mode: Default::default(),
            }
        }

        pub fn context(self) -> TContext<W> {
            Context {
                cli: self.client,
                anc: self.ancestor,
                srv: self.server,
                log: self.logger,
                root_rules: ConstantRules(self.mode),
                work: WorkStack::new(),
                tasks: UnqueuedTasks::new(),
            }
        }

        pub fn with_context<T, F : FnOnce (&TContext<W>) -> T>
            (&mut self, f: F) -> T
        {
            let context = Context {
                cli: mem::replace(&mut self.client, MemoryReplica::empty()),
                anc: mem::replace(&mut self.ancestor, MemoryReplica::empty()),
                srv: mem::replace(&mut self.server, MemoryReplica::empty()),
                log: self.logger.clone(),
                root_rules: ConstantRules(self.mode),
                work: WorkStack::new(),
                tasks: UnqueuedTasks::new(),
            };
            let ret = f(&context);
            self.client = context.cli;
            self.ancestor = context.anc;
            self.server = context.srv;
            ret
        }
    }

    pub fn replica() -> (MemoryReplica, DirHandle) {
        let mr = MemoryReplica::empty();
        let root = mr.root().unwrap();
        (mr, root)
    }

    pub fn root_dir_context<W : Write>(fx: &Fixture<W>)
                                       -> TDirContext {
        DirContext {
            cli: SingleDirContext {
                dir: fx.client.root().unwrap(),
                files: BTreeMap::new(),
            },
            anc: SingleDirContext {
                dir: fx.ancestor.root().unwrap(),
                files: BTreeMap::new(),
            },
            srv: SingleDirContext {
                dir: fx.server.root().unwrap(),
                files: BTreeMap::new(),
            },
            todo: BinaryHeap::new(),
            rules: ConstantRules(fx.mode),
        }
    }

    pub fn genreg(replica: &mut MemoryReplica) -> FileData {
        FileData::Regular(0o777, 0, 0, replica.gen_hash())
    }

    pub fn mkreg(replica: &mut MemoryReplica, dir: &mut DirHandle,
                 files: &mut BTreeMap<OsString,FileData>,
                 name: &OsStr) -> FileData {
        let fd = genreg(replica);
        replica.create(dir, File(name, &fd), MemoryReplica::null_transfer(&fd))
            .unwrap();
        files.insert(name.to_owned(), fd.clone());
        fd
    }

    pub fn mksym(replica: &MemoryReplica, dir: &mut DirHandle,
                 files: &mut BTreeMap<OsString,FileData>,
                 name: &OsStr, target: &str) -> FileData {
        let fd = FileData::Symlink(oss(target));
        replica.create(dir, File(name, &fd), MemoryReplica::null_transfer(&fd))
            .unwrap();
        files.insert(name.to_owned(), fd.clone());
        fd
    }

    pub fn mkspec(replica: &MemoryReplica, dir: &mut DirHandle,
                  files: &mut BTreeMap<OsString,FileData>,
                  name: &OsStr) -> FileData {
        let fd = FileData::Special;
        replica.create(dir, File(name, &fd), MemoryReplica::null_transfer(&fd))
            .unwrap();
        files.insert(name.to_owned(), fd.clone());
        fd
    }

    pub fn mkdir(replica: &MemoryReplica, dir: &mut DirHandle,
                 files: &mut BTreeMap<OsString,FileData>,
                 name: &OsStr, mode: FileMode) -> FileData {
        let fd = FileData::Directory(mode);
        replica.create(dir, File(name, &fd),
                       MemoryReplica::null_transfer(&fd)).unwrap();
        files.insert(name.to_owned(), fd.clone());
        fd
    }

    #[test]
    fn replace_ancestor_trivial() {
        let (replica, mut root) = replica();
        let mut files = BTreeMap::new();

        replace_ancestor(&replica, &mut root, &mut files,
                         &oss("foo"), None, None).unwrap();
        assert!(files.is_empty());
    }

    #[test]
    fn replace_ancestor_create_regular() {
        let (mut replica, mut root) = replica();
        let mut files = BTreeMap::new();

        let fd = genreg(&mut replica);
        let name = oss("foo");
        replace_ancestor(&replica, &mut root, &mut files,
                         &name, None, Some(&fd)).unwrap();
        assert_eq!(1, replica.list(&mut root).unwrap().len());
        assert_eq!(Some(&fd), files.get(&name));
    }

    #[test]
    fn replace_ancestor_replace_regular() {
        let (mut replica, mut root) = replica();
        let mut files = BTreeMap::new();

        let name = oss("foo");
        let fd1 = mkreg(&mut replica, &mut root, &mut files, &name);
        let fd2 = genreg(&mut replica);

        replace_ancestor(&replica, &mut root, &mut files,
                         &name, Some(&fd1), Some(&fd2)).unwrap();
        assert_eq!(1, files.len());
        assert_eq!(Some(&fd2), files.get(&name));
        assert_eq!(fd2, replica.list(&mut root).unwrap()[0].1);
    }

    #[test]
    fn replace_ancestor_remove_regular() {
        let (mut replica, mut root) = replica();
        let mut files = BTreeMap::new();
        let name = oss("foo");
        let fd = mkreg(&mut replica, &mut root, &mut files, &name);

        assert!(files.contains_key(&name));
        replace_ancestor(&replica, &mut root, &mut files,
                         &name, Some(&fd), None).unwrap();
        assert!(!files.contains_key(&name));
        assert!(replica.list(&mut root).unwrap().is_empty());
    }

    #[test]
    fn replace_ancestor_delete_dir_tree() {
        let (mut replica, mut root) = replica();
        let mut files = BTreeMap::new();
        let foo = oss("foo");
        let bar = oss("bar");
        let plugh = oss("plugh");

        let fd_foo = mkdir(&replica, &mut root, &mut files, &foo, 0o777);
        let mut subdir_foo = replica.chdir(&root, &foo).unwrap();
        mkdir(&replica, &mut subdir_foo, &mut BTreeMap::new(), &bar, 0o777);
        let mut subdir_bar = replica.chdir(&subdir_foo, &bar).unwrap();
        mkreg(&mut replica, &mut subdir_bar, &mut BTreeMap::new(), &plugh);

        replace_ancestor(&replica, &mut root, &mut files, &foo,
                         Some(&fd_foo), None).unwrap();
        assert!(!files.contains_key(&foo));
        assert!(replica.list(&mut root).unwrap().is_empty());
    }

    #[test]
    fn replace_ancestor_replace_dir_tree_with_file() {
        let (mut replica, mut root) = replica();
        let mut files = BTreeMap::new();
        let foo = oss("foo");
        let bar = oss("bar");
        let plugh = oss("plugh");
        let fd2 = genreg(&mut replica);

        let fd_foo = mkdir(&replica, &mut root, &mut files, &foo, 0o777);
        let mut subdir_foo = replica.chdir(&root, &foo).unwrap();
        mkdir(&replica, &mut subdir_foo, &mut BTreeMap::new(), &bar, 0o777);
        let mut subdir_bar = replica.chdir(&subdir_foo, &bar).unwrap();
        mkreg(&mut replica, &mut subdir_bar, &mut BTreeMap::new(), &plugh);

        replace_ancestor(&replica, &mut root, &mut files, &foo,
                         Some(&fd_foo), Some(&fd2)).unwrap();
        assert_eq!(Some(&fd2), files.get(&foo));
        assert!(replica.list(&mut subdir_foo).is_err());
    }

    #[test]
    fn replace_ancestor_chmod_dir() {
        let (mut replica, mut root) = replica();
        let mut files = BTreeMap::new();
        let foo = oss("foo");
        let bar = oss("bar");
        let plugh = oss("plugh");
        let fd2 = FileData::Directory(0o700);

        let fd_foo = mkdir(&replica, &mut root, &mut files, &foo, 0o777);
        let mut subdir_foo = replica.chdir(&root, &foo).unwrap();
        mkdir(&replica, &mut subdir_foo, &mut BTreeMap::new(), &bar, 0o777);
        let mut subdir_bar = replica.chdir(&subdir_foo, &bar).unwrap();
        mkreg(&mut replica, &mut subdir_bar, &mut BTreeMap::new(), &plugh);

        replace_ancestor(&replica, &mut root, &mut files, &foo,
                         Some(&fd_foo), Some(&fd2)).unwrap();
        assert_eq!(Some(&fd2), files.get(&foo));
        assert!(replica.list(&mut subdir_foo).is_ok());
        assert_eq!(plugh, replica.list(&mut subdir_bar).unwrap()[0].0);
    }

    #[test]
    fn replace_ancestor_replace_regular_with_dir() {
        let (mut replica, mut root) = replica();
        let mut files = BTreeMap::new();
        let foo = oss("foo");

        let fd1 = mkdir(&replica, &mut root, &mut files, &foo, 0o777);
        let fd2 = genreg(&mut replica);

        replace_ancestor(&replica, &mut root, &mut files, &foo,
                         Some(&fd1), Some(&fd2)).unwrap();
        assert_eq!(Some(&fd2), files.get(&foo));
        assert_eq!(fd2, replica.list(&mut root).unwrap()[0].1);
    }

    #[test]
    fn replace_replica_noop() {
        let (mut dst, mut dst_root) = replica();
        let (mut src, mut src_root) = replica();
        let mut files = BTreeMap::new();
        let foo = oss("foo");

        let fd = mkreg(&mut dst, &mut dst_root, &mut files, &foo);
        let fd2 = mkreg(&mut src, &mut src_root, &mut BTreeMap::new(), &foo);
        assert_eq!(fd, fd2);

        dst.data().faults.insert(
            memory_replica::Op::Update(oss(""), foo.clone()),
            Box::new(|_| simple_error()));
        assert_eq!(Some(fd.clone()), replace_replica(
            &dst, &mut dst_root, &mut files, &src, &mut src_root,
            &oss(""), &foo, Some(&fd), Some(&fd2),
            &PrintlnLogger::stdout(), ReplicaSide::Server).unwrap());
    }

    #[test]
    fn replace_replica_create_regular() {
        let (dst, mut dst_root) = replica();
        let (mut src, mut src_root) = replica();
        let mut files = BTreeMap::new();
        let foo = oss("foo");

        let fd = mkreg(&mut src, &mut src_root, &mut BTreeMap::new(), &foo);
        src.set_hashes_unknown(&mut src_root);
        let actual_fd = src.list(&mut src_root).unwrap()[0].1.clone();
        assert_eq!(FileData::Regular(0o777, 0, 0, UNKNOWN_HASH), actual_fd);

        let result = replace_replica(
            &dst, &mut dst_root, &mut files,
            &src, &src_root, &oss(""), &foo,
            None, Some(&actual_fd), &PrintlnLogger::stdout(),
            ReplicaSide::Client)
            .unwrap();
        assert_eq!(Some(&fd), result.as_ref());
        assert_eq!(Some(&fd), files.get(&foo));
    }

    #[test]
    fn replace_replica_update_regular() {
        let (mut dst, mut dst_root) = replica();
        let (mut src, mut src_root) = replica();
        let mut files = BTreeMap::new();
        let foo = oss("foo");

        genreg(&mut dst); // Discard first hash
        let to_replace = mkreg(&mut dst, &mut dst_root, &mut files, &foo);

        let fd = mkreg(&mut src, &mut src_root, &mut BTreeMap::new(), &foo);
        src.set_hashes_unknown(&mut src_root);
        let actual_fd = src.list(&mut src_root).unwrap()[0].1.clone();
        assert_eq!(FileData::Regular(0o777, 0, 0, UNKNOWN_HASH), actual_fd);

        let result = replace_replica(
            &dst, &mut dst_root, &mut files,
            &src, &src_root, &oss(""), &foo,
            Some(&to_replace), Some(&actual_fd),
            &PrintlnLogger::stdout(), ReplicaSide::Client)
            .unwrap();
        assert_eq!(Some(&fd), result.as_ref());
        assert_eq!(Some(&fd), files.get(&foo));
    }

    #[test]
    fn replace_replica_delete_regular() {
        let (mut dst, mut dst_root) = replica();
        let (src, src_root) = replica();
        let mut files = BTreeMap::new();
        let foo = oss("foo");

        let fd = mkreg(&mut dst, &mut dst_root, &mut files, &foo);

        let result = replace_replica(
            &dst, &mut dst_root, &mut files,
            &src, &src_root, &oss(""), &foo,
            Some(&fd), None, &PrintlnLogger::stdout(), ReplicaSide::Client)
            .unwrap();
        assert_eq!(None, result);
        assert!(files.is_empty());
    }

    #[test]
    fn try_replace_replica_success() {
        let (mut replica, mut root) = replica();
        let mut files = BTreeMap::new();
        let foo = oss("foo");
        let bar = oss("bar");

        mkreg(&mut replica, &mut root, &mut files, &foo);
        assert!(try_rename_replica(&replica, &mut root, &mut files,
                                   &oss(""), &foo, &bar,
                                   &PrintlnLogger::stdout(),
                                   ReplicaSide::Client));
        assert_eq!(1, files.len());
        assert!(files.contains_key(&bar));
        assert_eq!(&bar, &replica.list(&mut root).unwrap()[0].0);
    }

    #[test]
    fn try_replace_replica_error() {
        let (mut replica, mut root) = replica();
        let mut files = BTreeMap::new();
        let foo = oss("foo");
        let bar = oss("bar");

        mkreg(&mut replica, &mut root, &mut files, &foo);

        replica.data().faults.insert(
            memory_replica::Op::Rename(oss(""), foo.clone()),
            Box::new(|_| simple_error()));
        assert!(!try_rename_replica(&replica, &mut root, &mut files,
                                    &oss(""), &foo, &bar,
                                    &PrintlnLogger::stdout(),
                                    ReplicaSide::Server));
        assert_eq!(1, files.len());
        assert!(files.contains_key(&foo));
        assert_eq!(&foo, &replica.list(&mut root).unwrap()[0].0);
    }

    #[test]
    fn apply_recon_unsync() {
        let fx = Fixture::new(PrintWriter);
        let mut root = root_dir_context(&fx);
        let foo = oss("foo");

        let cfd = mkdir(&fx.client, &mut root.cli.dir, &mut root.cli.files,
                        &foo, 0o777);
        let sfd = mkdir(&fx.server, &mut root.srv.dir, &mut root.srv.files,
                        &foo, 0o666);

        let c = fx.context();
        assert_eq!(ApplyResult::Clean(false), c.apply_reconciliation(
            &mut root, &oss(""), &foo, Reconciliation::Unsync,
            Some(&cfd), Some(&sfd)));

        assert_eq!(Some(&cfd), root.cli.files.get(&foo));
        assert_eq!(None, root.anc.files.get(&foo));
        assert_eq!(Some(&sfd), root.srv.files.get(&foo));
    }

    #[test]
    fn apply_recon_insync_creates_ancestor() {
        let fx = Fixture::new(PrintWriter);
        let mut root = root_dir_context(&fx);
        let foo = oss("foo");

        let cfd = mkspec(&fx.client, &mut root.cli.dir,
                         &mut root.cli.files, &foo);
        let sfd = mkspec(&fx.server, &mut root.srv.dir,
                         &mut root.srv.files, &foo);

        let c = fx.context();
        assert_eq!(ApplyResult::Clean(false), c.apply_reconciliation(
            &mut root, &oss(""), &foo, Reconciliation::InSync,
            Some(&cfd), Some(&sfd)));

        assert_eq!(Some(&cfd), root.cli.files.get(&foo));
        assert_eq!(Some(&cfd), root.anc.files.get(&foo));
        assert_eq!(Some(&sfd), root.srv.files.get(&foo));
    }

    #[test]
    fn apply_recon_insync_ancestor_create_fail() {
        let fx = Fixture::new(PrintWriter);
        let mut root = root_dir_context(&fx);
        let foo = oss("foo");

        let cfd = mkspec(&fx.client, &mut root.cli.dir,
                         &mut root.cli.files, &foo);
        let sfd = mkspec(&fx.server, &mut root.srv.dir,
                         &mut root.srv.files, &foo);

        fx.ancestor.data().faults.insert(
            memory_replica::Op::Create(oss(""), foo.clone()),
            Box::new(|_| simple_error()));

        let c = fx.context();
        assert_eq!(ApplyResult::Fail, c.apply_reconciliation(
            &mut root, &oss(""), &foo, Reconciliation::InSync,
            Some(&cfd), Some(&sfd)));

        assert_eq!(Some(&cfd), root.cli.files.get(&foo));
        assert_eq!(None, root.anc.files.get(&foo));
        assert_eq!(Some(&sfd), root.srv.files.get(&foo));
    }

    #[test]
    fn apply_recon_insync_recurses_into_directories() {
        let fx = Fixture::new(PrintWriter);
        let mut root = root_dir_context(&fx);
        let foo = oss("foo");

        let cfd = mkdir(&fx.client, &mut root.cli.dir,
                        &mut root.cli.files, &foo, 0o777);
        mkdir(&fx.ancestor, &mut root.anc.dir, &mut root.anc.files,
              &foo, 0o777);
        let sfd = mkdir(&fx.server, &mut root.srv.dir,
                        &mut root.srv.files, &foo, 0o777);

        let c = fx.context();
        assert_eq!(ApplyResult::Clean(true), c.apply_reconciliation(
            &mut root, &oss(""), &foo, Reconciliation::InSync,
            Some(&cfd), Some(&sfd)));
    }

    #[test]
    fn apply_recon_use_client_normal_success() {
        let fx = Fixture::new(PrintWriter);
        let mut root = root_dir_context(&fx);
        let foo = oss("foo");

        let cfd = mksym(&fx.client, &mut root.cli.dir,
                        &mut root.cli.files, &foo, "client");
        mksym(&fx.ancestor, &mut root.anc.dir,
              &mut root.anc.files, &foo, "ancestor");
        let sfd = mksym(&fx.server, &mut root.srv.dir,
                        &mut root.srv.files, &foo, "server");

        let c = fx.context();
        assert_eq!(ApplyResult::Clean(false), c.apply_reconciliation(
            &mut root,
            &oss(""), &foo, Reconciliation::Use(ReconciliationSide::Client),
            Some(&cfd), Some(&sfd)));

        assert_eq!(Some(&cfd), root.cli.files.get(&foo));
        assert_eq!(Some(&cfd), root.anc.files.get(&foo));
        assert_eq!(Some(&cfd), root.srv.files.get(&foo));
    }

    #[test]
    fn apply_recon_use_client_create_dir() {
        let fx = Fixture::new(PrintWriter);
        let mut root = root_dir_context(&fx);
        let foo = oss("foo");

        let cfd = mkdir(&fx.client, &mut root.cli.dir,
                        &mut root.cli.files, &foo, 0o777);
        mksym(&fx.ancestor, &mut root.anc.dir,
              &mut root.anc.files, &foo, "ancestor");
        let sfd = mksym(&fx.server, &mut root.srv.dir,
                        &mut root.srv.files, &foo, "server");

        let c = fx.context();
        assert_eq!(ApplyResult::Clean(true), c.apply_reconciliation(
            &mut root,
            &oss(""), &foo, Reconciliation::Use(ReconciliationSide::Client),
            Some(&cfd), Some(&sfd)));

        assert_eq!(Some(&cfd), root.cli.files.get(&foo));
        assert_eq!(Some(&cfd), root.anc.files.get(&foo));
        assert_eq!(Some(&cfd), root.srv.files.get(&foo));
    }

    #[test]
    fn apply_recon_use_client_mutate_server_error() {
        let fx = Fixture::new(PrintWriter);
        let mut root = root_dir_context(&fx);
        let foo = oss("foo");

        let cfd = mkdir(&fx.client, &mut root.cli.dir,
                        &mut root.cli.files, &foo, 0o777);

        fx.server.data().faults.insert(
            memory_replica::Op::Create(oss(""), foo.clone()),
            Box::new(|_| simple_error()));

        let context = fx.context();
        assert_eq!(ApplyResult::Fail, context.apply_reconciliation(
            &mut root,
            &oss(""), &foo, Reconciliation::Use(ReconciliationSide::Client),
            Some(&cfd), None));

        assert_eq!(Some(&cfd), root.cli.files.get(&foo));
        assert_eq!(None, root.anc.files.get(&foo));
        assert_eq!(None, root.srv.files.get(&foo));
        assert!(context.anc.list(&mut root.anc.dir).unwrap().is_empty());
        assert!(context.srv.list(&mut root.srv.dir).unwrap().is_empty());
    }

    #[test]
    fn apply_recon_use_client_mutate_ancestor_error() {
        let fx = Fixture::new(PrintWriter);
        let mut root = root_dir_context(&fx);
        let foo = oss("foo");

        let cfd = mkdir(&fx.client, &mut root.cli.dir,
                        &mut root.cli.files, &foo, 0o777);

        fx.ancestor.data().faults.insert(
            memory_replica::Op::Create(oss(""), foo.clone()),
            Box::new(|_| simple_error()));

        let context = fx.context();
        assert_eq!(ApplyResult::Fail, context.apply_reconciliation(
            &mut root,
            &oss(""), &foo, Reconciliation::Use(ReconciliationSide::Client),
            Some(&cfd), None));

        assert_eq!(Some(&cfd), root.cli.files.get(&foo));
        assert_eq!(None, root.anc.files.get(&foo));
        assert_eq!(Some(&cfd), root.srv.files.get(&foo));
        assert!(context.anc.list(&mut root.anc.dir).unwrap().is_empty());
        assert_eq!(cfd, context.srv.list(&mut root.srv.dir).unwrap()[0].1);
    }

    #[test]
    fn apply_recon_use_client_recursive_delete_success() {
        let fx = Fixture::new(PrintWriter);
        let mut root = root_dir_context(&fx);
        let foo = oss("foo");

        mkspec(&fx.ancestor, &mut root.anc.dir,
               &mut root.anc.files, &foo);
        let sfd = mkdir(&fx.server, &mut root.srv.dir,
                        &mut root.srv.files, &foo, 0o777);

        assert_eq!(ApplyResult::RecursiveDelete(ReconciliationSide::Server,
                                                0o777),
                   fx.context().apply_reconciliation(
                       &mut root, &oss(""), &foo,
                       Reconciliation::Use(ReconciliationSide::Client),
                       None, Some(&sfd)));
        assert_eq!(None, root.cli.files.get(&foo));
        assert_eq!(Some(&sfd), root.anc.files.get(&foo));
        assert_eq!(Some(&sfd), root.srv.files.get(&foo));
    }

    #[test]
    fn apply_recon_use_client_recursive_delete_ancestor_error() {
        let fx = Fixture::new(PrintWriter);
        let mut root = root_dir_context(&fx);
        let foo = oss("foo");

        let afd = mkspec(&fx.ancestor, &mut root.anc.dir,
                         &mut root.anc.files, &foo);
        let sfd = mkdir(&fx.server, &mut root.srv.dir,
                        &mut root.srv.files, &foo, 0o777);

        fx.ancestor.data().faults.insert(
            memory_replica::Op::Update(oss(""), foo.clone()),
            Box::new(|_| simple_error()));

        assert_eq!(ApplyResult::Fail,
                   fx.context().apply_reconciliation(
                       &mut root, &oss(""), &foo,
                       Reconciliation::Use(ReconciliationSide::Client),
                       None, Some(&sfd)));
        assert_eq!(None, root.cli.files.get(&foo));
        assert_eq!(Some(&afd), root.anc.files.get(&foo));
        assert_eq!(Some(&sfd), root.srv.files.get(&foo));
    }

    #[test]
    fn apply_recon_use_server() {
        let fx = Fixture::new(PrintWriter);
        let mut root = root_dir_context(&fx);
        let foo = oss("foo");

        let sfd = mkdir(&fx.server, &mut root.srv.dir,
                        &mut root.srv.files, &foo, 0o777);

        let c = fx.context();
        assert_eq!(ApplyResult::Clean(true), c.apply_reconciliation(
            &mut root, &oss(""), &foo,
            Reconciliation::Use(ReconciliationSide::Server),
            None, Some(&sfd)));

        assert_eq!(Some(&sfd), root.cli.files.get(&foo));
        assert_eq!(Some(&sfd), root.anc.files.get(&foo));
        assert_eq!(Some(&sfd), root.srv.files.get(&foo));
    }

    #[test]
    fn apply_recon_split_client_delete_success() {
        let fx = Fixture::new(PrintWriter);
        let mut root = root_dir_context(&fx);
        let foo = oss("foo");
        let foo1 = oss("foo~1");

        let cfd = mkdir(&fx.client, &mut root.cli.dir,
                        &mut root.cli.files, &foo, 0o777);
        mkdir(&fx.ancestor, &mut root.anc.dir,
              &mut root.anc.files, &foo, 0o770);
        let sfd = mkdir(&fx.server, &mut root.srv.dir,
                        &mut root.srv.files, &foo, 0o700);

        let context = fx.context();
        assert_eq!(ApplyResult::Clean(false), context.apply_reconciliation(
            &mut root, &oss(""), &foo,
            Reconciliation::Split(ReconciliationSide::Client,
                                  SplitAncestorState::Delete),
            Some(&cfd), Some(&sfd)));

        assert_eq!(1, root.cli.files.len());
        assert_eq!(Some(&cfd), root.cli.files.get(&foo1));
        assert_eq!(&foo1, &context.cli.list(&mut root.cli.dir).unwrap()[0].0);
        assert!(root.anc.files.is_empty());
        assert!(context.anc.list(&mut root.anc.dir).unwrap().is_empty());
        assert_eq!(1, root.srv.files.len());
        assert_eq!(Some(&sfd), root.srv.files.get(&foo));
        assert_eq!(&foo, &context.srv.list(&mut root.srv.dir).unwrap()[0].0);
    }

    #[test]
    fn apply_recon_split_server_delete_success() {
        let fx = Fixture::new(PrintWriter);
        let mut root = root_dir_context(&fx);
        let foo = oss("foo");
        let foo1 = oss("foo~1");

        let cfd = mkdir(&fx.client, &mut root.cli.dir,
                        &mut root.cli.files, &foo, 0o777);
        mkdir(&fx.ancestor, &mut root.anc.dir,
              &mut root.anc.files, &foo, 0o770);
        let sfd = mkdir(&fx.server, &mut root.srv.dir,
                        &mut root.srv.files, &foo, 0o700);

        let context = fx.context();
        assert_eq!(ApplyResult::Clean(false), context.apply_reconciliation(
            &mut root, &oss(""), &foo,
            Reconciliation::Split(ReconciliationSide::Server,
                                  SplitAncestorState::Delete),
            Some(&cfd), Some(&sfd)));

        assert_eq!(1, root.cli.files.len());
        assert_eq!(Some(&cfd), root.cli.files.get(&foo));
        assert_eq!(&foo, &context.cli.list(&mut root.cli.dir).unwrap()[0].0);
        assert!(root.anc.files.is_empty());
        assert!(context.anc.list(&mut root.anc.dir).unwrap().is_empty());
        assert_eq!(1, root.srv.files.len());
        assert_eq!(Some(&sfd), root.srv.files.get(&foo1));
        assert_eq!(&foo1, &context.srv.list(&mut root.srv.dir).unwrap()[0].0);
    }

    #[test]
    fn apply_recon_split_client_delete_client_error() {
        let fx = Fixture::new(PrintWriter);
        let mut root = root_dir_context(&fx);
        let foo = oss("foo");

        let cfd = mkdir(&fx.client, &mut root.cli.dir,
                        &mut root.cli.files, &foo, 0o777);
        mkdir(&fx.ancestor, &mut root.anc.dir,
              &mut root.anc.files, &foo, 0o770);
        let sfd = mkdir(&fx.server, &mut root.srv.dir,
                        &mut root.srv.files, &foo, 0o700);

        fx.client.data().faults.insert(
            memory_replica::Op::Rename(oss(""), foo.clone()),
            Box::new(|_| simple_error()));

        let context = fx.context();
        assert_eq!(ApplyResult::Fail, context.apply_reconciliation(
            &mut root, &oss(""), &foo,
            Reconciliation::Split(ReconciliationSide::Client,
                                  SplitAncestorState::Delete),
            Some(&cfd), Some(&sfd)));

        assert_eq!(1, root.cli.files.len());
        assert_eq!(Some(&cfd), root.cli.files.get(&foo));
        assert_eq!(&foo, &context.cli.list(&mut root.cli.dir).unwrap()[0].0);
        assert!(root.anc.files.is_empty());
        assert!(context.anc.list(&mut root.anc.dir).unwrap().is_empty());
        assert_eq!(1, root.srv.files.len());
        assert_eq!(Some(&sfd), root.srv.files.get(&foo));
        assert_eq!(&foo, &context.srv.list(&mut root.srv.dir).unwrap()[0].0);
    }

    #[test]
    fn apply_recon_split_client_delete_ancestor_error() {
        let fx = Fixture::new(PrintWriter);
        let mut root = root_dir_context(&fx);
        let foo = oss("foo");

        let cfd = mkdir(&fx.client, &mut root.cli.dir,
                        &mut root.cli.files, &foo, 0o777);
        mkdir(&fx.ancestor, &mut root.anc.dir,
              &mut root.anc.files, &foo, 0o770);
        let sfd = mkdir(&fx.server, &mut root.srv.dir,
                        &mut root.srv.files, &foo, 0o700);

        fx.ancestor.data().faults.insert(
            memory_replica::Op::Remove(oss(""), foo.clone()),
            Box::new(|_| simple_error()));

        let context = fx.context();
        assert_eq!(ApplyResult::Fail, context.apply_reconciliation(
            &mut root, &oss(""), &foo,
            Reconciliation::Split(ReconciliationSide::Client,
                                  SplitAncestorState::Delete),
            Some(&cfd), Some(&sfd)));

        assert_eq!(1, root.cli.files.len());
        assert_eq!(Some(&cfd), root.cli.files.get(&foo));
        assert_eq!(&foo, &context.cli.list(&mut root.cli.dir).unwrap()[0].0);
        assert_eq!(1, root.srv.files.len());
        assert_eq!(Some(&sfd), root.srv.files.get(&foo));
        assert_eq!(&foo, &context.srv.list(&mut root.srv.dir).unwrap()[0].0);
    }

    #[test]
    fn apply_recon_split_client_move_ancestor_success() {
        let fx = Fixture::new(PrintWriter);
        let mut root = root_dir_context(&fx);
        let foo = oss("foo");
        let foo1 = oss("foo~1");

        let cfd = mkdir(&fx.client, &mut root.cli.dir,
                        &mut root.cli.files, &foo, 0o777);
        let afd = mkdir(&fx.ancestor, &mut root.anc.dir,
                        &mut root.anc.files, &foo, 0o770);
        let sfd = mkdir(&fx.server, &mut root.srv.dir,
                        &mut root.srv.files, &foo, 0o700);

        let context = fx.context();
        assert_eq!(ApplyResult::Clean(false), context.apply_reconciliation(
            &mut root, &oss(""), &foo,
            Reconciliation::Split(ReconciliationSide::Client,
                                  SplitAncestorState::Move),
            Some(&cfd), Some(&sfd)));
        assert_eq!(2, root.todo.len());

        assert_eq!(1, root.cli.files.len());
        assert_eq!(Some(&cfd), root.cli.files.get(&foo1));
        assert_eq!(&foo1, &context.cli.list(&mut root.cli.dir).unwrap()[0].0);
        assert_eq!(1, root.anc.files.len());
        assert_eq!(Some(&afd), root.anc.files.get(&foo1));
        assert_eq!(1, context.anc.list(&mut root.anc.dir).unwrap().len());
        assert_eq!(&foo1, &context.anc.list(&mut root.anc.dir).unwrap()[0].0);
        assert_eq!(1, root.srv.files.len());
        assert_eq!(Some(&sfd), root.srv.files.get(&foo));
        assert_eq!(&foo, &context.srv.list(&mut root.srv.dir).unwrap()[0].0);
    }

    #[test]
    fn apply_recon_split_client_move_ancestor_client_error() {
        let fx = Fixture::new(PrintWriter);
        let mut root = root_dir_context(&fx);
        let foo = oss("foo");

        let cfd = mkdir(&fx.client, &mut root.cli.dir,
                        &mut root.cli.files, &foo, 0o777);
        mkdir(&fx.ancestor, &mut root.anc.dir,
              &mut root.anc.files, &foo, 0o770);
        let sfd = mkdir(&fx.server, &mut root.srv.dir,
                        &mut root.srv.files, &foo, 0o700);

        fx.client.data().faults.insert(
            memory_replica::Op::Rename(oss(""), foo.clone()),
            Box::new(|_| simple_error()));

        let context = fx.context();
        assert_eq!(ApplyResult::Fail, context.apply_reconciliation(
            &mut root, &oss(""), &foo,
            Reconciliation::Split(ReconciliationSide::Client,
                                  SplitAncestorState::Move),
            Some(&cfd), Some(&sfd)));
        assert!(root.todo.is_empty());

        assert_eq!(1, root.cli.files.len());
        assert_eq!(Some(&cfd), root.cli.files.get(&foo));
        assert_eq!(&foo, &context.cli.list(&mut root.cli.dir).unwrap()[0].0);
        // root.anc.files will be out of date, but listing the ancestor will
        // effect the condemnation of the old name.
        assert_eq!(1, root.anc.files.len());
        assert!(context.anc.list(&mut root.anc.dir).unwrap().is_empty());
        assert_eq!(1, root.srv.files.len());
        assert_eq!(Some(&sfd), root.srv.files.get(&foo));
        assert_eq!(&foo, &context.srv.list(&mut root.srv.dir).unwrap()[0].0);
    }

    #[test]
    fn apply_recon_split_client_move_ancestor_ancestor_error() {
        let fx = Fixture::new(PrintWriter);
        let mut root = root_dir_context(&fx);
        let foo = oss("foo");
        let foo1 = oss("foo~1");

        let cfd = mkdir(&fx.client, &mut root.cli.dir,
                        &mut root.cli.files, &foo, 0o777);
        mkdir(&fx.ancestor, &mut root.anc.dir,
              &mut root.anc.files, &foo, 0o770);
        let sfd = mkdir(&fx.server, &mut root.srv.dir,
                        &mut root.srv.files, &foo, 0o700);

        fx.ancestor.data().faults.insert(
            memory_replica::Op::Rename(oss(""), foo.clone()),
            Box::new(|_| simple_error()));

        let context = fx.context();
        assert_eq!(ApplyResult::Fail, context.apply_reconciliation(
            &mut root, &oss(""), &foo,
            Reconciliation::Split(ReconciliationSide::Client,
                                  SplitAncestorState::Move),
            Some(&cfd), Some(&sfd)));
        assert!(root.todo.is_empty());

        assert_eq!(1, root.cli.files.len());
        assert_eq!(Some(&cfd), root.cli.files.get(&foo1));
        assert_eq!(&foo1, &context.cli.list(&mut root.cli.dir).unwrap()[0].0);
        // root.anc.files will be out of date, but listing the ancestor will
        // effect the condemnation of the old name.
        assert_eq!(1, root.anc.files.len());
        assert!(context.anc.list(&mut root.anc.dir).unwrap().is_empty());
        assert_eq!(1, root.srv.files.len());
        assert_eq!(Some(&sfd), root.srv.files.get(&foo));
        assert_eq!(&foo, &context.srv.list(&mut root.srv.dir).unwrap()[0].0);
    }
}
