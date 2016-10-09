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
fn starting_duplicate_transaction_is_err() {
    init!(dir, storage);

    storage.start_tx(42).unwrap();
    assert!(storage.start_tx(42).is_err());
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
fn adding_ops_to_nx_transaction_is_err() {
    init!(dir, storage);

    assert!(storage.mkdir(42, &hashid(1), &hashid(2), b"hello world")
            .is_err());
    assert!(storage.updir(42, &hashid(1), &hashid(2), 99, b"hello world")
            .is_err());
    assert!(storage.rmdir(42, &hashid(1), &hashid(2), 99).is_err());
    // Use `putobj` first as it will actually create the object anyway, and
    // then `linkobj` will actually need to interact with the transaction.
    assert!(storage.putobj(42, &hashid(1), &hashid(2), b"hello world")
            .is_err());
    assert!(storage.linkobj(42, &hashid(1), &hashid(2)).is_err());
    assert!(storage.unlinkobj(42, &hashid(1), &hashid(2)).is_err());
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
