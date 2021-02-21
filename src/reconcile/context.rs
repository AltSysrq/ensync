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

use std::cmp::{Ord,Ordering};
use std::collections::{BinaryHeap,BTreeMap,HashMap};
use std::num::Wrapping;
use std::ffi::{OsStr,OsString};
use std::sync::Mutex;

use crate::defs::*;
use crate::work_stack::WorkStack;
use crate::replica::*;
use crate::log::Logger;
use crate::rules::engine::{FileEngine, DirEngine};

// This whole thing is basically stable-man's-FnBox. We wrap an FnOnce in an
// option and a box so we can make something approximating an FnMut (and which
// thus can be invoked as a DST), while relegating the lifetime check to
// runtime.
struct TaskContainer<F : Send>(Mutex<Option<F>>);
pub trait TaskT<T> : Send {
    fn invoke(&self, t: &T);
}
impl<T, F : FnOnce(&T) + Send> TaskT<T> for TaskContainer<F> {
    fn invoke(&self, t: &T) {
        self.0.lock().unwrap().take().unwrap()(t)
    }
}
pub type Task<T> = Box<dyn TaskT<T>>;
pub fn task<T, F : FnOnce(&T) + Send + 'static>(f: F) -> Task<T> {
    Box::new(TaskContainer(Mutex::new(Some(f))))
}

/// Maps integer ids to unqueued tasks.
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

pub struct Context<CLI : Replica,
                   ANC : Replica + NullTransfer + Condemn,
                   SRV : Replica<TransferIn = CLI::TransferOut,
                                 TransferOut = CLI::TransferIn>,
                   LOG : Logger> {
    pub cli: CLI,
    pub anc: ANC,
    pub srv: SRV,
    pub log: LOG,
    pub root_rules: FileEngine,
    pub work: WorkStack<Task<Self>>,
    pub tasks: UnqueuedTasks<Task<Self>>,
}

/// Define an `impl` block on `Context`.
///
/// Stylistically, we usually don't indent the body of this macro, since the
/// methods inside aren't _really_ supposed to be members of `Context`.
// TODO This is pretty awful and prevents rustfmt from doing its job
macro_rules! def_context_impl {
    ($($t:tt)*) => {
        impl<CLI : $crate::replica::Replica,
             ANC : $crate::replica::Replica +
                   $crate::replica::NullTransfer +
                   $crate::replica::Condemn,
             SRV : $crate::replica::Replica<
                       TransferIn = CLI::TransferOut,
                       TransferOut = CLI::TransferIn>,
             LOG : $crate::log::Logger>
        $crate::reconcile::Context<CLI, ANC, SRV, LOG> {
            $($t)*
        }
    }
}

def_context_impl! {
    pub fn run_work(&self) {
        self.work.run(|task| task.invoke(self));
    }
}

/// Within `def_context_impl!`, expand to the type of the client-side
/// directory.
macro_rules! cli_dir { () => { <CLI as $crate::replica::Replica>::Directory } }
/// Within `def_context_impl!`, expand to the type of the ancestor-side
/// directory.
macro_rules! anc_dir { () => { <ANC as $crate::replica::Replica>::Directory } }
/// Within `def_context_impl!`, expand to the type of the server-side
/// directory.
macro_rules! srv_dir { () => { <SRV as $crate::replica::Replica>::Directory } }
/// Within `def_context_impl!`, expand to the corresponding `DirContext`
/// type.
macro_rules! dir_ctx { () => {
    $crate::reconcile::context::DirContext<cli_dir!(), anc_dir!(),
                                           srv_dir!()>
} }

/// Directory-specific context information for a single replica.
pub struct SingleDirContext<T> {
    /// The `Replica::Directory` object.
    pub dir: T,
    /// The files in this directory when it was listed.
    pub files: BTreeMap<OsString,FileData>,
}

/// Directory-specific context information.
pub struct DirContext<CD,AD,SD> {
    pub cli: SingleDirContext<CD>,
    pub anc: SingleDirContext<AD>,
    pub srv: SingleDirContext<SD>,
    /// Queue of filenames to process. Files are processed in approximately
    /// asciibetical order so that informational messages can give a rough idea
    /// of progress.
    pub todo: BinaryHeap<Reversed<OsString>>,
    pub rules: DirEngine,
}

impl<CD,AD,SD> DirContext<CD,AD,SD> {
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
