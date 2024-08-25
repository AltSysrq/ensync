//-
// Copyright (c) 2016, 2017, 2018, 2021, Jason Lingle
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

use std::error::Error;
use std::fmt;
use std::str::FromStr;

/// A single field of a sync mode.
#[derive(Clone, Copy, PartialEq, Eq, Debug, PartialOrd, Ord)]
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
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
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
        write!(
            f,
            "{}{}{}",
            self.create.ch('c', 'C'),
            self.update.ch('u', 'U'),
            self.delete.ch('d', 'D')
        )
    }
}

/// A full description of a sync mode.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
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

#[derive(Clone, Copy, Debug)]
pub struct SyncModeParseError {
    pub message: &'static str,
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

    fn cause(&self) -> Option<&dyn Error> {
        None
    }
}

impl FromStr for SyncMode {
    type Err = SyncModeParseError;

    fn from_str(s: &str) -> Result<Self, SyncModeParseError> {
        match s {
            "mirror" | "reset-server" => {
                return Ok(SyncMode {
                    inbound: HalfSyncMode {
                        create: SyncModeSetting::Off,
                        update: SyncModeSetting::Off,
                        delete: SyncModeSetting::Off,
                    },
                    outbound: HalfSyncMode {
                        create: SyncModeSetting::Force,
                        update: SyncModeSetting::Force,
                        delete: SyncModeSetting::Force,
                    },
                })
            }

            "reset-client" => {
                return Ok(SyncMode {
                    inbound: HalfSyncMode {
                        create: SyncModeSetting::Force,
                        update: SyncModeSetting::Force,
                        delete: SyncModeSetting::Force,
                    },
                    outbound: HalfSyncMode {
                        create: SyncModeSetting::Off,
                        update: SyncModeSetting::Off,
                        delete: SyncModeSetting::Off,
                    },
                })
            }

            "conservative-sync" | "sync" => {
                return Ok(SyncMode {
                    inbound: HalfSyncMode {
                        create: SyncModeSetting::On,
                        update: SyncModeSetting::On,
                        delete: SyncModeSetting::On,
                    },
                    outbound: HalfSyncMode {
                        create: SyncModeSetting::On,
                        update: SyncModeSetting::On,
                        delete: SyncModeSetting::On,
                    },
                })
            }

            "aggressive-sync" => {
                return Ok(SyncMode {
                    inbound: HalfSyncMode {
                        create: SyncModeSetting::Force,
                        update: SyncModeSetting::Force,
                        delete: SyncModeSetting::Force,
                    },
                    outbound: HalfSyncMode {
                        create: SyncModeSetting::Force,
                        update: SyncModeSetting::Force,
                        delete: SyncModeSetting::Force,
                    },
                })
            }

            _ => (),
        }

        let chars: Vec<_> = s.chars().collect();

        if 7 != chars.len() {
            return Err(SyncModeParseError {
                message: "Sync mode must be 7 characters, like \"cud/cud\"",
            });
        }

        if '/' != chars[3] {
            return Err(SyncModeParseError {
                message: "Sync mode must have a '/' at position 3, \
                          like in \"cud/cud\"",
            });
        }

        fn conv(
            on: char,
            force: char,
            actual: char,
        ) -> Result<SyncModeSetting, SyncModeParseError> {
            if '-' == actual {
                Ok(SyncModeSetting::Off)
            } else if on == actual {
                Ok(SyncModeSetting::On)
            } else if force == actual {
                Ok(SyncModeSetting::Force)
            } else {
                Err(SyncModeParseError {
                    message: "Illegal character in sync mode; must be in \
                              format like \"cud/cud\", \"---/---\", or \
                              \"CUD/CUD\"",
                })
            }
        }

        Ok(SyncMode {
            inbound: HalfSyncMode {
                create: conv('c', 'C', chars[0])?,
                update: conv('u', 'U', chars[1])?,
                delete: conv('d', 'D', chars[2])?,
            },
            outbound: HalfSyncMode {
                create: conv('c', 'C', chars[4])?,
                update: conv('u', 'U', chars[5])?,
                delete: conv('d', 'D', chars[6])?,
            },
        })
    }
}

#[cfg(test)]
mod test {
    use super::SyncModeSetting::*;
    use super::*;

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

    #[test]
    fn parse_sync_mode_aliases() {
        assert_eq!(
            "---/CUD",
            &"mirror".parse::<SyncMode>().unwrap().to_string()
        );
        assert_eq!(
            "---/CUD",
            &"reset-server".parse::<SyncMode>().unwrap().to_string()
        );
        assert_eq!(
            "CUD/---",
            &"reset-client".parse::<SyncMode>().unwrap().to_string()
        );
        assert_eq!("cud/cud", &"sync".parse::<SyncMode>().unwrap().to_string());
        assert_eq!(
            "cud/cud",
            &"conservative-sync".parse::<SyncMode>().unwrap().to_string()
        );
        assert_eq!(
            "CUD/CUD",
            &"aggressive-sync".parse::<SyncMode>().unwrap().to_string()
        );
    }
}
