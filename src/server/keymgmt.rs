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

//! Routines for performing high-level key management operations on the server.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use fourleaf;

use crate::defs::HashId;
use crate::errors::*;
use crate::server::crypt::*;
use crate::server::storage::*;
use crate::server::dir::DIRID_KEYS;

/// Caches a `root` internal key.
///
/// Since editing the KDF list requires `root` access, and in many setups all
/// keys are associated with `root`, we want to simply use a `root` key we
/// encounter incidentally if possible instead of needing to prompt the user
/// for it.
#[derive(Default)]
struct RootKey(Option<InternalKey>);

impl RootKey {
    fn chain(&mut self, kc: &KeyChain) {
        if self.0.is_none() {
            self.0 = kc.key(GROUP_ROOT).ok().map(InternalKey::to_owned);
        }
    }
}

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
                                     tx: Tx, old: Option<(&HashId, u32)>,
                                     key: &InternalKey)
                                     -> Result<(HashId, u32)> {
    let new_ver = rand_hashid();
    let new_data = fourleaf::to_vec(kdf)?;

    if let Some((old_ver, old_len)) = old {
        storage.rmdir(tx, &DIRID_KEYS,
                      &secret_dir_ver(&old_ver, key), old_len)?;
    }
    storage.mkdir(tx, &DIRID_KEYS, &new_ver,
                  &secret_dir_ver(&new_ver, key), &new_data)?;
    Ok((new_ver, new_data.len() as u32))
}

fn edit_kdflist<S : Storage + ?Sized, R,
                P : FnMut () -> Result<Vec<u8>>,
                F : FnMut (&mut KdfList, &mut RootKey) -> Result<R>>
    (storage: &S, mut get_root_passphrase: P, mut f: F) -> Result<R>
{
    let mut root_key = RootKey::default();

    do_tx(storage, |tx| {
        let (mut kdflist, old_ver, old_len) = get_kdflist(storage)?
            .ok_or(ErrorKind::KdfListNotExists)?;
        let r = f(&mut kdflist, &mut root_key)?;

        if root_key.0.is_none() {
            let root_passphrase = get_root_passphrase()?;
            root_key.chain(
                &try_derive_key(&root_passphrase, &kdflist.keys)
                    .ok_or(ErrorKind::PassphraseNotInKdfList)?);
        }

        put_kdflist(storage, &kdflist, tx, Some((&old_ver, old_len)),
                    root_key.0.as_ref().ok_or(
                        "Input key not in `root` group")?)?;
        Ok(r)
    })
}

/// Initialises the KDF List with a new internal key set and the given
/// passphrase associated with the default groups.
///
/// Returns the new key chain.
pub fn init_keys<S : Storage + ?Sized>(
    storage: &S, passphrase: &[u8], key_name: &str)
    -> Result<KeyChain>
{
    do_tx(storage, |tx| {
        if get_kdflist(storage)?.is_some() {
            return Err(ErrorKind::KdfListAlreadyExists.into());
        }

        let mut key_chain = KeyChain::generate_new();
        let mut kdflist = KdfList {
            keys: BTreeMap::new(),
            unknown: Default::default(),
        };
        kdflist.keys.insert(
            key_name.to_owned(),
            create_key(passphrase, &mut key_chain,
                       Utc::now(), None));

        put_kdflist(storage, &kdflist, tx, None,
                    key_chain.key(GROUP_ROOT).expect(
                        "Created KeyChain with no `root` group"))?;
        Ok(key_chain)
    })
}

/// Adds `new_passphrase` as a new key named `new_name` to the key store, using
/// `old_passphrase` to derive the key chain.
///
/// The new key will inherit the same groups as the old one.
pub fn add_key<S : Storage + ?Sized, P : FnMut () -> Result<Vec<u8>>>
    (storage: &S, old_passphrase: &[u8], new_passphrase: &[u8], new_name: &str,
     get_root_passphrase: P) -> Result<()>
{
    if new_name.is_empty() {
        return Err(ErrorKind::EmptyKeyName.into());
    }

    edit_kdflist(storage, get_root_passphrase, |kdflist, root_key| {
        let mut key_chain = try_derive_key(old_passphrase, &kdflist.keys)
            .ok_or(ErrorKind::PassphraseNotInKdfList)?;

        // Since the passphrase is used to identify the key implicitly, forbid
        // duplicates.
        if try_derive_key(new_passphrase, &kdflist.keys).is_some() {
            return Err(ErrorKind::PassphraseInKdfList.into());
        }

        root_key.chain(&key_chain);
        if kdflist.keys.insert(
            new_name.to_owned(),
            create_key(new_passphrase, &mut key_chain,
                       Utc::now(), None))
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
/// This fails if `name` identifies the last key in the key store, since
/// removing it would make it impossible to ever derive any internal keys
/// again. It also fails if the key corresponding to `name` is the last key in
/// any particular group.
pub fn del_key<S : Storage + ?Sized, P : FnMut () -> Result<Vec<u8>>>
    (storage: &S, name: &str, get_root_passphrase: P) -> Result<()>
{
    edit_kdflist(storage, get_root_passphrase, |kdflist, _root_key| {
        let old_entry = kdflist.keys.remove(name).ok_or_else(
            || ErrorKind::KeyNotInKdfList(name.to_owned()))?;

        if kdflist.keys.is_empty() {
            return Err(ErrorKind::WouldRemoveLastKdfEntry.into());
        }

        old_entry.groups.keys()
            .filter(|g| !kdflist.keys.values().any(
                |e| e.groups.contains_key(g.as_str())))
            .map(|g| Err(ErrorKind::WouldDisassocLastKeyFromGroup(
                name.to_owned(), g.to_owned())))
            .next().unwrap_or(Ok(()))?;

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
///
/// If the passphrase being changed is not the one being used to derive the
/// internal keys, the latter must be in a superset of groups as the former.
pub fn change_key<S : Storage + ?Sized, P : FnMut () -> Result<Vec<u8>>>(
    storage: &S, old_passphrase: &[u8],
    new_passphrase: &[u8], name: Option<&str>,
    allow_change_via_other_passphrase: bool,
    get_root_passphrase: P)
    -> Result<()>
{
    edit_kdflist(storage, get_root_passphrase, |kdflist, root_key| {
        let real_name = if let Some(name) = name {
            name.to_owned()
        } else if 1 == kdflist.keys.len() {
            kdflist.keys.iter().next().unwrap().0.to_owned()
        } else {
            return Err(ErrorKind::AnonChangeKeyButMultipleKdfEntries.into());
        };

        let old_entry = kdflist.keys.remove(&real_name)
            .ok_or_else(|| ErrorKind::KeyNotInKdfList(real_name.clone()))?;

        // Since the passphrase identifies the key implicitly, we need to make
        // sure that we don't get a duplicate passphrase.
        if try_derive_key(new_passphrase, &kdflist.keys).is_some() {
            return Err(ErrorKind::PassphraseInKdfList.into());
        }

        let key_chain = if let Some(mk) =
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

        root_key.chain(&key_chain);

        let mut new_chain = KeyChain::empty();
        for group in old_entry.groups.keys() {
            new_chain.keys.insert(group.to_owned(),
                                  key_chain.key(group)?.clone());
        }

        kdflist.keys.insert(real_name,
                            create_key(new_passphrase, &mut new_chain,
                                       old_entry.created,
                                       Some(Utc::now())));
        Ok(())
    })
}

/// Fetches the KDF list and uses `passphrase` to derive the key chain.
pub fn derive_key_chain<S : Storage + ?Sized>(storage: &S, passphrase: &[u8])
                                              -> Result<KeyChain> {
    let (kdflist, _, _) = get_kdflist(storage)?
        .ok_or(ErrorKind::KdfListNotExists)?;
    for entry in kdflist.keys.values() {
        if let Some(key_chain) = try_derive_key_single(passphrase, entry) {
            return Ok(key_chain);
        }
    }

    Err(ErrorKind::PassphraseNotInKdfList.into())
}

/// Create a group with each given name on the key with the given passphrase.
///
/// Fails if any group is already defined on any key.
pub fn create_group<S : Storage + ?Sized, IT : Iterator + Clone,
                    P : FnMut () -> Result<Vec<u8>>>
    (storage: &S, passphrase: &[u8], names: IT, get_root_passphrase: P)
     -> Result<()>
where IT::Item : AsRef<str> {
    for name in names.clone() {
        if name.as_ref().is_empty() {
            return Err(ErrorKind::EmptyKeyGroupName.into());
        }
    }

    edit_kdflist(storage, get_root_passphrase, |kdflist, root_key| {
        for name in names.clone() {
            let name = name.as_ref();
            for (_, e) in &kdflist.keys {
                if e.groups.contains_key(name) {
                    return Err(ErrorKind::GroupNameAlreadyInUse(
                        name.to_owned()).into());
                }
            }
        }

        for (_, e) in &mut kdflist.keys {
            if let Some(mut key_chain) =
                try_derive_key_single(passphrase, e)
            {
                root_key.chain(&key_chain);
                for name in names.clone() {
                    let name = name.as_ref();
                    key_chain.keys.insert(name.to_owned(),
                                          InternalKey::generate_new());
                }
                reassoc_keys(e, &mut key_chain);
                return Ok(());
            }
        }

        Err(ErrorKind::PassphraseNotInKdfList.into())
    })
}

/// Adds every group listed in `names` to the entry corresponding to
/// `dst_passphrase`, using `src_passphrase` to derive the internal keys for
/// this transfer.
pub fn assoc_group<S : Storage + ?Sized, IT : Iterator + Clone,
                   P : FnMut () -> Result<Vec<u8>>>
    (storage: &S, src_passphrase: &[u8], dst_passphrase: &[u8],
     names: IT, get_root_passphrase: P) -> Result<()>
where IT::Item : AsRef<str> {
    edit_kdflist(storage, get_root_passphrase, |kdflist, root_key| {
        let src_chain = try_derive_key(src_passphrase, &kdflist.keys)
            .ok_or_else(|| ErrorKind::PassphraseNotInKdfList)?;
        root_key.chain(&src_chain);

        for (_, e) in &mut kdflist.keys {
            if let Some(mut key_chain) =
                try_derive_key_single(dst_passphrase, e)
            {
                root_key.chain(&key_chain);

                for name in names.clone() {
                    let name = name.as_ref();
                    if key_chain.keys.contains_key(name) {
                        return Err(ErrorKind::KeyAlreadyInGroup(
                            name.to_owned()).into());
                    }
                    key_chain.keys.insert(
                        name.to_owned(), src_chain.key(name)?.clone());
                }
                reassoc_keys(e, &mut key_chain);
                return Ok(());
            }
        }

        Err(ErrorKind::PassphraseNotInKdfList.into())
    })
}

/// Disassociates the key named by `key` from all groups named in `names`.
///
/// It is an error to disassociate a group not associated, to disassociate
/// `everyone`, or to disassociate a group which has only one associated key.
pub fn disassoc_group<S : Storage + ?Sized, IT : Iterator + Clone,
                      P : FnMut () -> Result<Vec<u8>>>
    (storage: &S, key: &str, names: IT, get_root_passphrase: P) -> Result<()>
where IT::Item : AsRef<str> {
    for name in names.clone() {
        if GROUP_EVERYONE == name.as_ref() {
            return Err(ErrorKind::CannotDisassocGroup(
                GROUP_EVERYONE.to_owned()).into());
        }
    }

    edit_kdflist(storage, get_root_passphrase, |kdflist, _root_key| {
        {
            let entry = kdflist.keys.get_mut(key).ok_or_else(
                || ErrorKind::KeyNotInKdfList(key.to_owned()))?;
            for name in names.clone() {
                let name = name.as_ref();
                entry.groups.remove(name).ok_or_else(
                    || ErrorKind::KeyNotInGroup(name.to_owned()))?;
            }
        }

        for name in names.clone() {
            let name = name.as_ref();
            if !kdflist.keys.values().any(|e| e.groups.contains_key(name)) {
                return Err(ErrorKind::WouldDisassocLastKeyFromGroup(
                    key.to_owned(), name.to_owned()).into());
            }
        }

        Ok(())
    })
}

/// Removes all occurrences of each named group in the KDF list.
///
/// It is an error to try to destroy the `everyone` or `root` groups.
pub fn destroy_group<S : Storage + ?Sized, IT : Iterator + Clone,
                     P : FnMut () -> Result<Vec<u8>>>
    (storage: &S, names: IT, get_root_passphrase: P) -> Result<()>
where IT::Item : AsRef<str> {
    for name in names.clone() {
        let name = name.as_ref();
        if GROUP_EVERYONE == name || GROUP_ROOT == name {
            return Err(ErrorKind::CannotDestroyGroup(
                name.to_owned()).into());
        }
    }

    edit_kdflist(storage, get_root_passphrase, |kdflist, _root_key| {
        for name in names.clone() {
            let name = name.as_ref();
            let mut found = false;
            for e in kdflist.keys.values_mut() {
                found |= e.groups.remove(name).is_some();
            }

            if !found {
                return Err(ErrorKind::GroupNotInKdfList(name.to_owned())
                           .into());
            }
        }
        Ok(())
    })
}

/// Useful information about a `KdfEntry`, including its name, but excluding
/// binary stuff.
#[derive(Debug, Clone)]
pub struct KeyInfo {
    pub name: String,
    pub algorithm: String,
    pub created: DateTime<Utc>,
    pub updated: Option<DateTime<Utc>>,
    pub groups: Vec<String>,
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
               groups: e.groups.keys().map(|s| s.to_owned()).collect(),
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

    #[allow(unused_imports)] use crate::errors::*;
    use crate::server::local_storage::LocalStorage;
    use super::*;

    fn no_prompt() -> Result<Vec<u8>> {
        panic!("shouldn't prompt");
    }

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
                    add_key(&storage, b"a", b"b", "name", no_prompt));
        assert_err!(ErrorKind::KdfListNotExists,
                    change_key(&storage, b"a", b"b", None, false, no_prompt));
        assert_err!(ErrorKind::KdfListNotExists,
                    del_key(&storage, "name", no_prompt));
        assert!(list_keys(&storage).unwrap().is_empty());
    }

    #[test]
    fn init_keys_adds_one_key_but_fails_if_already_init() {
        init!(storage);

        init_keys(&storage, b"hunter2", "name").unwrap();
        assert_err!(ErrorKind::KdfListAlreadyExists,
                    init_keys(&storage, b"hunter3", "name"));
        derive_key_chain(&storage, b"hunter2").unwrap();
        assert_err!(ErrorKind::PassphraseNotInKdfList,
                    derive_key_chain(&storage, b"hunter3"));
    }

    #[test]
    fn add_key_creates_new_key() {
        init!(storage);

        init_keys(&storage, b"hunter2", "original").unwrap();
        add_key(&storage, b"hunter2", b"hunter3", "new", no_prompt).unwrap();

        let mk = derive_key_chain(&storage, b"hunter2").unwrap();
        let mk2 = derive_key_chain(&storage, b"hunter3").unwrap();
        assert_eq!(mk.keys, mk2.keys);
    }

    #[test]
    fn add_key_refuses_empty_name() {
        init!(storage);

        init_keys(&storage, b"hunter2", "original").unwrap();
        assert_err!(ErrorKind::EmptyKeyName,
                    add_key(&storage, b"hunter2", b"hunter3", "",
                            no_prompt));
    }

    #[test]
    fn add_key_wont_overwrite_existing_key() {
        init!(storage);

        init_keys(&storage, b"hunter2", "original").unwrap();
        assert_err!(ErrorKind::KeyNameAlreadyInUse(_),
                    add_key(&storage, b"hunter2", b"hunter3", "original",
                            no_prompt));
    }

    #[test]
    fn add_key_bad_old_pw() {
        init!(storage);

        init_keys(&storage, b"hunter2", "original").unwrap();
        assert_err!(ErrorKind::PassphraseNotInKdfList,
                    add_key(&storage, b"plugh", b"xyzzy", "new", no_prompt));
    }

    #[test]
    fn add_key_refuses_duplicate_pw() {
        init!(storage);

        init_keys(&storage, b"hunter2", "original").unwrap();
        assert_err!(ErrorKind::PassphraseInKdfList,
                    add_key(&storage, b"hunter2", b"hunter2", "new",
                            no_prompt));
    }

    #[test]
    fn change_key_doesnt_need_key_name_if_only_one_key() {
        init!(storage);

        init_keys(&storage, b"hunter2", "original").unwrap();
        let mk = derive_key_chain(&storage, b"hunter2").unwrap();

        change_key(&storage, b"hunter2", b"hunter3", None, false,
                   no_prompt).unwrap();
        let mk2 = derive_key_chain(&storage, b"hunter3").unwrap();
        assert_eq!(mk.keys, mk2.keys);

        assert_err!(ErrorKind::PassphraseNotInKdfList,
                    derive_key_chain(&storage, b"hunter2"));
    }

    #[test]
    fn change_key_fails_if_no_name_but_multiple_keys() {
        init!(storage);

        init_keys(&storage, b"hunter2", "original").unwrap();
        add_key(&storage, b"hunter2", b"hunter3", "new", no_prompt).unwrap();
        assert_err!(ErrorKind::AnonChangeKeyButMultipleKdfEntries,
                    change_key(&storage, b"hunter3", b"hunter4", None, false,
                               no_prompt));
    }

    #[test]
    fn change_key_by_name() {
        init!(storage);

        init_keys(&storage, b"hunter2", "original").unwrap();
        add_key(&storage, b"hunter2", b"hunter3", "new", no_prompt).unwrap();

        change_key(&storage, b"hunter2", b"hunter22", Some("original"), false,
                   no_prompt).unwrap();
        assert_err!(ErrorKind::PassphraseNotInKdfList,
                    derive_key_chain(&storage, b"hunter2"));
        derive_key_chain(&storage, b"hunter22").unwrap();
        derive_key_chain(&storage, b"hunter3").unwrap();

        change_key(&storage, b"hunter3", b"hunter33", Some("new"), false,
                   no_prompt).unwrap();
        assert_err!(ErrorKind::PassphraseNotInKdfList,
                    derive_key_chain(&storage, b"hunter3"));
        derive_key_chain(&storage, b"hunter22").unwrap();
        derive_key_chain(&storage, b"hunter33").unwrap();
    }

    #[test]
    fn change_key_by_name_nx() {
        init!(storage);

        init_keys(&storage, b"hunter2", "original").unwrap();
        assert_err!(ErrorKind::KeyNotInKdfList(_),
                    change_key(&storage, b"hunter2", b"hunter3", Some("new"),
                               false, no_prompt));
    }

    #[test]
    fn change_key_by_default_requires_corresponding_pw_and_name() {
        init!(storage);

        init_keys(&storage, b"hunter2", "original").unwrap();
        add_key(&storage, b"hunter2", b"hunter3", "new", no_prompt).unwrap();

        assert_err!(ErrorKind::ChangeKeyWithPassphraseMismatch,
                    change_key(&storage, b"hunter2", b"hunter33",
                               Some("new"), false, no_prompt));
    }

    #[test]
    fn change_key_allows_forcing_pw_name_mismatch() {
        init!(storage);

        init_keys(&storage, b"hunter2", "original").unwrap();
        add_key(&storage, b"hunter2", b"hunter3", "new", no_prompt).unwrap();

        change_key(&storage, b"hunter2", b"hunter33",
                   Some("new"), true, no_prompt).unwrap();

        let mk = derive_key_chain(&storage, b"hunter2").unwrap();
        let mk2 = derive_key_chain(&storage, b"hunter33").unwrap();
        assert_eq!(mk.keys, mk2.keys);
        assert_err!(ErrorKind::PassphraseNotInKdfList,
                    derive_key_chain(&storage, b"hunter3"));
    }

    #[test]
    fn change_key_bad_old_pw() {
        init!(storage);

        init_keys(&storage, b"hunter2", "original").unwrap();

        assert_err!(ErrorKind::PassphraseNotInKdfList,
                    change_key(&storage, b"plugh", b"xyzzy", None, false,
                               no_prompt));
    }

    #[test]
    fn change_key_dupe_pw() {
        init!(storage);

        init_keys(&storage, b"hunter2", "original").unwrap();
        add_key(&storage, b"hunter2", b"hunter3", "new", no_prompt).unwrap();

        assert_err!(ErrorKind::PassphraseInKdfList,
                    change_key(&storage, b"hunter3", b"hunter2",
                               Some("new"), false, no_prompt));
    }

    #[test]
    fn change_key_other_doesnt_add_groups() {
        init!(storage);

        init_keys(&storage, b"hunter2", "original").unwrap();
        add_key(&storage, b"hunter2", b"hunter3", "new", no_prompt).unwrap();
        create_group(&storage, b"hunter2", ["group"].iter(), no_prompt)
            .unwrap();

        change_key(&storage, b"hunter2", b"hunter33",
                   Some("new"), true, no_prompt).unwrap();

        let mk2 = derive_key_chain(&storage, b"hunter33").unwrap();
        assert_eq!(2, mk2.keys.len());
    }

    #[test]
    fn change_key_other_fails_if_insufficient_groups() {
        init!(storage);

        init_keys(&storage, b"hunter2", "original").unwrap();
        add_key(&storage, b"hunter2", b"hunter3", "new", no_prompt).unwrap();
        create_group(&storage, b"hunter3", ["group"].iter(), no_prompt)
            .unwrap();

        assert_err!(
            ErrorKind::KeyNotInGroup(..),
            change_key(&storage, b"hunter2", b"hunter33",
                       Some("new"), true, no_prompt));
    }

    #[test]
    fn del_key_wont_delete_last_key() {
        init!(storage);

        init_keys(&storage, b"hunter2", "original").unwrap();
        assert_err!(ErrorKind::WouldRemoveLastKdfEntry,
                    del_key(&storage, "original", no_prompt));
    }

    #[test]
    fn del_key_wont_delete_last_key_in_group() {
        init!(storage);

        init_keys(&storage, b"hunter2", "original").unwrap();
        add_key(&storage, b"hunter2", b"hunter3", "new", no_prompt).unwrap();
        create_group(&storage, b"hunter3", ["group"].iter(),
                     no_prompt).unwrap();
        assert_err!(ErrorKind::WouldDisassocLastKeyFromGroup(..),
                    del_key(&storage, "new", no_prompt));
    }

    #[test]
    fn del_key_name_nx() {
        init!(storage);

        init_keys(&storage, b"hunter2", "original").unwrap();
        assert_err!(ErrorKind::KeyNotInKdfList(_),
                    del_key(&storage, "plugh", no_prompt));
    }

    #[test]
    fn del_key_removes_named_key() {
        init!(storage);

        init_keys(&storage, b"hunter2", "original").unwrap();
        add_key(&storage, b"hunter2", b"hunter3", "new", no_prompt).unwrap();

        let mk = derive_key_chain(&storage, b"hunter2").unwrap();

        del_key(&storage, "original",
                || Ok((&b"hunter3"[..]).to_owned())).unwrap();

        let mk2 = derive_key_chain(&storage, b"hunter3").unwrap();
        assert_eq!(mk.keys, mk2.keys);

        assert_err!(ErrorKind::PassphraseNotInKdfList,
                    derive_key_chain(&storage, b"hunter2"));
    }

    #[test]
    fn kdf_timestamps_updated() {
        init!(storage);

        init_keys(&storage, b"hunter2", "original").unwrap();
        let list = list_keys(&storage).unwrap();
        assert_eq!(1, list.len());
        assert!(list[0].updated.is_none());

        change_key(&storage, b"hunter2", b"hunter3", None, false,
                   no_prompt).unwrap();
        let list = list_keys(&storage).unwrap();
        assert_eq!(1, list.len());
        assert!(list[0].updated.is_some());
    }

    #[test]
    fn create_group_already_exists() {
        init!(storage);

        init_keys(&storage, b"hunter2", "original").unwrap();
        assert_err!(ErrorKind::GroupNameAlreadyInUse(_),
                    create_group(&storage, b"hunter2", ["root"].iter(),
                                 no_prompt));
    }

    #[test]
    fn create_group_empty_name() {
        init!(storage);

        init_keys(&storage, b"hunter2", "original").unwrap();
        assert_err!(ErrorKind::EmptyKeyGroupName,
                    create_group(&storage, b"hunter2", [""].iter(),
                                 no_prompt));
    }

    #[test]
    fn create_group_success() {
        init!(storage);

        init_keys(&storage, b"hunter2", "original").unwrap();
        create_group(&storage, b"hunter2", ["users", "private"].iter(),
                     no_prompt).unwrap();

        let mk = derive_key_chain(&storage, b"hunter2").unwrap();
        assert_eq!(4, mk.keys.len());
        assert!(mk.keys.contains_key("root"));
        assert!(mk.keys.contains_key("everyone"));
        assert!(mk.keys.contains_key("users"));
        assert!(mk.keys.contains_key("private"));
    }

    #[test]
    fn assoc_group_nx_group() {
        init!(storage);

        init_keys(&storage, b"hunter2", "original").unwrap();
        add_key(&storage, b"hunter2", b"hunter3", "second",
                no_prompt).unwrap();
        assert_err!(
            ErrorKind::KeyNotInGroup(..),
            assoc_group(&storage, b"hunter2", b"hunter3",
                        ["group"].iter(), no_prompt));
    }

    #[test]
    fn assoc_group_success() {
        init!(storage);

        init_keys(&storage, b"hunter2", "original").unwrap();
        add_key(&storage, b"hunter2", b"hunter3", "second",
                no_prompt).unwrap();
        create_group(&storage, b"hunter2", ["users", "shared", "private"]
                     .iter(), no_prompt).unwrap();
        assoc_group(&storage, b"hunter2", b"hunter3", ["users", "shared"]
                    .iter(), no_prompt).unwrap();

        let mk = derive_key_chain(&storage, b"hunter2").unwrap();
        let mk2 = derive_key_chain(&storage, b"hunter3").unwrap();
        assert_eq!(5, mk.keys.len());
        assert_eq!(4, mk2.keys.len());
        assert_eq!(mk.keys["root"], mk2.keys["root"]);
        assert_eq!(mk.keys["everyone"], mk2.keys["everyone"]);
        assert_eq!(mk.keys["users"], mk2.keys["users"]);
        assert_eq!(mk.keys["shared"], mk2.keys["shared"]);
    }

    #[test]
    fn disassoc_group_refuses_everyone() {
        init!(storage);

        init_keys(&storage, b"hunter2", "original").unwrap();
        assert_err!(ErrorKind::CannotDisassocGroup(_),
                    disassoc_group(&storage, "original",
                                   ["everyone"].iter(), no_prompt));
    }

    #[test]
    fn disassoc_group_refuses_last_key() {
        init!(storage);

        init_keys(&storage, b"hunter2", "original").unwrap();
        add_key(&storage, b"hunter2", b"hunter3", "second",
                no_prompt).unwrap();
        create_group(&storage, b"hunter2", ["group"].iter(),
                     no_prompt).unwrap();
        assert_err!(
            ErrorKind::WouldDisassocLastKeyFromGroup(..),
            disassoc_group(&storage, "original", ["group"].iter(),
                           no_prompt));
    }

    #[test]
    fn disassoc_group_nx_group() {
        init!(storage);

        init_keys(&storage, b"hunter2", "original").unwrap();
        assert_err!(
            ErrorKind::KeyNotInGroup(..),
            disassoc_group(&storage, "original", ["group"].iter(),
                           no_prompt));
    }

    #[test]
    fn disassoc_group_nx_key() {
        init!(storage);

        init_keys(&storage, b"hunter2", "original").unwrap();
        assert_err!(
            ErrorKind::KeyNotInKdfList(..),
            disassoc_group(&storage, "plugh", ["root"].iter(), no_prompt));
    }

    #[test]
    fn disassoc_group_success() {
        init!(storage);

        init_keys(&storage, b"hunter2", "original").unwrap();
        add_key(&storage, b"hunter2", b"hunter3", "second",
                no_prompt).unwrap();
        create_group(&storage, b"hunter2", ["group"].iter(),
                     no_prompt).unwrap();
        assoc_group(&storage, b"hunter2", b"hunter3", ["group"].iter(),
                    no_prompt).unwrap();
        disassoc_group(&storage, "original", ["group", "root"].iter(),
                       || Ok((&b"hunter3"[..]).to_owned())).unwrap();

        let mk = derive_key_chain(&storage, b"hunter2").unwrap();
        let mk2 = derive_key_chain(&storage, b"hunter3").unwrap();
        assert_eq!(1, mk.keys.len());
        assert_eq!(mk2.keys["everyone"], mk.keys["everyone"]);
    }

    #[test]
    fn destroy_group_refuses_builtins() {
        init!(storage);

        init_keys(&storage, b"hunter2", "original").unwrap();
        assert_err!(ErrorKind::CannotDestroyGroup(..),
                    destroy_group(&storage, ["everyone"].iter(), no_prompt));
        assert_err!(ErrorKind::CannotDestroyGroup(..),
                    destroy_group(&storage, ["root"].iter(), no_prompt));
    }

    #[test]
    fn destroy_group_nx() {
        init!(storage);

        init_keys(&storage, b"hunter2", "original").unwrap();
        assert_err!(ErrorKind::GroupNotInKdfList(..),
                    destroy_group(&storage, ["plugh"].iter(), no_prompt));
    }

    #[test]
    fn destroy_group_success() {
        init!(storage);

        init_keys(&storage, b"hunter2", "original").unwrap();
        create_group(&storage, b"hunter2", ["group"].iter(),
                     no_prompt).unwrap();
        add_key(&storage, b"hunter2", b"hunter3", "second",
                no_prompt).unwrap();
        destroy_group(&storage, ["group"].iter(),
                      || Ok((&b"hunter2"[..]).to_owned())).unwrap();

        let mk = derive_key_chain(&storage, b"hunter2").unwrap();
        let mk2 = derive_key_chain(&storage, b"hunter3").unwrap();
        assert_eq!(2, mk.keys.len());
        assert_eq!(2, mk2.keys.len());
        assert_eq!(mk2.keys["everyone"], mk.keys["everyone"]);
        assert_eq!(mk2.keys["root"], mk.keys["root"]);
    }
}
