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

use std::error::Error;
use std::result::Result as StdResult;

use defs::*;

#[derive(Clone,Debug,PartialEq,Eq)]
pub enum FileStreamEntry<'a, XFER : 'a> {
    File(&'a File, &'a XFER),
    EndDirectory,
    EndStream,
}

pub type Result<T> = StdResult<T, Box<Error>>;

pub trait FileInputStream {
    type Transfer;

    fn next(&self) -> Result<FileStreamEntry<Self::Transfer>>;
    fn subdir<T>(&mut self, body: FnOnce (&mut Self) -> T) -> T;
}

pub trait FileOutputStream {
    type Transfer;
    type InputStream;

    fn transfer(&self, &Self::Transfer) -> Result<HashId>;
    fn put(&mut self, old: Option<&File>, new: Option<&File>) -> Result<()>;
    fn subdir<T>(&mut self, body: FnOnce (&mut Self) -> T) -> T;
    fn cancel_if_empty(&mut self) -> Result<()>;
    fn copy_subdir(&self, src: &mut Self::InputStream) -> Result<()>;
    fn last(&self) -> &File;
}
