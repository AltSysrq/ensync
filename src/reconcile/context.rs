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
use crate::replica::Replica;
use crate::work_stack::WorkStack;
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

pub struct Context<CLI, ANC, SRV> {
    pub cli: CLI,
    pub anc: ANC,
    pub srv: SRV,
    pub log: Box<dyn Logger + Send + Sync>,
    pub root_rules: FileEngine,
    pub work: WorkStack<Task<Self>>,
    pub tasks: UnqueuedTasks<Task<Self>>,
}

impl<CLI, ANC, SRV> Context<CLI, ANC, SRV> {
    pub fn run_work(&self) {
        self.work.run(|task| task.invoke(self));
    }
}

pub trait ContextExt {
    type Dir;
}

impl<CLI: Replica, ANC: Replica, SRV: Replica>
ContextExt for Context<CLI, ANC, SRV> {
    type Dir = DirContext<CLI::Directory, ANC::Directory, SRV::Directory>;
}

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
