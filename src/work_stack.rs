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

use std::sync::{Condvar, Mutex};

struct WorkStackData<T> {
    tasks: Vec<T>,
    in_flight: u32,
}

impl<T> WorkStackData<T> {
    fn new() -> Self {
        WorkStackData {
            tasks: vec![],
            in_flight: 0,
        }
    }
}

/// A lock-based, multi-producer multi-producer LIFO channel.
///
/// This is used for queueing recursion into directories, and so forth. The use
/// of a LIFO channel is not needed for correctness, but means we do something
/// more akin to a depth-first search rather than a breadth-first search, which
/// a FIFO would produce; this in turn results in better locality, both for
/// performance, correlation of errors, and progress tracking.
///
/// The channel does not offer an explicit "receive" call; instead, a thread
/// calls the `run()` method to process messages until the channel is fully
/// idle.
pub struct WorkStack<T> {
    data: Mutex<WorkStackData<T>>,
    cond: Condvar,
}

impl<T> WorkStack<T> {
    /// Creates a new, empty WorkStack.
    pub fn new() -> Self {
        WorkStack {
            data: Mutex::new(WorkStackData::new()),
            cond: Condvar::new(),
        }
    }

    /// Pushes the given task onto the stack.
    pub fn push(&self, task: T) {
        let mut lock = self.data.lock().unwrap();
        lock.tasks.push(task);
        self.cond.notify_one();
    }

    /// Donates the current thread to run tasks from this stack.
    ///
    /// The current thread will process tasks from the stack by passing them to
    /// `f`. The function returns when the stack is empty and there are no
    /// in-flight tasks, or when the interrupt handler has recorded an
    /// interrupt.
    pub fn run<F: FnMut(T)>(&self, mut f: F) {
        let mut lock = self.data.lock().unwrap();

        loop {
            if let Some(task) = lock.tasks.pop() {
                // We got a task. Increment the in-flight counter so other
                // threads know we might be adding something later, then unlock
                // to begin processing.
                lock.in_flight += 1;
                drop(lock);
                f(task);
                lock = self.data.lock().unwrap();
                lock.in_flight -= 1;
            } else if lock.in_flight > 0 {
                // No tasks ready, but other threads are still processing and
                // thus might add more tasks later.
                lock = self.cond.wait(lock).unwrap();
            } else {
                // No tasks and nothing processing; we're done.
                break;
            }
        }

        // Everything's done. Make sure all the other threads wake up to
        // notice.
        self.cond.notify_all();
    }
}
