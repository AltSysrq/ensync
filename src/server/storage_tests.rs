// This is not a separate module, but an include file which defines all the
// tests for `server::Storage` implementations.
//
// The includer is expected to define a `fn create_storage(dir: &Path) -> impl
// Storage` function. This should be placed after the `include!`.
//
// These tests assume that the implementation uses a directory on the local
// filesystem with the same layout as `LocalStorage`, and will in some cases
// act on this assumption to simulate interference from concurrent processes.

use std::path::Path;

use tempdir::TempDir;

use defs::*;
use server::storage::*;

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
fn for_dir_on_empty_storage_does_nothing() {
    init!(dir, storage);

    storage.for_dir(|_, _, _| panic!("for_dir emitted a value")).unwrap();
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
    storage.mkdir(1, &hashid(1), &hashid(2), b"hello world").unwrap();

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
    storage.mkdir(1, &hashid(1), &hashid(2), b"hello world").unwrap();
    assert!(storage.commit(1).unwrap());

    storage.start_tx(2).unwrap();
    storage.mkdir(2, &hashid(1), &hashid(3), b"goodbye world").unwrap();
    assert!(!storage.commit(2).unwrap());
}

#[test]
fn transaction_rolled_back_if_fails() {
    init!(dir, storage);

    storage.start_tx(1).unwrap();
    storage.mkdir(1, &hashid(1), &hashid(2), b"hello world").unwrap();
    storage.updir(1, &hashid(42), &hashid(43), 99, b"blah").unwrap();
    assert!(!storage.commit(1).unwrap());

    assert!(storage.getdir(&hashid(1)).unwrap().is_none())
}

#[test]
fn updir_visible_after_commit_but_not_before() {
    init!(dir, storage);

    storage.start_tx(1).unwrap();
    storage.mkdir(1, &hashid(1), &hashid(2), b"hello").unwrap();
    assert!(storage.commit(1).unwrap());

    storage.start_tx(2).unwrap();
    storage.updir(2, &hashid(1), &hashid(2), 5, b" world").unwrap();

    assert_eq!(b"hello", &storage.getdir(&hashid(1)).unwrap().unwrap().1[..]);

    assert!(storage.commit(2).unwrap());

    assert_eq!(b"hello world",
               &storage.getdir(&hashid(1)).unwrap().unwrap().1[..]);
}

#[test]
fn updir_fails_if_length_mismatch() {
    init!(dir, storage);

    storage.start_tx(1).unwrap();
    storage.mkdir(1, &hashid(1), &hashid(2), b"hello").unwrap();
    assert!(storage.commit(1).unwrap());

    storage.start_tx(2).unwrap();
    storage.updir(2, &hashid(1), &hashid(2), 2, b" world").unwrap();
    assert!(!storage.commit(2).unwrap());

    assert_eq!(b"hello", &storage.getdir(&hashid(1)).unwrap().unwrap().1[..]);
}

#[test]
fn updir_fails_if_version_mismatch() {
    init!(dir, storage);

    storage.start_tx(1).unwrap();
    storage.mkdir(1, &hashid(1), &hashid(2), b"hello").unwrap();
    assert!(storage.commit(1).unwrap());

    storage.start_tx(2).unwrap();
    storage.updir(2, &hashid(1), &hashid(99), 5, b" world").unwrap();
    assert!(!storage.commit(2).unwrap());

    assert_eq!(b"hello", &storage.getdir(&hashid(1)).unwrap().unwrap().1[..]);
}

#[test]
fn updir_overwrites_stray_trailing_data() {
    init!(dir, storage);

    storage.start_tx(1).unwrap();
    storage.mkdir(1, &hashid(1), &hashid(2), b"hello").unwrap();
    assert!(storage.commit(1).unwrap());

    storage.start_tx(2).unwrap();
    storage.updir(2, &hashid(1), &hashid(2), 5, b" there").unwrap();
    storage.mkdir(2, &hashid(1), &hashid(2), b"blah").unwrap();
    assert!(!storage.commit(2).unwrap());

    storage.start_tx(3).unwrap();
    storage.updir(3, &hashid(1), &hashid(2), 5, b" world").unwrap();
    assert!(storage.commit(3).unwrap());

    assert_eq!(b"hello world",
               &storage.getdir(&hashid(1)).unwrap().unwrap().1[..]);
}

#[test]
fn rmdir_fails_if_length_mismatch() {
    init!(dir, storage);

    storage.start_tx(1).unwrap();
    storage.mkdir(1, &hashid(1), &hashid(2), b"hello").unwrap();
    assert!(storage.commit(1).unwrap());

    storage.start_tx(2).unwrap();
    storage.rmdir(2, &hashid(1), &hashid(2), 4).unwrap();
    assert!(!storage.commit(2).unwrap());
}

#[test]
fn rmdir_fails_if_version_mismatch() {
    init!(dir, storage);

    storage.start_tx(1).unwrap();
    storage.mkdir(1, &hashid(1), &hashid(2), b"hello").unwrap();
    assert!(storage.commit(1).unwrap());

    storage.start_tx(2).unwrap();
    storage.rmdir(2, &hashid(1), &hashid(44), 5).unwrap();
    assert!(!storage.commit(2).unwrap());
}

#[test]
fn rmdir_visible_after_commit_but_not_before() {
    init!(dir, storage);

    storage.start_tx(1).unwrap();
    storage.mkdir(1, &hashid(1), &hashid(2), b"hello").unwrap();
    assert!(storage.commit(1).unwrap());

    storage.start_tx(2).unwrap();
    storage.rmdir(2, &hashid(1), &hashid(2), 5).unwrap();

    assert_eq!(b"hello", &storage.getdir(&hashid(1)).unwrap().unwrap().1[..]);
    assert!(storage.commit(2).unwrap());
    assert!(storage.getdir(&hashid(1)).unwrap().is_none());
}

#[test]
fn rmdir_then_mkdir_in_same_commit_permitted() {
    init!(dir, storage);

    storage.start_tx(1).unwrap();
    storage.mkdir(1, &hashid(1), &hashid(2), b"hello").unwrap();
    assert!(storage.commit(1).unwrap());

    storage.start_tx(2).unwrap();
    storage.rmdir(2, &hashid(1), &hashid(2), 5).unwrap();
    storage.mkdir(2, &hashid(1), &hashid(3), b"new").unwrap();
    assert!(storage.commit(2).unwrap());

    let (ver, data) = storage.getdir(&hashid(1)).unwrap().unwrap();
    assert_eq!(hashid(3), ver);
    assert_eq!(b"new", &data[..]);
}


#[test]
fn for_dir_iterates_directories() {
    init!(dir, storage);

    storage.start_tx(1).unwrap();
    storage.mkdir(1, &hashid(1), &hashid(2), b"foo").unwrap();
    storage.mkdir(1, &hashid(3), &hashid(4), b"plugh").unwrap();
    assert!(storage.commit(1).unwrap());

    let mut seen_foo = false;
    let mut seen_bar = false;
    storage.for_dir(|id, ver, len| {
        if hashid(1) == *id {
            assert!(!seen_foo);
            seen_foo = true;
            assert_eq!(hashid(2), *ver);
            assert_eq!(3, len);
        } else if hashid(3) == *id {
            assert!(!seen_bar);
            seen_bar = true;
            assert_eq!(hashid(4), *ver);
            assert_eq!(5, len);
        } else {
            panic!("Unexpected id: {:?}", id);
        }
        Ok(())
    }).unwrap();
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