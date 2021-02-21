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

use regex::Regex;

use crate::errors::*;
use crate::server::crypt::{GROUP_EVERYONE, GROUP_ROOT};

lazy_static! {
    static ref NAME_PATTERN: Regex = Regex::new(
        r#"(?x)
# Prefix
\.ensync\[

# Configs
([^\]]*)

# Suffix
\]"#
    )
    .unwrap();
    static ref VALID_CONFIG_PATTERN: Regex =
        Regex::new(r#"^([a-z]+=[^,\]]+,*)*$"#).unwrap();
    static ref PAIR_PATTERN: Regex =
        Regex::new(r#"([a-z]+)=([^,\]]+)"#).unwrap();
}

/// Contextual configuration for a single server-side directory.
///
/// The root directory has the configuration `DirConfig::default()`.
/// Subdirectories inherit their parents' configuration by default; however,
/// strings embedded in the directory name can be used to override some or all
/// of these properties. (See the notes in `crypt.rs` for a rationale of
/// embedding this configuration in the name.)
///
/// Name-embedded configuration is placed within `.ensync[...]` anywhere within
/// the name. There may be multiple such configuration blocks within one name.
/// (The explicit closing delimiter ensures that a suffix may be added to the
/// name without disturbing the configuration.) Within each block are any
/// number of comma-separated key-value pairs, where the key is one or more
/// ASCII letters, and the value is one or more characters other than comma or
/// right-bracket.
///
/// The keys `r` and `w` set the read and write groups, respectively. `rw` and
/// `wr` can be used to set both at once.
///
/// For example, `subdir.ensync[r=root,w=my-group]` will cause the subdirectory
/// to be in the read group `root` and the write group `my-group`.
/// `subdir.ensync[r=root]` only sets the read group but inherits the write
/// group from its parent. `subdir.ensync[rw=my-group]` sets both groups to
/// `my-group`.
#[derive(Debug, PartialEq, Eq, Clone)]
pub struct DirConfig {
    /// The name of the key group which may read this directory.
    ///
    /// This dictates the key used to encrypt and decrypt the directory
    /// content, so strictly speaking this also prevents writing sensibly to
    /// the directory if not in this group.
    ///
    /// Defaults to `everyone` at the root.
    pub read_group: String,
    /// The name of the key group used to control access.
    ///
    /// This dictates the key used for the HMAC of the secret directory version
    /// to effect write control by a compliant server.
    ///
    /// Defaults to `root` at the root.
    pub write_group: String,
}

impl Default for DirConfig {
    fn default() -> Self {
        DirConfig {
            read_group: GROUP_EVERYONE.to_owned(),
            write_group: GROUP_ROOT.to_owned(),
        }
    }
}

impl DirConfig {
    /// Returns the configuration to use for the given subdirectory name.
    ///
    /// Examines the given `name` for directory configuration. If any
    /// configuration is parsed, it is applied differentially to a clone of
    /// `self`. When all configuration has been parsed, the new value is
    /// returned.
    ///
    /// Returns an error if the name contains something looking like an ensync
    /// directory configuration but which is not understood by this
    /// implementation.
    pub fn sub(&self, name: &str) -> Result<Self> {
        let mut new = self.clone();

        for captures in NAME_PATTERN.captures_iter(name) {
            let configs = &captures[1];
            if !VALID_CONFIG_PATTERN.is_match(configs) {
                return Err(
                    ErrorKind::BadServerDirConfig(configs.to_owned()).into()
                );
            }

            for captures in PAIR_PATTERN.captures_iter(configs) {
                let key = &captures[1];
                let value = &captures[2];
                match key {
                    "r" => new.read_group = value.to_owned(),
                    "w" => new.write_group = value.to_owned(),
                    "rw" | "wr" => {
                        new.read_group = value.to_owned();
                        new.write_group = value.to_owned();
                    }
                    _ => {
                        return Err(ErrorKind::BadServerDirConfigKey(
                            configs.to_owned(),
                            key.to_owned(),
                        )
                        .into())
                    }
                }
            }
        }

        Ok(new)
    }
}

#[cfg(test)]
mod test {
    use super::DirConfig;
    use crate::errors::*;

    #[test]
    fn sub_no_config() {
        let root = DirConfig::default();
        let sub = root.sub("plugh").unwrap();
        assert_eq!(root, sub);
    }

    #[test]
    fn sub_config_r() {
        let root = DirConfig::default();
        let sub = root.sub("plugh.ensync[r=my-group]").unwrap();
        assert_eq!("root", &sub.write_group);
        assert_eq!("my-group", &sub.read_group);
    }

    #[test]
    fn sub_config_w() {
        let root = DirConfig::default();
        let sub = root.sub("plugh.ensync[w=my-group]").unwrap();
        assert_eq!("my-group", &sub.write_group);
        assert_eq!("everyone", &sub.read_group);
    }

    #[test]
    fn sub_config_rw() {
        let root = DirConfig::default();
        let sub = root.sub("plugh.ensync[rw=my-group]").unwrap();
        assert_eq!("my-group", &sub.write_group);
        assert_eq!("my-group", &sub.read_group);
    }

    #[test]
    fn sub_config_wr() {
        let root = DirConfig::default();
        let sub = root.sub("plugh.ensync[wr=my-group]").unwrap();
        assert_eq!("my-group", &sub.write_group);
        assert_eq!("my-group", &sub.read_group);
    }

    #[test]
    fn sub_config_r_and_w_one_block() {
        let root = DirConfig::default();
        let sub = root
            .sub("plugh.ensync[r=read-group,w=write-group]")
            .unwrap();
        assert_eq!("write-group", &sub.write_group);
        assert_eq!("read-group", &sub.read_group);
    }

    #[test]
    fn sub_config_r_and_w_two_blocks() {
        let root = DirConfig::default();
        let sub = root
            .sub("plugh.ensync[r=read-group].ensync[w=write-group]")
            .unwrap();
        assert_eq!("write-group", &sub.write_group);
        assert_eq!("read-group", &sub.read_group);
    }

    #[test]
    fn sub_invalid_config_syntax_rejected() {
        match DirConfig::default().sub("plugh.ensync[r]") {
            Ok(_) => panic!("Unexpectedly succeeded"),
            Err(Error(ErrorKind::BadServerDirConfig(..), _)) => (),
            Err(e) => panic!("Failed for wrong reason: {}", e),
        }
    }

    #[test]
    fn sub_unknown_config_key_rejected() {
        match DirConfig::default().sub("plugh.ensync[thing=foo]") {
            Ok(_) => panic!("Unexpectedly succeeded"),
            Err(Error(ErrorKind::BadServerDirConfigKey(..), _)) => (),
            Err(e) => panic!("Failed for wrong reason: {}", e),
        }
    }
}
