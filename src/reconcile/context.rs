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

use std::cmp::{Ord,Ordering};
use std::collections::{BinaryHeap,BTreeMap};
use std::ffi::{CStr,CString};

use defs::*;
use replica::*;
use log::Logger;
use rules::DirRules;

/// A bundle of traits passed to the reconciler.
///
/// This is only intended to be implemented by `Context`. The true purpose of
/// this trait is to essentially provide typedefs, since those are currently
/// not allowed in inherent impls right now for whatever reason.
pub trait Interface<'a> {
    type CliDir : ReplicaDirectory;
    type AncDir : ReplicaDirectory;
    type SrvDir : ReplicaDirectory;
    type Cli : 'a + Replica<Directory = Self::CliDir>;
    type Anc : 'a + Replica<Directory = Self::AncDir> + NullTransfer + Condemn;
    type Srv : 'a + Replica<Directory = Self::SrvDir,
                            TransferIn = <Self::Cli as Replica>::TransferOut,
                            TransferOut = <Self::Cli as Replica>::TransferIn>;
    type Log : 'a + Logger;
    type Rules : 'a + DirRules;

    fn cli(&self) -> &Self::Cli;
    fn anc(&self) -> &Self::Anc;
    fn srv(&self) -> &Self::Srv;
    fn log(&self) -> &Self::Log;
    fn root_rules(&self) -> Self::Rules;
}

pub struct Context<'a,
                   CLI : 'a + Replica,
                   ANC : 'a + Replica + NullTransfer + Condemn,
                   SRV : 'a + Replica<TransferIn = CLI::TransferOut,
                                      TransferOut = CLI::TransferIn>,
                   LOG : 'a + Logger,
                   RULES : 'a + DirRules> {
    pub cli: &'a CLI,
    pub anc: &'a ANC,
    pub srv: &'a SRV,
    pub logger: &'a LOG,
    pub root_rules: RULES,
}

impl<'a,
     CLI : 'a + Replica,
     ANC : 'a + Replica + NullTransfer + Condemn,
     SRV : 'a + Replica<TransferIn = CLI::TransferOut,
                        TransferOut = CLI::TransferIn>,
     LOG : 'a + Logger,
     RULES : 'a + DirRules>
Interface<'a> for Context<'a, CLI, ANC, SRV, LOG, RULES> {
    type Cli = CLI;
    type CliDir = CLI::Directory;
    type Anc = ANC;
    type AncDir = ANC::Directory;
    type Srv = SRV;
    type SrvDir = SRV::Directory;
    type Log = LOG;
    type Rules = RULES;

    fn cli(&self) -> &Self::Cli { self.cli }
    fn anc(&self) -> &Self::Anc { self.anc }
    fn srv(&self) -> &Self::Srv { self.srv }
    fn log(&self) -> &Self::Log { self.logger }
    fn root_rules(&self) -> Self::Rules { self.root_rules.clone() }
}


/// Directory-specific context information for a single replica.
pub struct SingleDirContext<T> {
    /// The `Replica::Directory` object.
    pub dir: T,
    /// The files in this directory when it was listed.
    pub files: BTreeMap<CString,FileData>,
}

/// Directory-specific context information.
pub struct DirContext<'a, I : Interface<'a>> {
    pub cli: SingleDirContext<I::CliDir>,
    pub anc: SingleDirContext<I::AncDir>,
    pub srv: SingleDirContext<I::SrvDir>,
    /// Queue of filenames to process. Files are processed in approximately
    /// asciibetical order so that informational messages can give a rough idea
    /// of progress.
    pub todo: BinaryHeap<Reversed<CString>>,
    pub rules: I::Rules,
}

impl<'a, I : Interface<'a>> DirContext<'a,I> {
    pub fn name_in_use(&self, name: &CStr) -> bool {
        self.cli.files.contains_key(name) ||
            self.anc.files.contains_key(name) ||
            self.srv.files.contains_key(name)
    }
}

#[derive(PartialEq,Eq)]
pub struct Reversed<T : Ord + PartialEq + Eq>(pub T);
impl<T : Ord> PartialOrd<Reversed<T>> for Reversed<T> {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl<T : Ord> Ord for Reversed<T> {
    fn cmp(&self, other: &Self) -> Ordering {
        other.0.cmp(&self.0)
    }
}
