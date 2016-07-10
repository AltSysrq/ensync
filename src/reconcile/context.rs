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

use std::cmp::{Ord,Ordering};
use std::collections::{BinaryHeap,BTreeMap,HashMap};
use std::num::Wrapping;
use std::ffi::{OsStr,OsString};
use std::sync::Mutex;

use defs::*;
use work_stack::WorkStack;
use replica::*;
use log::Logger;
use rules::DirRules;

// This whole thing is basicall stable-man's-FnBox. We wrap an FnOnce in an
// option and a box so we can make something approximating an FnMut (and which
// thus can be invoked as a DST), while relegating the lifetime check to
// runtime.
struct TaskContainer<F>(Mutex<Option<F>>);
pub trait TaskT<T> {
    fn invoke(&self, t: &T);
}
impl<T, F : FnOnce(&T)> TaskT<T> for TaskContainer<F> {
    fn invoke(&self, t: &T) {
        self.0.lock().unwrap().take().unwrap()(t)
    }
}
pub type Task<T> = Box<TaskT<T>>;
pub fn task<T, F : FnOnce(&T) + 'static>(f: F) -> Task<T> {
    Box::new(TaskContainer(Mutex::new(Some(f))))
}

/// Maps integer ids to unqueued tasks.
///
/// An unfortunate side-effect of the `Interface` thing is that the lifetime of
/// a `Task` is inextricably linked to that of the `Interface` that it will
/// eventually be queued into, even though it does not (and, in fact, *can
/// not*) contain a reference dependent on the lifetime of that `Interface`.
///
/// Thus we need to store tasks inside the interface itself; this provides the
/// functionality to do so.
struct UnqueuedTasksData<T> {
    tasks: HashMap<usize,T>,
    next_id: Wrapping<usize>,
}
pub struct UnqueuedTasks<T>(Mutex<UnqueuedTasksData<T>>);
impl<T> UnqueuedTasks<T> {
    pub fn new() -> Self {
        UnqueuedTasks(Mutex::new(UnqueuedTasksData {
            tasks: HashMap::new(),
            next_id: Wrapping(0),
        }))
    }

    pub fn put(&self, task: T) -> usize {
        let mut lock = self.0.lock().unwrap();
        while lock.tasks.contains_key(&lock.next_id.0) {
            lock.next_id = lock.next_id + Wrapping(1);
        }

        let id = lock.next_id.0;
        lock.next_id = lock.next_id + Wrapping(1);
        lock.tasks.insert(id, task);
        id
    }

    pub fn get(&self, id: usize) -> T {
        self.0.lock().unwrap().tasks.remove(&id).unwrap()
    }
}

/// A bundle of traits passed to the reconciler.
///
/// This is only intended to be implemented by `Context`. The true purpose of
/// this trait is to essentially provide typedefs, since those are currently
/// not allowed in inherent impls right now for whatever reason.
pub trait Interface : Sized {
    type CliDir : ReplicaDirectory + 'static;
    type AncDir : ReplicaDirectory + 'static;
    type SrvDir : ReplicaDirectory + 'static;
    type Cli : Replica<Directory = Self::CliDir>;
    type Anc : Replica<Directory = Self::AncDir> + NullTransfer + Condemn;
    type Srv : Replica<Directory = Self::SrvDir,
                            TransferIn = <Self::Cli as Replica>::TransferOut,
                            TransferOut = <Self::Cli as Replica>::TransferIn>;
    type Log : Logger;
    type Rules : DirRules + 'static;

    fn cli(&self) -> &Self::Cli;
    fn anc(&self) -> &Self::Anc;
    fn srv(&self) -> &Self::Srv;
    fn log(&self) -> &Self::Log;
    fn root_rules(&self) -> <Self::Rules as DirRules>::FileRules;
    fn work(&self) -> &WorkStack<Task<Self>>;
    fn tasks(&self) -> &UnqueuedTasks<Task<Self>>;

    fn run_work(&self) {
        self.work().run(|task| task.invoke(self));
    }
}

pub struct Context<'a,
                   CLI : 'a + Replica,
                   ANC : 'a + Replica + NullTransfer + Condemn,
                   SRV : 'a + Replica<TransferIn = CLI::TransferOut,
                                      TransferOut = CLI::TransferIn>,
                   LOG : 'a + Logger,
                   RULES : DirRules + 'static> {
    pub cli: &'a CLI,
    pub anc: &'a ANC,
    pub srv: &'a SRV,
    pub logger: &'a LOG,
    pub root_rules: <RULES as DirRules>::FileRules,
    pub work: WorkStack<Task<Context<'a,CLI,ANC,SRV,LOG,RULES>>>,
    pub tasks: UnqueuedTasks<Task<Context<'a,CLI,ANC,SRV,LOG,RULES>>>,
}

impl<'a,
     CLI : 'a + Replica,
     ANC : 'a + Replica + NullTransfer + Condemn,
     SRV : 'a + Replica<TransferIn = CLI::TransferOut,
                        TransferOut = CLI::TransferIn>,
     LOG : 'a + Logger,
     RULES : 'a + DirRules>
Interface for Context<'a, CLI, ANC, SRV, LOG, RULES> {
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
    fn root_rules(&self) -> <RULES as DirRules>::FileRules { self.root_rules.clone() }
    fn work(&self) -> &WorkStack<Task<Self>> { &self.work }
    fn tasks(&self) -> &UnqueuedTasks<Task<Self>> { &self.tasks }
}


/// Directory-specific context information for a single replica.
pub struct SingleDirContext<T> {
    /// The `Replica::Directory` object.
    pub dir: T,
    /// The files in this directory when it was listed.
    pub files: BTreeMap<OsString,FileData>,
}

/// Directory-specific context information.
pub struct DirContextRaw<CD,AD,SD,RU> {
    pub cli: SingleDirContext<CD>,
    pub anc: SingleDirContext<AD>,
    pub srv: SingleDirContext<SD>,
    /// Queue of filenames to process. Files are processed in approximately
    /// asciibetical order so that informational messages can give a rough idea
    /// of progress.
    pub todo: BinaryHeap<Reversed<OsString>>,
    pub rules: RU,
}
/// Convenience for `DirContextRaw` without needing to type everything out
/// every time.
///
/// This currently produces a warning that the constraint is not checked (even
/// though it is, by nature of actually using the type as such). This may cause
/// problems down the road if enforcement includes the weird tendency to
/// propagate lifetimes to nominally (but not physically)-referenced types.
/// Hopefully, by the time that comes, it will finally be possible to define
/// inherent types, and the `Interface` thing can go away altogether and this
/// will just be a typedef inside `Context`.
pub type DirContext<I : Interface> = DirContextRaw<
        I::CliDir, I::AncDir, I::SrvDir, I::Rules>;

impl<CD,AD,SD,RU> DirContextRaw<CD,AD,SD,RU> {
    pub fn name_in_use(&self, name: &OsStr) -> bool {
        self.cli.files.contains_key(name) ||
            self.anc.files.contains_key(name) ||
            self.srv.files.contains_key(name)
    }
}

#[derive(PartialEq,Eq)]
pub struct Reversed<T : Ord + PartialEq + Eq>(pub T);
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
