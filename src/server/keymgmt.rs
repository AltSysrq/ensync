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

//! Routines for performing high-level key management operations on the server.

use std::collections::BTreeMap;

use chrono::{DateTime, UTC};
use fourleaf;

use defs::HashId;
use errors::*;
use server::crypt::*;
use server::storage::*;
use server::dir::DIRID_KEYS;

fn do_tx<S : Storage + ?Sized, R, F : FnMut (Tx) -> Result<R>>(
    storage: &S, mut f: F) -> Result<R>
{
    // For now just always use a constant since we don't run concurrently with
    // anything.
    let tx = 0;

    for _ in 0..16 {
        storage.start_tx(tx)?;
        match f(tx) {
            Ok(r) => if storage.commit(tx)? {
                return Ok(r);
            }, // else retry transaction
            Err(e) => {
                let _ = storage.abort(tx);
                return Err(e);
            },
        }
    }

    Err(ErrorKind::TooManyTxRetries.into())
}

fn get_kdflist<S : Storage + ?Sized>(
    storage: &S) -> Result<Option<(KdfList, HashId, u32)>>
{
    let mut config = fourleaf::DeConfig::default();
    config.max_blob = 16*1024*1024;
    config.max_collect = 65536;

    if let Some((ver, data)) = storage.getdir(&DIRID_KEYS)? {
        Ok(Some((fourleaf::from_slice_copy(&data, &config)?, ver,
                 data.len() as u32)))
    } else {
        Ok(None)
    }
}

fn put_kdflist<S : Storage + ?Sized>(storage: &S, kdf: &KdfList,
                                     tx: Tx, old: Option<(&HashId, u32)>)
                                     -> Result<(HashId, u32)> {
    let new_ver = rand_hashid();
    let new_data = fourleaf::to_vec(kdf)?;

    if let Some((old_ver, old_len)) = old {
        storage.rmdir(tx, &DIRID_KEYS, old_ver, old_len)?;
    }
    storage.mkdir(tx, &DIRID_KEYS, &new_ver, &new_data)?;
    Ok((new_ver, new_data.len() as u32))
}

fn edit_kdflist<S : Storage + ?Sized, R, F : FnMut (&mut KdfList) -> Result<R>>
    (storage: &S, mut f: F) -> Result<R>
{
    do_tx(storage, |tx| {
        let (mut kdflist, old_ver, old_len) = get_kdflist(storage)?
            .ok_or(ErrorKind::KdfListNotExists)?;
        let r = f(&mut kdflist)?;
        put_kdflist(storage, &kdflist, tx, Some((&old_ver, old_len)))?;
        Ok(r)
    })
}

/// Initialises the KDF List with a new master key and the given passphrase.
pub fn init_keys<S : Storage + ?Sized>(
    storage: &S, passphrase: &[u8], key_name: &str)
    -> Result<()>
{
    do_tx(storage, |tx| {
        if get_kdflist(storage)?.is_some() {
            return Err(ErrorKind::KdfListAlreadyExists.into());
        }

        let master_key = MasterKey::generate_new();
        let mut kdflist = KdfList {
            keys: BTreeMap::new()
        };
        kdflist.keys.insert(
            key_name.to_owned(),
            create_key(passphrase, &master_key,
                       UTC::now(), None, None));

        put_kdflist(storage, &kdflist, tx, None)?;
        Ok(())
    })
}

/// Adds `new_passphrase` as a new key named `new_name` to the key store, using
/// `old_passphrase` to derive the master key.
pub fn add_key<S : Storage + ?Sized>(storage: &S, old_passphrase: &[u8],
                                     new_passphrase: &[u8], new_name: &str)
                                     -> Result<()> {
    edit_kdflist(storage, |kdflist| {
        let master_key = try_derive_key(old_passphrase, &kdflist.keys)
            .ok_or(ErrorKind::PassphraseNotInKdfList)?;
        if kdflist.keys.insert(
            new_name.to_owned(),
            create_key(new_passphrase, &master_key,
                       UTC::now(), None, None))
            .is_some()
        {
            return Err(ErrorKind::KeyNameAlreadyInUse(new_name.to_owned())
                       .into());
        }
        Ok(())
    })
}

/// Deletes the key identified by `name`.
///
/// This does not require being able to derive the master key. We explicitly do
/// not require doing so here so as not to create the illusion that an attacker
/// would need to do so.
///
/// This fails if `name` identifies the last key in the key store, since
/// removing it would make it impossible to ever derive the master key again.
pub fn del_key<S : Storage + ?Sized>(storage: &S, name: &str) -> Result<()> {
    edit_kdflist(storage, |kdflist| {
        kdflist.keys.remove(name).ok_or_else(
            || ErrorKind::KeyNotInKdfList(name.to_owned()))?;

        if kdflist.keys.is_empty() {
            return Err(ErrorKind::WouldRemoveLastKdfEntry.into());
        }
        Ok(())
    })
}

/// Changes the passphrase of a single key.
///
/// If `name` is `Some`, it names the key to edit. Otherwise, there must be
/// exactly one key in the key store, and that key will be edited.
///
/// If `allow_change_via_other_passphrase` is false, this call fails if
/// `old_passphrase` is a valid passphrase in the key store but does not
/// correspond to `name`. If true, `old_passphrase` does not need to correspond
/// to `name`.
pub fn change_key<S : Storage + ?Sized>(
    storage: &S, old_passphrase: &[u8],
    new_passphrase: &[u8], name: Option<&str>,
    allow_change_via_other_passphrase: bool)
    -> Result<()>
{
    edit_kdflist(storage, |kdflist| {
        let real_name = if let Some(name) = name {
            name.to_owned()
        } else if 1 == kdflist.keys.len() {
            kdflist.keys.iter().next().unwrap().0.to_owned()
        } else {
            return Err(ErrorKind::AnonChangeKeyButMultipleKdfEntries.into());
        };

        let old_entry = kdflist.keys.remove(&real_name)
            .ok_or_else(|| ErrorKind::KeyNotInKdfList(real_name.clone()))?;

        let master_key = if let Some(mk) =
            try_derive_key_single(old_passphrase, &old_entry)
        {
            mk
        } else if let Some(mk) = try_derive_key(old_passphrase, &kdflist.keys) {
            if allow_change_via_other_passphrase {
                mk
            } else {
                return Err(ErrorKind::ChangeKeyWithPassphraseMismatch.into());
            }
        } else {
            return Err(ErrorKind::PassphraseNotInKdfList.into());
        };

        kdflist.keys.insert(real_name,
                            create_key(new_passphrase, &master_key,
                                       old_entry.created,
                                       Some(UTC::now()),
                                       old_entry.used));
        Ok(())
    })
}

/// Fetches the KDF list and uses `passphrase` to derive the master key.
///
/// This will also update the last-used time of the matched entry.
pub fn derive_master_key<S : Storage + ?Sized>(storage: &S, passphrase: &[u8])
                                               -> Result<MasterKey> {
    edit_kdflist(storage, |kdflist| {
        for (_, e) in &mut kdflist.keys {
            if let Some(master_key) = try_derive_key_single(passphrase, e) {
                e.used = Some(UTC::now());
                return Ok(master_key);
            }
        }

        Err(ErrorKind::PassphraseNotInKdfList.into())
    })
}

/// Useful information about a `KdfEntry`, including its name, but excluding
/// binary stuff.
#[derive(Debug, Clone)]
pub struct KeyInfo {
    pub name: String,
    pub algorithm: String,
    pub created: DateTime<UTC>,
    pub updated: Option<DateTime<UTC>>,
    pub used: Option<DateTime<UTC>>,
}

/// Fetches the list of keys in the storage.
///
/// If the key store has not been initialised, returns an empty vec.
pub fn list_keys<S : Storage + ?Sized>(storage: &S) -> Result<Vec<KeyInfo>> {
    if let Some((kdflist, _, _)) = get_kdflist(storage)? {
        Ok(kdflist.keys.iter()
           .map(|(name, e)| KeyInfo {
               name: name.clone(),
               algorithm: e.algorithm.clone(),
               created: e.created,
               updated: e.updated,
               used: e.used,
           }).collect())
    } else {
        Ok(vec![])
    }
}

// These tests are going to be extremely slow on debug builds since they call
// into the scrypt stuff.
#[cfg(test)]
mod test {
    use tempdir::TempDir;

    use errors::*;
    use server::local_storage::LocalStorage;
    use super::*;

    macro_rules! init {
        ($storage:ident) => {
            let dir = TempDir::new("keymgmt").unwrap();
            let $storage = LocalStorage::open(dir.path()).unwrap();
        }
    }

    macro_rules! assert_err {
        ($expected:pat, $actual:expr) => { match $actual {
            Ok(_) => panic!("Call succeeded unexpectedly"),
            Err(Error($expected, _)) => { },
            Err(e) => panic!("Error was not the expected error: {:?}", e),
        } }
    }

    #[test]
    fn empty() {
        init!(storage);
        assert_err!(ErrorKind::KdfListNotExists,
                    add_key(&storage, b"a", b"b", "name"));
        assert_err!(ErrorKind::KdfListNotExists,
                    change_key(&storage, b"a", b"b", None, false));
        assert_err!(ErrorKind::KdfListNotExists,
                    del_key(&storage, "name"));
        assert!(list_keys(&storage).unwrap().is_empty());
    }

    #[test]
    fn init_keys_adds_one_key_but_fails_if_already_init() {
        init!(storage);

        init_keys(&storage, b"hunter2", "name").unwrap();
        assert_err!(ErrorKind::KdfListAlreadyExists,
                    init_keys(&storage, b"hunter3", "name"));
        derive_master_key(&storage, b"hunter2").unwrap();
        assert_err!(ErrorKind::PassphraseNotInKdfList,
                    derive_master_key(&storage, b"hunter3"));
    }

    #[test]
    fn add_key_creates_new_key() {
        init!(storage);

        init_keys(&storage, b"hunter2", "original").unwrap();
        add_key(&storage, b"hunter2", b"hunter3", "new").unwrap();

        let mk = derive_master_key(&storage, b"hunter2").unwrap();
        let mk2 = derive_master_key(&storage, b"hunter3").unwrap();
        assert_eq!(mk, mk2);
    }

    #[test]
    fn add_key_wont_overwrite_existing_key() {
        init!(storage);

        init_keys(&storage, b"hunter2", "original").unwrap();
        assert_err!(ErrorKind::KeyNameAlreadyInUse(_),
                    add_key(&storage, b"hunter2", b"hunter3", "original"));
    }

    #[test]
    fn add_key_bad_old_pw() {
        init!(storage);

        init_keys(&storage, b"hunter2", "original").unwrap();
        assert_err!(ErrorKind::PassphraseNotInKdfList,
                    add_key(&storage, b"plugh", b"xyzzy", "new"));
    }

    #[test]
    fn change_key_doesnt_need_key_name_if_only_one_key() {
        init!(storage);

        init_keys(&storage, b"hunter2", "original").unwrap();
        let mk = derive_master_key(&storage, b"hunter2").unwrap();

        change_key(&storage, b"hunter2", b"hunter3", None, false).unwrap();
        let mk2 = derive_master_key(&storage, b"hunter3").unwrap();
        assert_eq!(mk, mk2);

        assert_err!(ErrorKind::PassphraseNotInKdfList,
                    derive_master_key(&storage, b"hunter2"));
    }

    #[test]
    fn change_key_fails_if_no_name_but_multiple_keys() {
        init!(storage);

        init_keys(&storage, b"hunter2", "original").unwrap();
        add_key(&storage, b"hunter2", b"hunter3", "new").unwrap();
        assert_err!(ErrorKind::AnonChangeKeyButMultipleKdfEntries,
                    change_key(&storage, b"hunter3", b"hunter4", None, false));
    }

    #[test]
    fn change_key_by_name() {
        init!(storage);

        init_keys(&storage, b"hunter2", "original").unwrap();
        add_key(&storage, b"hunter2", b"hunter3", "new").unwrap();

        change_key(&storage, b"hunter2", b"hunter22", Some("original"), false)
            .unwrap();
        assert_err!(ErrorKind::PassphraseNotInKdfList,
                    derive_master_key(&storage, b"hunter2"));
        derive_master_key(&storage, b"hunter22").unwrap();
        derive_master_key(&storage, b"hunter3").unwrap();

        change_key(&storage, b"hunter3", b"hunter33", Some("new"), false)
            .unwrap();
        assert_err!(ErrorKind::PassphraseNotInKdfList,
                    derive_master_key(&storage, b"hunter3"));
        derive_master_key(&storage, b"hunter22").unwrap();
        derive_master_key(&storage, b"hunter33").unwrap();
    }

    #[test]
    fn change_key_by_name_nx() {
        init!(storage);

        init_keys(&storage, b"hunter2", "original").unwrap();
        assert_err!(ErrorKind::KeyNotInKdfList(_),
                    change_key(&storage, b"hunter2", b"hunter3", Some("new"),
                               false));
    }

    #[test]
    fn change_key_by_default_requires_corresponding_pw_and_name() {
        init!(storage);

        init_keys(&storage, b"hunter2", "original").unwrap();
        add_key(&storage, b"hunter2", b"hunter3", "new").unwrap();

        assert_err!(ErrorKind::ChangeKeyWithPassphraseMismatch,
                    change_key(&storage, b"hunter2", b"hunter33",
                               Some("new"), false));
    }

    #[test]
    fn change_key_allows_forcing_pw_name_mismatch() {
        init!(storage);

        init_keys(&storage, b"hunter2", "original").unwrap();
        add_key(&storage, b"hunter2", b"hunter3", "new").unwrap();

        change_key(&storage, b"hunter2", b"hunter33",
                   Some("new"), true).unwrap();

        let mk = derive_master_key(&storage, b"hunter2").unwrap();
        let mk2 = derive_master_key(&storage, b"hunter33").unwrap();
        assert_eq!(mk, mk2);
        assert_err!(ErrorKind::PassphraseNotInKdfList,
                    derive_master_key(&storage, b"hunter3"));
    }

    #[test]
    fn change_key_bad_old_pw() {
        init!(storage);

        init_keys(&storage, b"hunter2", "original").unwrap();

        assert_err!(ErrorKind::PassphraseNotInKdfList,
                    change_key(&storage, b"plugh", b"xyzzy", None, false));
    }

    #[test]
    fn del_key_wont_delete_last_key() {
        init!(storage);

        init_keys(&storage, b"hunter2", "original").unwrap();
        assert_err!(ErrorKind::WouldRemoveLastKdfEntry,
                    del_key(&storage, "original"));
    }

    #[test]
    fn del_key_name_nx() {
        init!(storage);

        init_keys(&storage, b"hunter2", "original").unwrap();
        assert_err!(ErrorKind::KeyNotInKdfList(_),
                    del_key(&storage, "plugh"));
    }

    #[test]
    fn del_key_removes_named_key() {
        init!(storage);

        init_keys(&storage, b"hunter2", "original").unwrap();
        add_key(&storage, b"hunter2", b"hunter3", "new").unwrap();

        let mk = derive_master_key(&storage, b"hunter2").unwrap();

        del_key(&storage, "original").unwrap();

        let mk2 = derive_master_key(&storage, b"hunter3").unwrap();
        assert_eq!(mk, mk2);

        assert_err!(ErrorKind::PassphraseNotInKdfList,
                    derive_master_key(&storage, b"hunter2"));
    }

    #[test]
    fn kdf_timestamps_updated() {
        init!(storage);

        init_keys(&storage, b"hunter2", "original").unwrap();
        let list = list_keys(&storage).unwrap();
        assert_eq!(1, list.len());
        assert!(list[0].updated.is_none());
        assert!(list[0].used.is_none());

        change_key(&storage, b"hunter2", b"hunter3", None, false).unwrap();
        let list = list_keys(&storage).unwrap();
        assert_eq!(1, list.len());
        assert!(list[0].updated.is_some());
        assert!(list[0].used.is_none());

        derive_master_key(&storage, b"hunter3").unwrap();
        let list = list_keys(&storage).unwrap();
        assert_eq!(1, list.len());
        assert!(list[0].updated.is_some());
        assert!(list[0].used.is_some());
    }
}
