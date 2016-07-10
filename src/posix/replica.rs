//-
// Copyright (c) 2016, Jason Lingle
//
// Permission to  use, copy,  modify, and/or distribute  this software  for any
// purpose  with or  without fee  is hereby  granted, provided  that the  above
// copyright notice and this permission notice appear in all copies.
//
// THE SOFTWARE  IS PROVIDED "AS  IS" AND  THE AUTHOR DISCLAIMS  ALL WARRANTIES
// WITH  REGARD   TO  THIS  SOFTWARE   INCLUDING  ALL  IMPLIED   WARRANTIES  OF
// MERCHANTABILITY AND FITNESS. IN NO EVENT  SHALL THE AUTHOR BE LIABLE FOR ANY
// SPECIAL,  DIRECT,   INDIRECT,  OR  CONSEQUENTIAL  DAMAGES   OR  ANY  DAMAGES
// WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS, WHETHER IN AN ACTION
// OF  CONTRACT, NEGLIGENCE  OR OTHER  TORTIOUS ACTION,  ARISING OUT  OF OR  IN
// CONNECTION WITH THE USE OR PERFORMANCE OF THIS SOFTWARE.

#![allow(dead_code)]

use std::ffi::{CStr,CString,OsStr};
use std::os::unix::ffi::OsStrExt;
use std::sync::{Arc,Mutex};

use defs::*;
use replica::*;
use block_xfer::*;
use super::dao::{self,Dao,InodeStatus};
use super::dir::*;

pub struct PosixReplica {
    hmac_secret: Vec<u8>,
    root: CString,
    private_dir: CString,
    // Since the transfer objects need to write back into the DAO, and the lack
    // of HKTs means they can't hold a reference to the replica itself, we need
    // to use Arc to allow the DAO to be shared explicitly.
    dao: Arc<Mutex<Dao>>,
}

impl Replica for PosixReplica {
    type Directory = DirHandle;
    type TransferIn = Option<Box<ContentAddressableSource>>;
    type TransferOut = Option<Box<StreamSource>>;

    fn is_dir_dirty(&self, dir: &DirHandle) -> bool {
        return !self.dao.lock().unwrap().is_dir_clean(dir.full_path())
            .unwrap_or(true)
    }

    fn set_dir_clean(&self, dir: &DirHandle) -> Result<bool> {
        try!(self.dao.lock().unwrap().set_dir_clean(
            dir.full_path(), &dir.hash()));
        Ok(true)
    }

    fn root(&self) -> Result<DirHandle> {
        Ok(DirHandle::root(self.root.clone()))
    }

    fn list(&self, dir: &mut DirHandle) -> Result<Vec<(CString,FileData)>> {
        unimplemented!()
    }

    fn rename(&self, dir: &mut DirHandle, old: &CStr, new: &CStr)
              -> Result<()> {
        unimplemented!()
    }

    fn remove(&self, dir: &mut DirHandle, target: File) -> Result<()> {
        unimplemented!()
    }

    fn create(&self, dir: &mut DirHandle, source: File,
              xfer: Option<Box<ContentAddressableSource>>)
              -> Result<FileData> {
        unimplemented!()
    }

    fn update(&self, dir: &mut DirHandle, name: &CStr,
              old: &FileData, new: &FileData,
              xfer: Option<Box<ContentAddressableSource>>)
              -> Result<FileData> {
        unimplemented!()
    }

    fn chdir(&self, dir: &DirHandle, subdir: &CStr)
             -> Result<DirHandle> {
        unimplemented!()
    }

    fn synthdir(&self, dir: &mut DirHandle, subdir: &CStr, mode: FileMode)
                -> DirHandle {
        unimplemented!()
    }

    fn rmdir(&self, dir: &mut DirHandle) -> Result<()> {
        unimplemented!()
    }

    fn transfer(&self, dir: &DirHandle, file: File)
                -> Option<Box<StreamSource>> {
        unimplemented!()
    }

    fn prepare(&self) -> Result<()> {
        unimplemented!()
    }

    fn clean_up(&self) -> Result<()> {
        unimplemented!()
    }
}
