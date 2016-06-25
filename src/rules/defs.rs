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
use std::error::Error;
use std::str::FromStr;

use defs::File;

/// A single field of a sync mode.
#[derive(Clone,Copy,PartialEq,Eq,Debug,PartialOrd,Ord)]
pub enum SyncModeSetting {
    /// This type of propagation shall not happen.
    Off,
    /// This type of propagation shall happen. Handle conflicts regarding it
    /// conservatively.
    On,
    /// This type of propagation shall happen. In case of conflict, "force" the
    /// resolution to be this particular propagation.
    Force,
}

impl Default for SyncModeSetting {
    fn default() -> Self {
        SyncModeSetting::Off
    }
}

impl SyncModeSetting {
    /// Returns whether the given setting is in any enabled state.
    pub fn on(self) -> bool {
        self >= SyncModeSetting::On
    }

    /// Returns whether the given setting is in any forced state.
    pub fn force(self) -> bool {
        self >= SyncModeSetting::Force
    }

    fn ch(self, when_on: char, when_force: char) -> char {
        use self::SyncModeSetting::*;

        match self {
            Off => '-',
            On => when_on,
            Force => when_force,
        }
    }
}

/// The sync settings for one direction of propagation.
#[derive(Clone,Copy,PartialEq,Eq,Debug,Default)]
pub struct HalfSyncMode {
    /// Whether creates should happen in this direction.
    pub create: SyncModeSetting,
    /// Whether updates should happen in this direction.
    pub update: SyncModeSetting,
    /// Whether deletes should happen in this direction.
    pub delete: SyncModeSetting,
}

impl fmt::Display for HalfSyncMode {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}{}{}", self.create.ch('c','C'),
               self.update.ch('u','U'), self.delete.ch('d','D'))
    }
}

/// A full description of a sync mode.
#[derive(Clone,Copy,PartialEq,Eq,Debug,Default)]
pub struct SyncMode {
    /// Whether particular types of changes should propagate from server to
    /// client.
    pub inbound: HalfSyncMode,
    /// Whether particular types of changes should propagate from client to
    /// server.
    pub outbound: HalfSyncMode,
}

impl fmt::Display for SyncMode {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}/{}", self.inbound, self.outbound)
    }
}

#[derive(Clone,Copy,Debug)]
pub struct SyncModeParseError {
    pub message: &'static str,
    pub offset: usize,
}

impl fmt::Display for SyncModeParseError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl Error for SyncModeParseError {
    fn description(&self) -> &str {
        self.message
    }

    fn cause(&self) -> Option<&Error> { None }
}

impl FromStr for SyncMode {
    type Err = SyncModeParseError;

    fn from_str(s: &str) -> Result<Self, SyncModeParseError> {
        let chars : Vec<_> = s.chars().collect();

        if 7 != chars.len() {
            return Err(SyncModeParseError {
                offset: 0,
                message: "Sync mode must be 7 characters, like \"cud/cud\""
            });
        }

        if '/' != chars[3] {
            return Err(SyncModeParseError {
                offset: 3,
                message: "Sync mode must have a '/' at position 3, \
                          like in \"cud/cud\""
            });
        }

        fn conv(on: char, force: char, actual: char, off: usize)
                -> Result<SyncModeSetting, SyncModeParseError> {
            if '-' == actual {
                Ok(SyncModeSetting::Off)
            } else if on == actual {
                Ok(SyncModeSetting::On)
            } else if force == actual {
                Ok(SyncModeSetting::Force)
            } else {
                Err(SyncModeParseError {
                    offset: off,
                    message: "Illegal character in sync mode; must be in \
                              format like \"cud/cud\", \"---/---\", or \
                              \"CUD/CUD\""
                })
            }
        }

        Ok(SyncMode {
            inbound: HalfSyncMode {
                create: try!(conv('c', 'C', chars[0], 0)),
                update: try!(conv('u', 'U', chars[1], 1)),
                delete: try!(conv('d', 'D', chars[2], 2)),
            },
            outbound: HalfSyncMode {
                create: try!(conv('c', 'C', chars[4], 4)),
                update: try!(conv('u', 'U', chars[5], 5)),
                delete: try!(conv('d', 'D', chars[6], 6)),
            },
        })
    }
}

/// Rules matching context corresponding to a fully-read directory.
pub trait DirRules : Sized + Clone {
    type Builder : DirRulesBuilder<DirRules = Self>;
    type FileRules : FileRules<DirRules = Self>;

    /// Obtains the rules matching context for the given file within this
    /// directory.
    fn file(&self, file: File) -> Self::FileRules;
}

/// Rules matching context corresponding to a single file.
pub trait FileRules : Sized + Clone {
    type DirRules : DirRules<FileRules = Self>;

    /// Returns the sync mode for this particular file.
    fn sync_mode(&self) -> SyncMode;
    /// Creates a builder for a subdirectory corresponding to this file.
    fn subdir(self) -> <Self::DirRules as DirRules>::Builder;
}

/// Builder for a directory rules matching context.
///
/// The builder must be told about each file within the directory, then
/// `build()` called to construct the actual `DirRules`.
pub trait DirRulesBuilder : Sized + Clone {
    type DirRules : DirRules<Builder = Self>;

    /// Informs the builder that the directory contains the given file.
    fn contains(&mut self, file: File);
    /// Finishes construction of the directory rules.
    fn build(self) -> Self::DirRules;
}

#[cfg(test)]
mod test {
    use super::*;
    use super::SyncModeSetting::*;

    #[test]
    fn sync_setting_properties() {
        assert!(!Off.on());
        assert!(!Off.force());
        assert!(On.on());
        assert!(!On.force());
        assert!(Force.on());
        assert!(Force.force());
    }

    #[test]
    fn parse_stringify_null_sync_mode() {
        let mode: SyncMode = "---/---".parse().unwrap();
        assert_eq!(Off, mode.inbound.create);
        assert_eq!(Off, mode.inbound.update);
        assert_eq!(Off, mode.inbound.delete);
        assert_eq!(Off, mode.outbound.create);
        assert_eq!(Off, mode.outbound.update);
        assert_eq!(Off, mode.outbound.delete);
        assert_eq!("---/---", mode.to_string());
    }

    #[test]
    fn parse_stringify_all_on_sync_mode() {
        let mode: SyncMode = "cud/cud".parse().unwrap();
        assert_eq!(On, mode.inbound.create);
        assert_eq!(On, mode.inbound.update);
        assert_eq!(On, mode.inbound.delete);
        assert_eq!(On, mode.outbound.create);
        assert_eq!(On, mode.outbound.update);
        assert_eq!(On, mode.outbound.delete);
        assert_eq!("cud/cud", mode.to_string());
    }

    #[test]
    fn parse_stringify_all_force_sync_mode() {
        let mode: SyncMode = "CUD/CUD".parse().unwrap();
        assert_eq!(Force, mode.inbound.create);
        assert_eq!(Force, mode.inbound.update);
        assert_eq!(Force, mode.inbound.delete);
        assert_eq!(Force, mode.outbound.create);
        assert_eq!(Force, mode.outbound.update);
        assert_eq!(Force, mode.outbound.delete);
        assert_eq!("CUD/CUD", mode.to_string());
    }

    #[test]
    fn parse_stringify_mixed_sync_mode() {
        let mode: SyncMode = "Cu-/c-D".parse().unwrap();
        assert_eq!(Force, mode.inbound.create);
        assert_eq!(On, mode.inbound.update);
        assert_eq!(Off, mode.inbound.delete);
        assert_eq!(On, mode.outbound.create);
        assert_eq!(Off, mode.outbound.update);
        assert_eq!(Force, mode.outbound.delete);
        assert_eq!("Cu-/c-D", mode.to_string());
    }

    #[test]
    fn parse_sync_mode_too_short() {
        assert!("cud/cu".parse::<SyncMode>().is_err());
    }

    #[test]
    fn parse_sync_mode_too_long() {
        assert!("cud/cudd".parse::<SyncMode>().is_err());
    }

    #[test]
    fn parse_sync_mode_no_slash_at_3() {
        assert!("cud:cud".parse::<SyncMode>().is_err());
    }

    #[test]
    fn parse_sync_mode_incorrect_letter() {
        assert!("dud/cud".parse::<SyncMode>().is_err());
    }
}
