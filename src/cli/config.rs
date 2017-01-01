//-
// Copyright (c) 2016, Jason Lingle
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

use std::env;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process;
use std::result::Result as StdResult;
use std::str::FromStr;

use rpassword;
use toml;

use defs::PRIVATE_DIR_NAME;
use errors::*;
use rules::engine::SyncRules;

const CONFIG_FILE_NAME: &'static str = "config.toml";

#[derive(Clone, Debug)]
pub struct Config {
    /// The path in the local filesystem to use as the client root.
    pub client_root: PathBuf,
    /// The path in the local filesystem to use as the Ensync private
    /// directory. This is derived from the path to the configuration.
    pub private_root: PathBuf,
    /// Where or how to run the server.
    pub server: ServerConfig,
    /// The named root to use within the server storage.
    pub server_root: String,
    /// How to obtain the passphrase for the server storage.
    pub passphrase: PassphraseConfig,
    /// The block size to use for new transfers.
    pub block_size: u32,
    /// The sync rules to use for reconciliation.
    pub sync_rules: SyncRules,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ServerConfig {
    /// Use the given path on the local filesystem as the server.
    Path(String),
    /// Invoke the given shell command to launch the server, using its stdin
    /// and stdout as input and output for `RemoteStorage`.
    Shell(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PassphraseConfig {
    /// Prompt the controlling terminal for the passphrase. Fail if there is no
    /// controlling terminal or an empty string is read.
    Prompt,
    /// Use the given exact string as the passphrase.
    String(String),
    /// Use the binary content, excluding any trailing LF or CR characters, of
    /// the named file as the passphrase. Fail if the file cannot be read.
    File(String),
    /// Invoke the given shell command and use its full binary output,
    /// excluding any trailing LF or CR characters, as the passphrase. Fail if
    /// the command does not exit successfully or emits no output.
    Shell(String),
}

impl Config {
    /// Transform the given path (e.g., provided by the user) into the actual
    /// path for the configuration file.
    ///
    /// The resulting path will always be absolute and will reference what
    /// should be a regular file.
    pub fn file_location<P : AsRef<Path>>(given: P) -> Result<PathBuf> {
        let mut filename = given.as_ref().to_owned();
        if !filename.ends_with(CONFIG_FILE_NAME) {
            filename.push(CONFIG_FILE_NAME);
        }

        if filename.is_relative() {
            let mut cwd = env::current_dir()
                .chain_err(|| "Failed to determine current directory")?;
            cwd.push(&filename);
            filename = cwd;
        }

        Ok(filename)
    }

    /// Loads the configuration from the given path. The path is implicitly
    /// passed through `file_location` so that this function can tolerate
    /// relative paths and references to the whole directory instead of the
    /// configuration itself.
    pub fn read<P : AsRef<Path>>(filename: P) -> Result<Self> {
        let filename = Self::file_location(filename)?;

        let mut text = String::new();
        fs::File::open(&filename).and_then(
            |mut file| file.read_to_string(&mut text))
            .map_err(|e| format!("{}: {}", filename.display(), e))?;

        Self::parse(&filename, &text)
    }

    /// Parses the configuration in `s`. `filename` names the file from which
    /// the text was loaded and must end with `CONFIG_FILE_NAME` and have a
    /// parent.
    pub fn parse<P : AsRef<Path>>(filename: P, s: &str) -> Result<Self> {
        let filename = filename.as_ref();
        assert!(filename.ends_with(CONFIG_FILE_NAME));
        assert!(filename.parent().is_some());

        let mut parser = toml::Parser::new(s);
        let table = parser.parse().ok_or_else(||  {
            let error = &parser.errors[0];
            let (line, col) = parser.to_linecol(error.lo);


            format!("{}: Syntax error in at line {}, column {}: {}",
                    filename.display(), line + 1, col, error.desc)
        })?;

        macro_rules! extract {
            ($from:expr, $section:expr, $type_prefix:expr, $key:expr,
             $type_suffix:expr, $convert:ident, $convert_name:expr) => {
                $from.get($key)
                    .ok_or_else(
                        || format!("{}: Missing {}{}{} under {}",
                                   filename.display(), $type_prefix, $key,
                                   $type_suffix, $section))?
                    .$convert().ok_or_else(
                        || format!("{}: Key '{}' under {} must be {}",
                                   filename.display(), $key, $section,
                                   $convert_name))
            };

            ($from:expr, $section:expr, [$key:ident]) => {
                extract!($from, $section, "section [", stringify!($key),
                         "]", as_table, "a table")
            };

            ($from:expr, $section:expr, $key:ident, str) => {
                extract!($from, $section, "key \"", stringify!($key),
                         "\"", as_str, "a string")
            };

            ($from:expr, $section:expr, $key:ident, i64) => {
                extract!($from, $section, "key \"", stringify!($key),
                         "\"", as_integer, "an integer")
            };
        }

        let general = extract!(table, "top level", [general])?;
        let rules = extract!(table, "top level", [rules])?;

        Ok(Config {
            client_root: extract!(general, "[general]", path, str)?
                .to_owned().into(),

            private_root: filename.parent().expect(
                "filename has no parent").join(PRIVATE_DIR_NAME),

            server: extract!(general, "[general]", server, str)?
                .parse().map_err(|e| format!("{}: {}", filename.display(), e))?,
            server_root: extract!(general, "[general]", server_root, str)?
                .to_owned(),
            passphrase: extract!(general, "[general]", passphrase, str)?
                .parse().map_err(|e| format!("{}: {}", filename.display(), e))?,
            block_size: {
                let bs = extract!(general, "[general]", block_size, i64)?;
                // There is strictly speaking nothing preventing use of really
                // tiny or really large blocks, but it is not useful either, so
                // enforce some mostly arbitrary bounds as a sanity check.
                if bs < 256 {
                    bail!(format!("{}: Block size {} too small (minimum 256)",
                                  filename.display(), bs));
                }
                if bs > 1024*1024*1024 {
                    bail!(format!("{}: Block size {} too large (maximum 1GB)",
                                  filename.display(), bs));
                }
                bs as u32
            },

            sync_rules: SyncRules::parse(&rules, "rules")
                .chain_err(|| format!("{}: Invalid sync rules configuration",
                                      filename.display()))?,
        })
    }
}

impl FromStr for ServerConfig {
    type Err = String;

    fn from_str(s: &str) -> StdResult<Self, String> {
        let colon = s.find(':').ok_or_else(
            || format!("Invalid server config; syntax is `type:value` \
                        (write `path:{}` if you want to sync to the local \
                        path `{}`)", s, s))?;

        let typ = &s[..colon];
        let value = &s[colon+1..];

        match typ {
            "path" => Ok(ServerConfig::Path(value.to_owned())),
            "shell" => Ok(ServerConfig::Shell(value.to_owned())),
            _ => Err(format!("Invalid server config type '{}' \
                              (if `{}` is intended to be an scp-style path, \
                              write something like \
                              `shell:ssh {} ensync server '{}'` instead)",
                             typ, s, typ, value)),
        }
    }
}

impl FromStr for PassphraseConfig {
    type Err = String;

    fn from_str(s: &str) -> StdResult<Self, String> {
        if "prompt" == s {
            return Ok(PassphraseConfig::Prompt);
        }

        let colon = s.find(':').ok_or_else(
            || format!("Invalid passphrase config; syntax is `type` \
                        or `type:value`, and '{}' is not a valid bare type",
                       s))?;

        let typ = &s[..colon];
        let value = &s[colon+1..];
        match typ {
            "string" => Ok(PassphraseConfig::String(value.to_owned())),
            "file" => Ok(PassphraseConfig::File(value.to_owned())),
            "shell" => Ok(PassphraseConfig::Shell(value.to_owned())),
            _ => Err(format!("Invalid passphrase config type '{}'", typ)),
        }
    }
}

impl PassphraseConfig {
    /// Read the value of this passphrase value.
    ///
    /// `what` will be printed in interactive prompts; it should be a noun
    /// phrase like "new password". If `confirm` is true, interactive prompts
    /// will read the password twice and fail if the two attempts do not match.
    ///
    /// Any trailing newlines on the passphrase are implicitly stripped. Empty
    /// passphrases are forbidden.
    pub fn read_passphrase(&self, what: &str, confirm: bool)
                           -> Result<Vec<u8>> {
        let mut data = self.read_passphrase_impl(what, confirm)?;

        // Strip any trailing newlines since these are often left at the end of
        // text files or process output and aren't intended to be part of the
        // password.
        while Some(&b'\n') == data.last() || Some(&b'\r') == data.last() {
            data.pop();
        }

        if data.is_empty() {
            return Err("Password is empty".into());
        }

        Ok(data)
    }

    fn read_passphrase_impl(&self, what: &str, confirm: bool)
                            -> Result<Vec<u8>> {
        match *self {
            PassphraseConfig::Prompt => {
                let first = rpassword::prompt_password_stdout(
                    &format!("Enter {}: ", what))?;
                if confirm {
                    let second = rpassword::prompt_password_stdout(
                        &format!("Retype {}: ", what))?;
                    if first != second {
                        return Err("Passwords do not match".into());
                    }
                }
                Ok(first.into())
            },

            PassphraseConfig::String(ref s) => Ok(s.clone().into()),

            PassphraseConfig::File(ref filename) => {
                let mut data = Vec::new();
                fs::File::open(filename).and_then(
                    |mut file| file.read_to_end(&mut data))
                    .chain_err(
                        || format!("Failed to read passphrase from {}",
                                   filename))?;
                Ok(data)
            },

            PassphraseConfig::Shell(ref command) => {
                // If we ever support Windows this will need to be updated.
                let output = process::Command::new("/bin/sh")
                    .arg("-c")
                    .arg(command)
                    .stderr(process::Stdio::inherit())
                    .stdin(process::Stdio::null())
                    .output()
                    .chain_err(
                        || format!("Failed to execute command `{}`", command))?;
                if !output.status.success() {
                    return Err(format!("Command `{}` failed with {}",
                                       command, output.status).into());
                }

                Ok(output.stdout)
            },
        }
    }
}

#[cfg(test)]
mod test {
    use tempfile::NamedTempFile;

    use super::*;

    #[test]
    fn parse_minimal() {
        let config = Config::parse("/foo/bar/config.toml", r#"
[general]
path = "/the/client/path"
server = "path:/the/server/path"
server_root = "r00t"
passphrase = "prompt"
block_size = 65536

[[rules.root.files]]
mode = "---/---"
"#).unwrap();
        assert_eq!("/the/client/path", config.client_root.to_str().unwrap());
        assert_eq!(ServerConfig::Path("/the/server/path".to_owned()),
                   config.server);
        assert_eq!("r00t", &config.server_root);
        assert_eq!(PassphraseConfig::Prompt, config.passphrase);
        assert_eq!(65536, config.block_size);
    }

    #[test]
    fn parse_server_shell() {
        let sconf: ServerConfig =
            "shell:ssh turist@host.example.org ensync ~/sync"
            .parse().unwrap();

        assert_eq!(ServerConfig::Shell(
                       "ssh turist@host.example.org ensync ~/sync".to_owned()),
                   sconf);
    }

    #[test]
    fn passphrase_from_string() {
        let pconf: PassphraseConfig = "string:hunter2".parse().unwrap();
        assert_eq!(PassphraseConfig::String("hunter2".to_owned()),
                   pconf);
        assert_eq!(b"hunter2", &pconf.read_passphrase("", false).unwrap()[..]);
    }

    #[test]
    fn passphrase_from_file() {
        use std::io::Write;

        let mut tempfile = NamedTempFile::new_in(".").unwrap();
        let configstr = format!(
            "file:{}", tempfile.path().to_str().unwrap());
        let pconf: PassphraseConfig = configstr.parse().unwrap();

        writeln!(&mut*tempfile, "hunter2\r\n").unwrap();
        assert_eq!(b"hunter2", &pconf.read_passphrase("", false).unwrap()[..]);
    }

    #[test]
    fn passphrase_from_shell() {
        // Another thing that won't work on Windows
        let pconf: PassphraseConfig = "shell:printf 'hunter%d\r\n' 2"
            .parse().unwrap();
        assert_eq!(b"hunter2", &pconf.read_passphrase("", false).unwrap()[..]);
    }
}
