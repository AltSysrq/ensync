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

use std::io::{self, Write};

use chrono::{DateTime, UTC};

use errors::*;
use server::*;
use cli::config::*;

pub fn init_keys(config: &Config, storage: &Storage, name: Option<&str>)
                 -> Result<()> {
    keymgmt::init_keys(storage,
                       &config.passphrase.read_passphrase(
                           "new passphrase", true)?[..],
                       name.unwrap_or("original"))
}

pub fn add_key(storage: &Storage, old: &PassphraseConfig,
               new: &PassphraseConfig, name: &str) -> Result<()> {
    let old_pass = old.read_passphrase("old passphrase", false)?;
    let new_pass = new.read_passphrase("new passphrase", true)?;
    keymgmt::add_key(storage, &old_pass, &new_pass, name)
}

pub fn list_keys(storage: &Storage) -> Result<()> {
    fn format_date(date: Option<&DateTime<UTC>>) -> String {
        if let Some(date) = date {
            super::format_date::format_date(date)
        } else {
            "never".to_owned()
        }
    }

    let keys = keymgmt::list_keys(storage)?;
    for key in keys {
        print!("{}:", key.name);
        for group in &key.groups {
            print!(" {}", group);
        }
        println!("");
        println!("  algorithm:    {}", key.algorithm);
        println!("  created:      {}", format_date(Some(&key.created)));
        println!("  last changed: {}", format_date(key.updated.as_ref()));
        println!("  last used:    {}", format_date(key.used.as_ref()));
        println!("");
    }
    Ok(())
}

pub fn change_key(config: &Config, storage: &Storage, old: &PassphraseConfig,
                  new: &PassphraseConfig, name: Option<&str>,
                  allow_change_via_other_passphrase: bool)
                  -> Result<()> {
    let old_pass = old.read_passphrase("old passphrase", false)?;
    let new_pass = new.read_passphrase("new passphrase", true)?;
    keymgmt::change_key(storage, &old_pass, &new_pass, name,
                        allow_change_via_other_passphrase)?;

    if config.passphrase == *old && config.passphrase != *new {
        println!("Don't forget to update the passphrase configuration \
                  in {}", config.full_path().display());
    }
    Ok(())
}

pub fn del_key(storage: &Storage, name: &str) -> Result<()> {
    keymgmt::del_key(storage, name)
}

pub fn create_group<IT : Iterator + Clone>
    (storage: &Storage, key: &PassphraseConfig, names: IT) -> Result<()>
where IT::Item : AsRef<str> {
    let pass = key.read_passphrase("passphrase", false)?;

    keymgmt::create_group(storage, &pass, names)
}

pub fn assoc_group<IT : Iterator + Clone>
    (storage: &Storage, from: &PassphraseConfig, to: &PassphraseConfig,
     names: IT) -> Result<()>
where IT::Item : AsRef<str> {
    let from_pass = from.read_passphrase(
        "passphrase with these groups", false)?;
    let to_pass = to.read_passphrase(
        "passphrase to receive groups", false)?;

    keymgmt::assoc_group(storage, &from_pass, &to_pass, names)
}

pub fn disassoc_group<IT : Iterator + Clone>
    (storage: &Storage, from: &str, names: IT) -> Result<()>
where IT::Item : AsRef<str> {
    keymgmt::disassoc_group(storage, from, names)
}


pub fn destroy_group<IT : Iterator + Clone>
    (storage: &Storage, dont_ask: bool, names: IT) -> Result<()>
where IT::Item : AsRef<str> {
    if !dont_ask {
        print!("\
WARNING: If there are any directories protected by any of these groups, they \n\
will no longer be accessible. This cannot be undone. This means that if you \n\
have a write-protected directory with one of these groups, it will be \n\
impossible to remove it. If you have a read-protected directory with one of \n\
these groups, it and its contents WILL BE RENDERED UNRECOVERABLE FOREVER.\n\
\n\
If you are certain you want to do this, type \"yes\": ");
        let _ = io::stdout().flush();
        let mut yes = String::new();
        let _ = io::stdin().read_line(&mut yes);
        if "yes" != yes.trim() {
            return Err("Not confirmed".into());
        }
    }

    keymgmt::destroy_group(storage, names)
}
