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

use std::cmp::{Ord,Ordering};
use std::collections::{BinaryHeap,BTreeMap};
use std::ffi::{CStr,CString};
use std::result;

use defs::*;
use rules::*;
use log;
use log::{Logger,ReplicaSide,EditDeleteConflictResolution};
use log::{EditEditConflictResolution,ErrorOperation,Log};
use replica::{Replica,ReplicaDirectory,Result,NullTransfer,Condemn};
use super::compute::{Reconciliation,ReconciliationSide,gen_alternate_name};
use super::compute::SplitAncestorState;

/// A bundle of traits passed to the reconciler.
///
/// This is only intended to be implemented by `Context`. The true purpose of
/// this trait is to essentially provide typedefs, since those are currently
/// not allowed in inherent impls right now for whatever reason.
pub trait Interface<'a> {
    type CliDir : ReplicaDirectory;
    type AncDir : ReplicaDirectory;
    type SrvDir : ReplicaDirectory;
    type Cli : 'a + Replica<Directory = Self::CliDir>;
    type Anc : 'a + Replica<Directory = Self::AncDir> + NullTransfer + Condemn;
    type Srv : 'a + Replica<Directory = Self::SrvDir,
                            TransferIn = <Self::Cli as Replica>::TransferOut,
                            TransferOut = <Self::Cli as Replica>::TransferIn>;
    type Log : 'a + Logger;
    type Rules : 'a + RulesMatcher;

    fn cli(&self) -> &Self::Cli;
    fn anc(&self) -> &Self::Anc;
    fn srv(&self) -> &Self::Srv;
    fn log(&self) -> &Self::Log;
    fn root_rules(&self) -> Self::Rules;
}

pub struct Context<'a,
                   CLI : 'a + Replica,
                   ANC : 'a + Replica + NullTransfer + Condemn,
                   SRV : 'a + Replica<TransferIn = CLI::TransferOut,
                                      TransferOut = CLI::TransferIn>,
                   LOG : 'a + Logger,
                   RULES : 'a + RulesMatcher> {
    pub cli: &'a CLI,
    pub anc: &'a ANC,
    pub srv: &'a SRV,
    pub logger: &'a LOG,
    pub root_rules: RULES,
}

impl<'a,
     CLI : 'a + Replica,
     ANC : 'a + Replica + NullTransfer + Condemn,
     SRV : 'a + Replica<TransferIn = CLI::TransferOut,
                        TransferOut = CLI::TransferIn>,
     LOG : 'a + Logger,
     RULES : 'a + RulesMatcher>
Interface<'a> for Context<'a, CLI, ANC, SRV, LOG, RULES> {
    type Cli = CLI;
    type CliDir = CLI::Directory;
    type Anc = ANC;
    type AncDir = ANC::Directory;
    type Srv = SRV;
    type SrvDir = SRV::Directory;
    type Log = LOG;
    type Rules = RULES;

    fn cli(&self) -> &Self::Cli { self.cli }
    fn anc(&self) -> &Self::Anc { self.anc }
    fn srv(&self) -> &Self::Srv { self.srv }
    fn log(&self) -> &Self::Log { self.logger }
    fn root_rules(&self) -> Self::Rules { self.root_rules.clone() }
}


#[derive(PartialEq,Eq)]
struct Reversed<T : Ord + PartialEq + Eq>(T);
impl<T : Ord> PartialOrd<Reversed<T>> for Reversed<T> {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl<T : Ord> Ord for Reversed<T> {
    fn cmp(&self, other: &Self) -> Ordering {
        other.0.cmp(&self.0)
    }
}

/// Directory-specific context information for a single replica.
struct SingleDirContext<T> {
    /// The `Replica::Directory` object.
    dir: T,
    /// The files in this directory when it was listed.
    files: BTreeMap<CString,FileData>,
}

/// Directory-specific context information.
pub struct DirContext<'a, I : Interface<'a>> {
    cli: SingleDirContext<I::CliDir>,
    anc: SingleDirContext<I::AncDir>,
    srv: SingleDirContext<I::SrvDir>,
    /// Queue of filenames to process. Files are processed in approximately
    /// asciibetical order so that informational messages can give a rough idea
    /// of progress.
    todo: BinaryHeap<Reversed<CString>>,
    rules: I::Rules,
}

impl<'a, I : Interface<'a>> DirContext<'a,I> {
    fn name_in_use(&self, name: &CStr) -> bool {
        self.cli.files.contains_key(name) ||
            self.anc.files.contains_key(name) ||
            self.srv.files.contains_key(name)
    }
}

fn is_dir(fd: Option<&FileData>) -> bool {
    match fd {
        Some(&FileData::Directory(_)) => true,
        _ => false,
    }
}

/// Updates the ancestor replica so that the file identified by `name` is
/// changed from the state `old` to the state `new`.
///
/// On success, `files` is updated accordinly.
///
/// This automatically handles issues like needing to recursively remove
/// directories if `old` is an existing directory and `new` is not.
fn replace_ancestor<A : Replica + NullTransfer>(
    replica: &A, in_dir: &mut A::Directory,
    files: &mut BTreeMap<CString,FileData>,
    name: &CStr,
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
    files: &mut BTreeMap<CString,FileData>,
    dir_name: &CStr, name: &CStr,
    old: Option<&FileData>, new: Option<&FileData>,
    log: &LOG) -> bool
{
    replace_ancestor(replica, in_dir, files, name, old, new)
        .to_bool(|error| log.log(
            log::ERROR, &Log::Error(
                ReplicaSide::Ancestor, dir_name,
                ErrorOperation::Update(name), &*error)))
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
    dst_files: &mut BTreeMap<CString,FileData>,
    src: &SRC, src_dir: &SRC::Directory,
    dir_name: &CStr, name: &CStr,
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
                    log.log(log::ERROR, &Log::Error(
                        side, dir_name, ErrorOperation::Remove(name),
                        &*error));
                    Err(error)
                }
            }
        },
        (Some(oldfd), Some(newfd)) => {
            // Update.
            log.log(log::EDIT, &Log::Update(side, dir_name, name,
                                            oldfd, newfd));
            match dst.update(dst_dir, name, oldfd, newfd,
                             src.transfer(src_dir, File(name, newfd))) {
                Ok(r) => {
                    // We need to insert r, not newfd, because newfd may be
                    // out-of-date.
                    dst_files.insert(name.to_owned(), r.clone())
                        .expect("Updated file not in table?");
                    Ok(Some(r))
                },
                Err(error) => {
                    log.log(log::ERROR, &Log::Error(
                        side, dir_name, ErrorOperation::Update(name),
                        &*error));
                    Err(error)
                }
            }
        },
        (None, Some(newfd)) => {
            // Insertion.
            log.log(log::EDIT, &Log::Create(side, dir_name, name, newfd));
            match dst.create(dst_dir, File(name, newfd),
                             src.transfer(src_dir, File(name, newfd))) {
                Ok(r) => {
                    // We need to insert r, not newfd, because newfd may be
                    // out-of-date.
                    dst_files.insert(name.to_owned(), r.clone())
                        .map(|_| panic!("Inserted file already in table?"));
                    Ok(Some(r))
                },
                Err(error) => {
                    log.log(log::ERROR, &Log::Error(
                        side, dir_name, ErrorOperation::Create(name),
                        &*error));
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
    files: &mut BTreeMap<CString,FileData>,
    dir_name: &CStr, old_name: &CStr, new_name: &CStr,
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
            log.log(log::ERROR, &Log::Error(
                side, dir_name, ErrorOperation::Rename(old_name), &*error));
            false
        }
    }
}

/// Return type from `apply_reconciliation()`.
pub enum ApplyResult {
    /// Any changes needed have been applied. The field indicates whether the
    /// final file is a directory and should be entered recursively.
    Clean(bool),
    /// Changes have failed. Do not mark parent directories clean.
    Fail,
    /// A directory on the identified side is being (probably) recursively
    /// deleted. Recurse, creating a synthetic directory on the opposite end
    /// replica. After recursion, if the directories are empty,
    RecursiveDelete(ReconciliationSide),
}

/// Applies the determined reconciliation for one file.
///
/// Returns whether recursion is necessary to process this file, and if so,
/// what should happen afterwards.
///
/// Errors are passed down the context's logger.
pub fn apply_reconciliation<'a, I : Interface<'a>>(
    i: &I, dir: &mut DirContext<'a, I>,
    dir_name: &CStr, name: &CStr,
    recon: Reconciliation,
    old_cli: Option<FileData>, old_srv: Option<FileData>)
    -> ApplyResult
{
    use super::compute::Reconciliation::*;

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
        InSync => Some(old_cli.clone()),
        // For Unsync and Irreconcilable, the end replicas do not agree and
        // will not agree, so it doesn't make sense to change the ancestor at
        // all.
        Unsync => None,
        Irreconcilable => None,
        // For the Use(x) cases, we propagate the change, and use the resulting
        // FileData for the ancestor.
        Use(ReconciliationSide::Client) => {
            // If we're deleting a directory, we need to recurse first and let
            // reconciliation empty it naturally.
            if is_dir(old_srv.as_ref()) && old_cli.is_none() {
                return ApplyResult::RecursiveDelete(ReconciliationSide::Server);
            }
            // Other than the directory deletion case, the compute stage is
            // supposed to never give us Use(x) which replaces a directory with
            // a non-directory.
            assert!(is_dir(old_cli.as_ref()) || !is_dir(old_srv.as_ref()));
            // Do the actual replacement.
            match replace_replica(
                i.srv(), &mut dir.srv.dir, &mut dir.srv.files,
                i.cli(), &dir.cli.dir,
                dir_name, name,
                old_srv.as_ref(), old_cli.as_ref(),
                i.log(), ReplicaSide::Server)
            {
                Ok(r) => Some(r),
                Err(_) => return ApplyResult::Fail,
            }
        },
        Use(ReconciliationSide::Server) => {
            if is_dir(old_cli.as_ref()) && old_srv.is_none() {
                return ApplyResult::RecursiveDelete(ReconciliationSide::Client);
            }
            assert!(is_dir(old_srv.as_ref()) || !is_dir(old_cli.as_ref()));
            match replace_replica(
                i.cli(), &mut dir.cli.dir, &mut dir.cli.files,
                i.srv(), &dir.srv.dir,
                dir_name, name,
                old_srv.as_ref(), old_cli.as_ref(),
                i.log(), ReplicaSide::Client)
            {
                Ok(r) => Some(r),
                Err(_) => return ApplyResult::Fail,
            }
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
                    i.anc(), &mut dir.anc.dir, &mut dir.anc.files,
                    dir_name, name, old_ancestor.as_ref(), None, i.log()),

                SplitAncestorState::Move =>
                    i.anc().condemn(&mut dir.anc.dir, name).to_bool(
                        |error|  i.log().log(log::ERROR, &Log::Error(
                            ReplicaSide::Ancestor, dir_name,
                            ErrorOperation::Access(name), &*error))),
            }

            && match side {
                ReconciliationSide::Client => try_rename_replica(
                    i.cli(), &mut dir.cli.dir, &mut dir.cli.files,
                    dir_name, name, &new_name, i.log(), ReplicaSide::Client),

                ReconciliationSide::Server => try_rename_replica(
                    i.srv(), &mut dir.srv.dir, &mut dir.srv.files,
                    dir_name, name, &new_name, i.log(), ReplicaSide::Server),
            }

            && match anc_state {
                SplitAncestorState::Delete => true,

                SplitAncestorState::Move => {
                    try_rename_replica(
                        i.anc(), &mut dir.anc.dir, &mut dir.anc.files,
                        dir_name, name, &new_name, i.log(), ReplicaSide::Ancestor)

                    && i.anc().uncondemn(&mut dir.anc.dir, name).to_bool(
                        |error| i.log().log(log::ERROR, &Log::Error(
                            ReplicaSide::Ancestor, dir_name,
                            ErrorOperation::Access(name), &*error)))
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
            i.anc(), &mut dir.anc.dir, &mut dir.anc.files,
            dir_name, name, old.as_ref(), new_ancestor.as_ref(), i.log());

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
}

#[cfg(test)]
mod test {
    use std::collections::{BTreeMap};
    use std::ffi::{CString,CStr};

    use defs::*;
    use log::{PrintlnLogger,ReplicaSide};
    use replica::{Replica,NullTransfer};
    use memory_replica::{self,MemoryReplica,DirHandle,simple_error};
    use rules::*;

    use super::*;
    use super::{replace_ancestor,replace_replica,try_rename_replica};

    #[derive(Clone)]
    struct ConstantRules<'a>(&'a SyncMode);

    impl<'a> RulesMatcher for ConstantRules<'a> {
        fn dir_contains(&mut self, _: File) { }
        fn child(&self, _: &CStr) -> Self { self.clone() }
        fn sync_mode(&self) -> SyncMode { *self.0 }
    }

    type TContext<'a> = Context<'a, MemoryReplica, MemoryReplica, MemoryReplica,
                                PrintlnLogger, ConstantRules<'a>>;

    struct Fixture {
        client: MemoryReplica,
        ancestor: MemoryReplica,
        server: MemoryReplica,
        logger: PrintlnLogger,
        mode: SyncMode,
    }

    impl Fixture {
        fn new() -> Self {
            Fixture {
                client: MemoryReplica::empty(),
                ancestor: MemoryReplica::empty(),
                server: MemoryReplica::empty(),
                logger: PrintlnLogger,
                mode: Default::default(),
            }
        }

        fn context(&self) -> TContext {
            Context {
                cli: &self.client,
                anc: &self.ancestor,
                srv: &self.server,
                logger: &self.logger,
                root_rules: ConstantRules(&self.mode),
            }
        }
    }

    fn oss(s: &str) -> CString {
        CString::new(s).unwrap()
    }

    fn replica() -> (MemoryReplica, DirHandle) {
        let mr = MemoryReplica::empty();
        let root = mr.root().unwrap();
        (mr, root)
    }

    fn genreg(replica: &mut MemoryReplica) -> FileData {
        FileData::Regular(0o777, 0, 0, replica.gen_hash())
    }

    fn mkreg(replica: &mut MemoryReplica, dir: &mut DirHandle,
             files: &mut BTreeMap<CString,FileData>,
             name: &CStr) -> FileData {
        let fd = genreg(replica);
        replica.create(dir, File(name, &fd), MemoryReplica::null_transfer(&fd))
            .unwrap();
        files.insert(name.to_owned(), fd.clone());
        fd
    }

    fn mkdir(replica: &MemoryReplica, dir: &mut DirHandle,
             files: &mut BTreeMap<CString,FileData>,
             name: &CStr, mode: FileMode) -> FileData {
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
        assert_eq!(1, replica.list(&root).unwrap().len());
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
        assert_eq!(fd2, replica.list(&root).unwrap()[0].1);
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
        assert!(replica.list(&root).unwrap().is_empty());
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
        assert!(replica.list(&root).unwrap().is_empty());
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
        assert!(replica.list(&subdir_foo).is_err());
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
        assert!(replica.list(&subdir_foo).is_ok());
        assert_eq!(plugh, replica.list(&subdir_bar).unwrap()[0].0);
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
        assert_eq!(fd2, replica.list(&root).unwrap()[0].1);
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
            &PrintlnLogger, ReplicaSide::Server).unwrap());
    }

    #[test]
    fn replace_replica_create_regular() {
        let (dst, mut dst_root) = replica();
        let (mut src, mut src_root) = replica();
        let mut files = BTreeMap::new();
        let foo = oss("foo");

        let fd = mkreg(&mut src, &mut src_root, &mut BTreeMap::new(), &foo);
        src.set_hashes_unknown(&mut src_root);
        let actual_fd = src.list(&src_root).unwrap()[0].1.clone();
        assert_eq!(FileData::Regular(0o777, 0, 0, UNKNOWN_HASH), actual_fd);

        let result = replace_replica(
            &dst, &mut dst_root, &mut files,
            &src, &src_root, &oss(""), &foo,
            None, Some(&actual_fd), &PrintlnLogger, ReplicaSide::Client)
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
        let actual_fd = src.list(&src_root).unwrap()[0].1.clone();
        assert_eq!(FileData::Regular(0o777, 0, 0, UNKNOWN_HASH), actual_fd);

        let result = replace_replica(
            &dst, &mut dst_root, &mut files,
            &src, &src_root, &oss(""), &foo,
            Some(&to_replace), Some(&actual_fd),
            &PrintlnLogger, ReplicaSide::Client)
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
            Some(&fd), None, &PrintlnLogger, ReplicaSide::Client)
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
                                   &PrintlnLogger, ReplicaSide::Client));
        assert_eq!(1, files.len());
        assert!(files.contains_key(&bar));
        assert_eq!(&bar, &replica.list(&root).unwrap()[0].0);
    }

    #[test]
    fn try_replace_replica_error() {
        let (mut replica, mut root) = replica();
        let mut files = BTreeMap::new();
        let foo = oss("foo");
        let bar = oss("bar");

        mkreg(&mut replica, &mut root, &mut files, &foo);

        replica.data().faults.insert(
            memory_replica::Op::Rename(oss(""), foo.clone(), bar.clone()),
            Box::new(|_| simple_error()));
        assert!(!try_rename_replica(&replica, &mut root, &mut files,
                                    &oss(""), &foo, &bar,
                                    &PrintlnLogger, ReplicaSide::Server));
        assert_eq!(1, files.len());
        assert!(files.contains_key(&foo));
        assert_eq!(&foo, &replica.list(&root).unwrap()[0].0);
    }
}
