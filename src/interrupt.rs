//-
// Copyright (c) 2017, 2021, Jason Lingle
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

use libc;
use std::ptr;
use std::sync::atomic::Ordering::{Relaxed, SeqCst};
use std::sync::atomic::{AtomicBool, AtomicPtr};
use std::sync::Arc;

use crate::replica::WatchHandle;

static INTERRUPTED: AtomicBool = AtomicBool::new(false);
static NOTIFY_WATCH: AtomicPtr<WatchHandle> = AtomicPtr::new(ptr::null_mut());

pub fn is_interrupted() -> bool {
    INTERRUPTED.load(Relaxed)
}

unsafe extern "C" fn handle_sigint(_: libc::c_int) {
    let message = b"\nSync interrupted, stopping gracefully.\n\
                    Press ^C again to terminate immediately.\n";
    libc::write(
        2,
        message.as_ptr() as *const libc::c_void,
        message.len() as libc::size_t,
    );
    INTERRUPTED.store(true, Relaxed);
    libc::signal(libc::SIGTERM, libc::SIG_DFL);
    libc::signal(libc::SIGINT, libc::SIG_DFL);

    let watch: *const WatchHandle = NOTIFY_WATCH.load(SeqCst);
    if !watch.is_null() {
        (*watch).notify();
    }
}

pub fn install_signal_handler() {
    unsafe {
        libc::signal(
            libc::SIGTERM,
            handle_sigint as unsafe extern "C" fn(libc::c_int)
                as libc::sighandler_t,
        );
        libc::signal(
            libc::SIGINT,
            handle_sigint as unsafe extern "C" fn(libc::c_int)
                as libc::sighandler_t,
        );
    }
}

pub fn notify_on_signal(watch: Arc<WatchHandle>) {
    let ptr = Arc::into_raw(watch);

    assert!(
        Ok(ptr::null_mut())
            == NOTIFY_WATCH.compare_exchange(
                ptr::null_mut(),
                ptr as *mut WatchHandle,
                SeqCst,
                SeqCst
            )
    );
}

pub fn clear_notify() {
    // Unset the global pointer, *then* check whether an interrupt has
    // happened.
    let ptr = NOTIFY_WATCH.swap(ptr::null_mut(), SeqCst);
    let interrupted = INTERRUPTED.load(SeqCst);
    // If an interrupt has occurred, leak the `WatchHandle` since the interrupt
    // handler could be trying to use it from another thread. The leak is fine
    // since the program will exit soon anyway.
    if !interrupted && !ptr.is_null() {
        drop(unsafe { Arc::from_raw(ptr as *const WatchHandle) })
    }
}
