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

//! Defines structured logs emitted primarily by the reconciler.
//!
//! The intent is to immediately support normal "verbose" operation while also
//! providing useful output for tests, while also allowing eventual rsync-style
//! itemised output.

#![allow(dead_code)]

use std::error::Error;
use std::ffi::CStr;
use reconcile::compute::{Reconciliation,Conflict};

use defs::*;

pub type LogLevel = u8;
/// Log level indicating an unrecoverable, non-localised error.
pub const FATAL: LogLevel = 0;
/// Log level indicating a localised error.
pub const ERROR: LogLevel = 1;
/// Log level indicating a somewhat surprising situation that can still be
/// handled reasonably, such as edit/edit conflicts.
pub const WARN: LogLevel = 2;
/// Log level indicating the reconciler making changes to one of the real
/// replicas.
pub const EDIT: LogLevel = 3;
/// Log level for informational messages not indicative of problems or changes
/// being made.
pub const INFO: LogLevel = 4;

#[derive(Clone,Copy,Debug,PartialEq,Eq)]
pub enum ReplicaSide {
    Client, Ancestor, Server
}

#[derive(Clone,Copy,Debug,PartialEq,Eq)]
pub enum EditDeleteConflictResolution {
    Delete(ReplicaSide), Resurrect(ReplicaSide)
}

#[derive(Clone,Copy,Debug,PartialEq,Eq)]
pub enum EditEditConflictResolution<'a> {
    Choose(ReplicaSide),
    Rename(ReplicaSide, &'a CStr),
}

#[derive(Clone,Copy,Debug,PartialEq,Eq)]
pub enum ErrorOperation<'a> {
    List,
    MarkClean,
    Chdir(&'a CStr),
    Create(&'a CStr),
    Update(&'a CStr),
    Rename(&'a CStr),
    Remove(&'a CStr),
    Rmdir,
    Access(&'a CStr),
}

#[derive(Clone,Copy,Debug)]
pub enum Log<'a> {
    EnterDirectory(&'a CStr),
    LeaveDirectory(&'a CStr),
    Inspect(&'a CStr, &'a CStr, Reconciliation, Conflict),
    Create(ReplicaSide, &'a CStr, &'a CStr, &'a FileData),
    Update(ReplicaSide, &'a CStr, &'a CStr, &'a FileData, &'a FileData),
    Rename(ReplicaSide, &'a CStr, &'a CStr, &'a CStr),
    Remove(ReplicaSide, &'a CStr, &'a CStr, &'a FileData),
    Rmdir(ReplicaSide, &'a CStr),
    Error(ReplicaSide, &'a CStr, ErrorOperation<'a>, &'a Error),
}

pub trait Logger {
    fn log(&self, level: LogLevel, what: &Log);
}

#[cfg(test)]
mod println_logger {
    use super::*;

    /// Trivial implementation of `Logger` which simply dumps everything (in
    /// debug format) to stdout.
    pub struct PrintlnLogger;

    impl Logger for PrintlnLogger {
        fn log(&self, level: LogLevel, what: &Log) {
            let level_str = match level {
                FATAL => "FATAL",
                ERROR => "ERROR",
                WARN  => " WARN",
                EDIT  => " EDIT",
                INFO  => " INFO",
                _     => "?????",
            };
            println!("[{}] {:?}", level_str, what);
        }
    }
}

#[cfg(test)]
pub use self::println_logger::PrintlnLogger;
