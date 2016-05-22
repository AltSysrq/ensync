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

use std::cmp::{Ord,Ordering};
use std::collections::{BinaryHeap,BTreeMap};
use std::ffi::{CStr,CString};

use defs::*;
use rules::*;

use log;
use log::{Logger,ReplicaSide,EditDeleteConflictResolution};
use log::{EditEditConflictResolution,ErrorOperation,Log};
use replica::{Replica,ReplicaDirectory,Result,NullTransfer};

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
    type Anc : 'a + Replica<Directory = Self::AncDir> + NullTransfer;
    type Srv : 'a + Replica<Directory = Self::SrvDir,
                            TransferIn = <Self::Cli as Replica>::TransferOut,
                            TransferOut = <Self::Cli as Replica>::TransferIn>;
    type Log : 'a + Logger;
    type Rules : 'a + RulesMatcher;

    fn cli(&self) -> &Self::Cli;
    fn anc(&self) -> &Self::Anc;
    fn srv(&self) -> &Self::Srv;
    fn log(&self) -> &Self::Log;
    fn root_rules(&self) -> Self::Rules;
}

pub struct Context<'a,
                   CLI : 'a + Replica,
                   ANC : 'a + Replica + NullTransfer,
                   SRV : 'a + Replica<TransferIn = CLI::TransferOut,
                                      TransferOut = CLI::TransferIn>,
                   LOG : 'a + Logger,
                   RULES : 'a + RulesMatcher> {
    pub cli: &'a CLI,
    pub anc: &'a ANC,
    pub srv: &'a SRV,
    pub logger: &'a LOG,
    pub root_rules: RULES,
}

impl<'a,
     CLI : 'a + Replica,
     ANC : 'a + Replica + NullTransfer,
     SRV : 'a + Replica<TransferIn = CLI::TransferOut,
                        TransferOut = CLI::TransferIn>,
     LOG : 'a + Logger,
     RULES : 'a + RulesMatcher>
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


#[derive(PartialEq,Eq)]
struct Reversed<T : Ord + PartialEq + Eq>(T);
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

/// Directory-specific context information for a single replica.
struct SingleDirContext<T> {
    /// The `Replica::Directory` object.
    dir: T,
    /// The files in this directory when it was listed.
    files: BTreeMap<CString,FileData>,
}

/// Directory-specific context information.
pub struct DirContext<'a, I : Interface<'a>> {
    cli: SingleDirContext<I::CliDir>,
    anc: SingleDirContext<I::AncDir>,
    srv: SingleDirContext<I::SrvDir>,
    /// Queue of filenames to process. Files are processed in approximately
    /// asciibetical order so that informational messages can give a rough idea
    /// of progress.
    todo: BinaryHeap<Reversed<CString>>,
    rules: I::Rules,
}

impl<'a, I : Interface<'a>> DirContext<'a,I> {
    fn name_in_use(&self, name: &CStr) -> bool {
        self.cli.files.contains_key(name) ||
            self.anc.files.contains_key(name) ||
            self.srv.files.contains_key(name)
    }
}

/// Indicates whether reconciliation should recurse into a subdirectory.
#[derive(Clone,Copy,Debug,PartialEq,Eq)]
enum Recurse {
    /// Do not recurse.
    No,
    /// Recurse, and take the specified action afterwards.
    Recurse(ActionAfterRecurse),
}

/// Describes what to do once a recursion level completes.
#[derive(Clone,Copy,Debug,PartialEq,Eq)]
enum ActionAfterRecurse {
    /// Take no further action.
    Nop,
    /// If the directory we recursed into is now empty, remove it.
    DeleteIfEmpty,
}

#[cfg(test)]
mod test {
    use std::collections::HashSet;
    use std::ffi::{CString,CStr};

    use super::*;
    use rules::*;

    use log::PrintlnLogger;
    use memory_replica::MemoryReplica;

    #[derive(Clone)]
    struct ConstantRules<'a>(&'a SyncMode);

    impl<'a> RulesMatcher for ConstantRules<'a> {
        fn dir_contains(&mut self, _: &CStr) { }
        fn child(&self, _: &CStr) -> Self { self.clone() }
        fn sync_mode(&self) -> SyncMode { *self.0 }
    }

    type TContext<'a> = Context<'a, MemoryReplica, MemoryReplica, MemoryReplica,
                                PrintlnLogger, ConstantRules<'a>>;

    struct Fixture {
        client: MemoryReplica,
        ancestor: MemoryReplica,
        server: MemoryReplica,
        logger: PrintlnLogger,
        mode: SyncMode,
    }

    impl Fixture {
        fn new() -> Self {
            Fixture {
                client: MemoryReplica::empty(),
                ancestor: MemoryReplica::empty(),
                server: MemoryReplica::empty(),
                logger: PrintlnLogger,
                mode: Default::default(),
            }
        }

        fn context(&self) -> TContext {
            Context {
                cli: &self.client,
                anc: &self.ancestor,
                srv: &self.server,
                logger: &self.logger,
                root_rules: ConstantRules(&self.mode),
            }
        }
    }

    fn oss(s: &str) -> CString {
        CString::new(s).unwrap()
    }
}
