//-
// Copyright (c) 2016, 2017 Jason Lingle
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


// This is not a separate module, but an include file which defines all the
// tests for `server::Storage` implementations.
//
// The includer is expected to define a `fn create_storage(dir: &Path) -> impl
// Storage` function. This should be placed after the `include!`.
//
// These tests assume that the implementation uses a directory on the local
// filesystem with the same layout as `LocalStorage`, and will in some cases
// act on this assumption to simulate interference from concurrent processes.

use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize};
use std::sync::atomic::Ordering::SeqCst;
use std::thread;
use std::time::Duration;

use tempdir::TempDir;

#[allow(unused_imports)] use crate::defs::*;
#[allow(unused_imports)] use crate::server::storage::*;

macro_rules! init {
    ($dir:ident, $storage:ident) => {
        let $dir = TempDir::new("storage").unwrap();
        let $storage = create_storage($dir.path());
    }
}

fn hashid(mut v: u32) -> HashId {
    let mut h = UNKNOWN_HASH;
    h[0] = v as u8; v >>= 8;
    h[1] = v as u8; v >>= 8;
    h[2] = v as u8; v >>= 8;
    h[3] = v as u8;
    h
}

#[test]
fn get_nx_dir_returns_none() {
    init!(dir, storage);

    assert!(storage.getdir(&hashid(42)).unwrap().is_none());
}

#[test]
fn get_nx_obj_returns_none() {
    init!(dir, storage);

    assert!(storage.getobj(&hashid(42)).unwrap().is_none());
}

#[test]
fn committing_empty_transaction_succeeds() {
    init!(dir, storage);

    storage.start_tx(42).unwrap();
    assert!(storage.commit(42).unwrap());
}

#[test]
fn committing_nx_transaction_is_err() {
    init!(dir, storage);

    assert!(storage.commit(42).is_err());
}

#[test]
fn aborting_nx_transaction_is_err() {
    init!(dir, storage);

    assert!(storage.abort(42).is_err());
}

#[test]
fn transaction_number_can_be_reused_after_commit() {
    init!(dir, storage);

    storage.start_tx(42).unwrap();
    assert!(storage.commit(42).unwrap());
    storage.start_tx(42).unwrap();
    assert!(storage.commit(42).unwrap());
}

#[test]
fn mkdir_becomes_visible_after_commit_but_not_before() {
    init!(dir, storage);

    storage.start_tx(1).unwrap();
    storage.mkdir(1, &hashid(1), &hashid(2), &hashid(3),
                  b"hello world").unwrap();

    assert!(storage.getdir(&hashid(1)).unwrap().is_none());

    assert!(storage.commit(1).unwrap());

    let (created_version, data) = storage.getdir(&hashid(1)).unwrap().unwrap();
    assert_eq!(hashid(2), created_version);
    assert_eq!(b"hello world", &data[..]);
}

#[test]
fn mkdir_wont_replace_existing_dir() {
    init!(dir, storage);

    storage.start_tx(1).unwrap();
    storage.mkdir(1, &hashid(1), &hashid(2), &hashid(3),
                  b"hello world").unwrap();
    assert!(storage.commit(1).unwrap());

    storage.start_tx(2).unwrap();
    storage.mkdir(2, &hashid(1), &hashid(4), &hashid(5),
                  b"goodbye world").unwrap();
    assert!(!storage.commit(2).unwrap());
}

#[test]
fn transaction_rolled_back_if_fails() {
    init!(dir, storage);

    storage.start_tx(1).unwrap();
    storage.mkdir(1, &hashid(1), &hashid(2), &hashid(3),
                  b"hello world").unwrap();
    storage.updir(1, &hashid(42), &hashid(43), 99, b"blah").unwrap();
    assert!(!storage.commit(1).unwrap());

    assert!(storage.getdir(&hashid(1)).unwrap().is_none())
}

#[test]
fn updir_visible_after_commit_but_not_before() {
    init!(dir, storage);

    storage.start_tx(1).unwrap();
    storage.mkdir(1, &hashid(1), &hashid(2), &hashid(3),
                  b"hello").unwrap();
    assert!(storage.commit(1).unwrap());

    storage.start_tx(2).unwrap();
    storage.updir(2, &hashid(1), &hashid(3), 5, b" world").unwrap();

    assert_eq!(b"hello", &storage.getdir(&hashid(1)).unwrap().unwrap().1[..]);

    assert!(storage.commit(2).unwrap());

    assert_eq!(b"hello world",
               &storage.getdir(&hashid(1)).unwrap().unwrap().1[..]);
}

#[test]
fn updir_fails_if_length_mismatch() {
    init!(dir, storage);

    storage.start_tx(1).unwrap();
    storage.mkdir(1, &hashid(1), &hashid(2), &hashid(3),
                  b"hello").unwrap();
    assert!(storage.commit(1).unwrap());

    storage.start_tx(2).unwrap();
    storage.updir(2, &hashid(1), &hashid(3), 2, b" world").unwrap();
    assert!(!storage.commit(2).unwrap());

    assert_eq!(b"hello", &storage.getdir(&hashid(1)).unwrap().unwrap().1[..]);
}

#[test]
fn updir_fails_if_version_mismatch() {
    init!(dir, storage);

    storage.start_tx(1).unwrap();
    storage.mkdir(1, &hashid(1), &hashid(2), &hashid(3),
                  b"hello").unwrap();
    assert!(storage.commit(1).unwrap());

    storage.start_tx(2).unwrap();
    storage.updir(2, &hashid(1), &hashid(99), 5, b" world").unwrap();
    assert!(!storage.commit(2).unwrap());

    assert_eq!(b"hello", &storage.getdir(&hashid(1)).unwrap().unwrap().1[..]);
}

#[test]
fn updir_fails_if_normal_version_not_secret_version() {
    init!(dir, storage);

    storage.start_tx(1).unwrap();
    storage.mkdir(1, &hashid(1), &hashid(2), &hashid(3),
                  b"hello").unwrap();
    assert!(storage.commit(1).unwrap());

    storage.start_tx(2).unwrap();
    storage.updir(2, &hashid(1), &hashid(2), 5, b" world").unwrap();
    assert!(!storage.commit(2).unwrap());

    assert_eq!(b"hello", &storage.getdir(&hashid(1)).unwrap().unwrap().1[..]);
}

#[test]
fn updir_overwrites_stray_trailing_data() {
    init!(dir, storage);

    storage.start_tx(1).unwrap();
    storage.mkdir(1, &hashid(1), &hashid(2), &hashid(3),
                  b"hello").unwrap();
    assert!(storage.commit(1).unwrap());

    storage.start_tx(2).unwrap();
    storage.updir(2, &hashid(1), &hashid(3), 5, b" there").unwrap();
    storage.mkdir(2, &hashid(1), &hashid(2), &hashid(3),
                  b"blah").unwrap();
    assert!(!storage.commit(2).unwrap());

    storage.start_tx(3).unwrap();
    storage.updir(3, &hashid(1), &hashid(3), 5, b" world").unwrap();
    assert!(storage.commit(3).unwrap());

    assert_eq!(b"hello world",
               &storage.getdir(&hashid(1)).unwrap().unwrap().1[..]);
}

#[test]
fn rmdir_fails_if_length_mismatch() {
    init!(dir, storage);

    storage.start_tx(1).unwrap();
    storage.mkdir(1, &hashid(1), &hashid(2), &hashid(3),
                  b"hello").unwrap();
    assert!(storage.commit(1).unwrap());

    storage.start_tx(2).unwrap();
    storage.rmdir(2, &hashid(1), &hashid(3), 4).unwrap();
    assert!(!storage.commit(2).unwrap());
}

#[test]
fn rmdir_fails_if_version_mismatch() {
    init!(dir, storage);

    storage.start_tx(1).unwrap();
    storage.mkdir(1, &hashid(1), &hashid(2), &hashid(3),
                  b"hello").unwrap();
    assert!(storage.commit(1).unwrap());

    storage.start_tx(2).unwrap();
    storage.rmdir(2, &hashid(1), &hashid(44), 5).unwrap();
    assert!(!storage.commit(2).unwrap());
}

#[test]
fn rmdir_fails_if_version_normal_instead_of_secret() {
    init!(dir, storage);

    storage.start_tx(1).unwrap();
    storage.mkdir(1, &hashid(1), &hashid(2), &hashid(3),
                  b"hello").unwrap();
    assert!(storage.commit(1).unwrap());

    storage.start_tx(2).unwrap();
    storage.rmdir(2, &hashid(1), &hashid(2), 5).unwrap();
    assert!(!storage.commit(2).unwrap());
}

#[test]
fn rmdir_visible_after_commit_but_not_before() {
    init!(dir, storage);

    storage.start_tx(1).unwrap();
    storage.mkdir(1, &hashid(1), &hashid(2), &hashid(3),
                  b"hello").unwrap();
    assert!(storage.commit(1).unwrap());

    storage.start_tx(2).unwrap();
    storage.rmdir(2, &hashid(1), &hashid(3), 5).unwrap();

    assert_eq!(b"hello", &storage.getdir(&hashid(1)).unwrap().unwrap().1[..]);
    assert!(storage.commit(2).unwrap());
    assert!(storage.getdir(&hashid(1)).unwrap().is_none());
}

#[test]
fn rmdir_then_mkdir_in_same_commit_permitted() {
    init!(dir, storage);

    storage.start_tx(1).unwrap();
    storage.mkdir(1, &hashid(1), &hashid(2), &hashid(3),
                  b"hello").unwrap();
    assert!(storage.commit(1).unwrap());

    storage.start_tx(2).unwrap();
    storage.rmdir(2, &hashid(1), &hashid(3), 5).unwrap();
    storage.mkdir(2, &hashid(1), &hashid(4), &hashid(5),
                  b"new").unwrap();
    assert!(storage.commit(2).unwrap());

    let (ver, data) = storage.getdir(&hashid(1)).unwrap().unwrap();
    assert_eq!(hashid(4), ver);
    assert_eq!(b"new", &data[..]);
}


#[test]
fn check_dirty_dir_handles_nx_length_mismatch_and_ver_mismatch() {
    init!(dir, storage);

    storage.start_tx(1).unwrap();
    storage.mkdir(1, &hashid(1), &hashid(2), &hashid(0),
                  b"unchanged").unwrap();
    storage.mkdir(1, &hashid(3), &hashid(4), &hashid(0),
                  b"length mismatch").unwrap();
    storage.mkdir(1, &hashid(5), &hashid(6), &hashid(0),
                  b"version mismatch").unwrap();
    storage.commit(1).unwrap();

    storage.check_dir_dirty(&hashid(1), &hashid(2), 9).unwrap(); // clean
    storage.check_dir_dirty(&hashid(3), &hashid(4), 3).unwrap(); // length
    storage.check_dir_dirty(&hashid(5), &hashid(0), 16).unwrap(); // version
    storage.check_dir_dirty(&hashid(9), &hashid(0), 42).unwrap(); // unknown

    let mut returned = HashSet::new();
    storage.for_dirty_dir(&mut |id| {
        returned.insert(*id);
        Ok(())
    }).unwrap();

    assert_eq!(3, returned.len());
    assert!(returned.contains(&hashid(3)));
    assert!(returned.contains(&hashid(5)));
    assert!(returned.contains(&hashid(9)));
}

#[test]
fn for_ditry_dir_resets_buffer() {
    init!(dir, storage);

    storage.check_dir_dirty(&hashid(1), &hashid(2), 9).unwrap();
    let mut returned = false;
    storage.for_dirty_dir(&mut |id| {
        assert_eq!(hashid(1), *id);
        assert!(!returned);
        returned = true;
        Ok(())
    }).unwrap();

    storage.for_dirty_dir(&mut |_| panic!("Item returned")).unwrap();
}

#[test]
fn putobj_visible_after_commit() { // and possibly before commit
    init!(dir, storage);

    storage.start_tx(1).unwrap();
    storage.putobj(1, &hashid(1), &hashid(1), b"hello world").unwrap();
    assert!(storage.commit(1).unwrap());

    assert_eq!(b"hello world",
               &storage.getobj(&hashid(1)).unwrap().unwrap()[..]);
}

#[test]
fn cleanup_doesnt_remove_object_with_link() {
    init!(dir, storage);

    storage.start_tx(1).unwrap();
    storage.putobj(1, &hashid(1), &hashid(1), b"hello world").unwrap();
    assert!(storage.commit(1).unwrap());

    storage.clean_up();

    assert_eq!(b"hello world",
               &storage.getobj(&hashid(1)).unwrap().unwrap()[..]);
}

#[test]
fn unlinkobj_removes_link_and_cleanup_removes_obj() {
    init!(dir, storage);

    storage.start_tx(1).unwrap();
    storage.putobj(1, &hashid(1), &hashid(1), b"hello world").unwrap();
    storage.unlinkobj(1, &hashid(1), &hashid(1)).unwrap();
    assert!(storage.commit(1).unwrap());

    storage.clean_up();

    assert!(storage.getobj(&hashid(1)).unwrap().is_none());
}

#[test]
fn linkobj_adds_to_ref_accum() {
    init!(dir, storage);

    storage.start_tx(1).unwrap();
    storage.putobj(1, &hashid(1), &hashid(1), b"hello world").unwrap();
    assert!(storage.commit(1).unwrap());

    storage.start_tx(2).unwrap();
    assert!(storage.linkobj(2, &hashid(1), &hashid(2)).unwrap());
    assert!(storage.commit(2).unwrap());

    storage.clean_up();
    assert!(storage.getobj(&hashid(1)).unwrap().is_some());

    storage.start_tx(3).unwrap();
    storage.unlinkobj(3, &hashid(1), &hashid(1)).unwrap();
    assert!(storage.commit(3).unwrap());

    storage.clean_up();
    assert!(storage.getobj(&hashid(1)).unwrap().is_some());

    storage.start_tx(4).unwrap();
    storage.unlinkobj(4, &hashid(1), &hashid(2)).unwrap();
    assert!(storage.commit(4).unwrap());

    storage.clean_up();
    assert!(storage.getobj(&hashid(1)).unwrap().is_none());
}

#[test]
fn putobj_holds_handle_to_file_to_recreate_if_needed() {
    init!(dir, storage);

    storage.start_tx(1).unwrap();
    storage.putobj(1, &hashid(1), &hashid(1), b"hello world").unwrap();
    // If the implementation creates the object before the commit, this will
    // remove it.
    storage.clean_up();
    assert!(storage.getobj(&hashid(1)).unwrap().is_none());
    // Commit succeeds and recreates with same content
    assert!(storage.commit(1).unwrap());

    assert_eq!(b"hello world",
               &storage.getobj(&hashid(1)).unwrap().unwrap()[..]);
}

#[test]
fn linkobj_holds_handle_to_file_to_recreate_if_needed() {
    init!(dir, storage);

    storage.start_tx(1).unwrap();
    storage.putobj(1, &hashid(1), &hashid(1), b"hello world").unwrap();
    assert!(storage.commit(1).unwrap());

    storage.start_tx(2).unwrap();
    assert!(storage.linkobj(2, &hashid(1), &hashid(2)).unwrap());

    storage.start_tx(3).unwrap();
    storage.unlinkobj(3, &hashid(1), &hashid(1)).unwrap();
    assert!(storage.commit(3).unwrap());

    storage.clean_up();
    assert!(storage.getobj(&hashid(1)).unwrap().is_none());

    assert!(storage.commit(2).unwrap());
    assert_eq!(b"hello world",
               &storage.getobj(&hashid(1)).unwrap().unwrap()[..]);
}

#[test]
fn watch_doesnt_notify_changes_by_self() {
    init!(dir, storage);
    let mut storage = storage;

    let pass = Arc::new(AtomicBool::new(true));
    let pass2 = pass.clone();
    storage.watch(Box::new(move |_| pass2.store(false, SeqCst))).unwrap();

    storage.start_tx(1).unwrap();
    storage.mkdir(1, &hashid(1), &hashid(2), &hashid(0),
                  b"unchanged").unwrap();
    assert!(storage.commit(1).unwrap());

    thread::sleep(Duration::new(10, 0));
    assert!(pass.load(SeqCst));
}

#[test]
fn watch_notifies_changes_by_other() {
    init!(dir, storage1);
    let mut storage1 = storage1;
    let storage2 = create_storage(&dir.path());

    let seen_notification = Arc::new(AtomicUsize::new(0));
    let seen_notification2 = seen_notification.clone();
    let fail = Arc::new(AtomicBool::new(false));
    let fail2 = fail.clone();
    storage1.watch(Box::new(move |id| {
        if let Some(&id) = id {
            if id != hashid(1) {
                fail2.store(true, SeqCst);
            } else {
                seen_notification2.fetch_add(1, SeqCst);
            }
        }
    })).unwrap();

    storage1.start_tx(1).unwrap();
    storage1.mkdir(1, &hashid(1), &hashid(2), &hashid(0),
                   b"plugh").unwrap();
    assert!(storage1.commit(1).unwrap());
    storage1.watchdir(&hashid(1), &hashid(2), 5).unwrap();

    storage2.start_tx(1).unwrap();
    storage2.updir(1, &hashid(1), &hashid(0), 5, b"xyzzy").unwrap();
    assert!(storage2.commit(1).unwrap());

    thread::sleep(Duration::new(10, 0));
    assert!(!fail.load(SeqCst));
    assert_eq!(1, seen_notification.load(SeqCst));
}
