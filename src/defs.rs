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

use std::fmt;

pub type HashId = [u8;32];
pub const UNKNOWN_HASH : HashId = [0;32];

/// Describes the type of a directory pointer.
#[derive(Clone,Copy,PartialEq,Eq,PartialOrd,Ord,Debug)]
pub enum DirPtrType {
    /// The child is a subdirectory.
    Directory,
    /// The child is a symbolic link (or other non-regular file)
    Symlink,
    /// The child is a regular file
    File,
}

/// An entry in a directory. Called a "directory pointer" to disambiguate it
/// from operating system directory entries.
#[derive(Clone,PartialEq,Eq,Debug)]
pub struct DirPtr {
    /// The simple name of this object.
    pub name: String,
    /// The type of this object.
    pub typ: DirPtrType,
    /// The value of this object, ie, its SHA-3 sum.
    pub value: HashId,
}

/// A triple describing the input or output state of reconciliation.
#[derive(Clone,PartialEq,Eq,Debug)]
pub struct SyncTriple {
    pub client: Option<DirPtr>,
    pub ancestor: Option<DirPtr>,
    pub server: Option<DirPtr>,
}

/// One direction of a `SyncMode`.
#[derive(Clone,Copy,PartialEq,Eq,Debug)]
pub struct HalfSyncMode {
    pub create: bool,
    pub update: bool,
    pub delete: bool,
}

/// A full sync mode, describing what actions take place in each direction.
#[derive(Clone,Copy,PartialEq,Eq,Debug)]
pub struct SyncMode {
    pub inbound: HalfSyncMode,
    pub outbound: HalfSyncMode,
}

impl fmt::Display for HalfSyncMode {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        fn chif(ch: char, cnd: bool) -> char {
            if cnd { ch } else { '-' }
        }

        write!(f, "{}{}{}", chif('c', self.create),
               chif('u', self.update), chif('d', self.delete))
    }
}

impl fmt::Display for SyncMode {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}/{}", self.inbound, self.outbound)
    }
}

impl DirPtr {
    pub fn matches(&self, that: &DirPtr) -> bool {
        self.typ == that.typ && self.value == that.value
    }

    pub fn matches_opt(a: &Option<DirPtr>, b: &Option<DirPtr>) -> bool {
        match (a, b) {
            (&None, &None) => true,
            (&None, &Some(_)) | (&Some(_), &None) => false,
            (&Some(ref ai), &Some(ref bi)) => ai.matches(bi),
        }
    }
}
