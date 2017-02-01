//-
// Copyright (c) 2017, Jason Lingle
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

use std::sync::atomic::{AtomicBool, ATOMIC_BOOL_INIT};
use std::sync::atomic::Ordering::Relaxed;
use libc;

static INTERRUPTED: AtomicBool = ATOMIC_BOOL_INIT;

pub fn is_interrupted() -> bool {
    INTERRUPTED.load(Relaxed)
}

unsafe extern "C" fn handle_sigint(_: libc::c_int) {
    let message = b"Sync interrupted, stopping gracefully.\n\
                    Press ^C again to terminate gracelessly.\n";
    libc::write(2, message.as_ptr() as *const libc::c_void,
                message.len() as libc::size_t);
    INTERRUPTED.store(true, Relaxed);
    libc::signal(libc::SIGTERM, libc::SIG_DFL);
}

pub fn install_signal_handler() {
    unsafe {
        libc::signal(libc::SIGTERM, handle_sigint
                     as unsafe extern "C" fn (libc::c_int)
                     as libc::sighandler_t);
        libc::signal(libc::SIGINT, handle_sigint
                     as unsafe extern "C" fn (libc::c_int)
                     as libc::sighandler_t);
    }
}
