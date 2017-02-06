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

use std::collections::BTreeSet;
use std::env;
use std::ffi::OsStr;
use std::fs;
use std::io::{self, BufRead, Read, Write};
use std::path::Path;
use std::process;
use std::str::FromStr;
use std::sync::Arc;

use toml;

use cli::cmd_server::SHELL_IDENTITY;
use cli::config::*;
use cli::open_server::{connect_server_storage, open_server_replica};
use defs::{File, FileData};
use errors::*;
use replica::Replica;
use rules::SyncMode;
use server::keymgmt;
use server::{self, LocalStorage, Storage};

// `Path::parent()` for a relative path in the current dir returns "", but
// `exists()` and `is_dir()` return `false` for "".
trait PathExt {
    fn exists2(&self) -> bool;
    fn is_dir2(&self) -> bool;
}
impl PathExt for Path {
    fn exists2(&self) -> bool {
        return self.iter().next().is_none() || self.exists();
    }
    fn is_dir2(&self) -> bool {
        return self.iter().next().is_none() || self.is_dir();
    }
}

pub fn run<CFG : AsRef<Path>, LOC : AsRef<Path>, REM : AsRef<Path>>
    (passphrase: &PassphraseConfig,
     config: CFG, local: LOC, remote: REM) -> Result<()>
{
    let config = config.as_ref();
    let local = local.as_ref();
    let remote = remote.as_ref();
    let cwd = env::current_dir().chain_err(
        || "Failed to determine current directory")?;

    if config.exists() {
        return Err(format!("\
Configuration location '{}' already exists.
The configuration location should be the name of a directory to be created
under an already existing directory.", config.display()).into());
    }

    if config.extension().is_some() {
        confirm(&format!("\
The path '{}' looks like it was intended as a file name rather than a directory
name. You can go ahead with this name anyway, which would result in the
configuration being stored at '{}'.
Use this configuration path? ",
                         config.display(),
                         config.join("config.toml").display()))?;
    }

    if !config.parent().map_or(true, |p| p.exists2()) {
        confirm(&format!("\
Parent of configuration location, '{}', does not exist;
Should it be created too? ", config.parent().unwrap().display()))?;
    }

    if !local.is_dir() {
        return Err(format!("Local path '{}' does not exist",
                           local.display()).into());
    }

    let full_local = cwd.join(local);
    let full_config = cwd.join(config);

    let storage: Arc<Storage>;
    let server_spec: String;
    if let Some((host, remote_path)) = remote.to_str()
        .and_then(|s| s.find(':').map(|ix| (&s[..ix], &s[ix+1..])))
    {
        confirm(&format!("This will set up the following configuration:
Files synced to/from: {}
Encrypted data written to REMOTE path: {} on {}
Server accessed via: ssh {}
Configuration and state stored in: {}

Does this look reasonable? ", full_local.display(), remote_path,
                         host, shell_escape(&host), full_config.display()))?;

        let mut child = process::Command::new("ssh")
            .arg(&host)
            // If we actually connect to a POSIX shell, we get the `0`
            // response. However, if the server is running ensync shell, it
            // ignores these arguments and returns `SHELL_IDENTITY`.
            .arg("printf").arg("'0\\0'").arg("&&").arg("exec").arg("sh")
            .stderr(process::Stdio::inherit())
            .stdin(process::Stdio::piped())
            .stdout(process::Stdio::piped())
            .spawn().chain_err(
                || format!("Failed to run `ssh {}`", shell_escape(&host)))?;

        let mut input = child.stdin.take().expect(
            "Missing stdin pipe on child");
        let mut output = child.stdout.take().expect(
            "Missing stdout pipe on child");
        let ensync_path = configure_server(&mut output, &mut input,
                                           remote_path)?;

        let command;
        if let Some(ensync_path) = ensync_path {
            // Using `exec` ensures that if the process fails to start, the
            // shell exits anyway and we don't end up feeding binary fourleaf
            // data into the shell. We also put the `printf` in the same
            // statement so that we can wait for that byte come in to know that
            // the shell as relinquished stdin.
            writeln!(input, "printf . && exec {} server {}",
                     shell_escape(&ensync_path), shell_escape(remote_path))
                .and_then(|_| input.flush())
                .chain_err(|| "Failed to start server process")?;

            let mut period = [0u8;1];
            output.read_exact(&mut period).chain_err(
                || "Failed to wait for server process to start")?;

            command = format!("ssh -T {} {} server {}",
                              shell_escape(&host),
                              // Awkwardly, we need to double-escape the paths
                              // here since they get passed to `sh -c`.
                              shell_escape(&shell_escape(&ensync_path)),
                              shell_escape(&shell_escape(remote_path)));
        } else {
            command = format!("ssh -T {}", shell_escape(&host));
        }
        server_spec = format!("shell:{}", command);
        storage = Arc::new(
            connect_server_storage(child, input, output, &command)?);
    } else {
        sanity_check_remote(LocalPathAccess, remote)?;

        let full_remote = cwd.join(remote);
        confirm(&format!("This will set up the following configuration:
Files synced to/from: {}
Encrypted data written to LOCAL path: {}
Configuration and state stored in: {}

Does this look reasonable? ", full_local.display(), full_remote.display(),
        full_config.display()))?;

        server_spec = format!("path:{}", full_remote.to_str().unwrap());
        storage = Arc::new(LocalStorage::open(&full_remote).chain_err(
            || "Error opening local storage")?);
    }

    println!("Remote storage initialised successfully.");

    let key_chain = if storage.getdir(&server::DIRID_KEYS).chain_err(
        || "Failed to check whether key store initialised")?.is_none()
    {
        let passphrase = passphrase.read_passphrase(
            "new passphrase", true)?;
        keymgmt::init_keys(&*storage, &passphrase, "original")
            .chain_err(|| "Failed to initialise key store")?
    } else {
        let passphrase = passphrase.read_passphrase(
            "existing passphrase", false)?;
        keymgmt::derive_key_chain(&*storage, &passphrase)?
    };

    let config_file_name = Config::file_location(&full_config)?;
    // Parse a dummy configuration so we get all the derived paths.
    let dummy_config = Config::parse(
        &config_file_name,
        r#"
[general]
path = "\u0000"
server = "path:\u0000"
server_root = "/"
passphrase = "file:/dev/null"
compression = "none"

[[rules.root.files]]
mode = "---/---"
"#).unwrap();

    fs::create_dir_all(&dummy_config.private_root).chain_err(
        || format!("Failed to create '{}'",
                   dummy_config.private_root.display()))?;

    let replica = open_server_replica(&dummy_config, storage.clone(),
                                      Some(Arc::new(key_chain)))?;
    let mut proot = replica.pseudo_root();
    let mut existing_roots = BTreeSet::new();
    for (name, fd) in replica.list(&mut proot).chain_err(
        || "Failed to list logical roots on server")?
    {
        if let FileData::Directory(..) = fd {
            if let Ok(name) = name.into_string() {
                existing_roots.insert(name);
            }
        }
    }

    let chosen_root = if existing_roots.is_empty() {
        println!("\nNo logical roots exist yet.
Enter a name for the new logical root, or just press enter to use \"root\".");
        prompt_line("root", "Logical root name")?
    } else {
        println!("\nThe following logical roots exist:");
        for name in &existing_roots {
            println!("\t{}", name);
        }
        println!("\
If you want to sync with an existing root, enter its name below. Otherwise,
enter a name for a new logical root.");
        let mut selected;
        loop {
            selected = prompt_line(existing_roots.iter().next().unwrap(),
                                   "Logical root name")?;
            if !existing_roots.contains(&selected) {
                if !ask(&format!("Root '{}' does not exist. Create it? ",
                                 selected))?
                { continue; }
            }
            break;
        }
        selected
    };

    if !existing_roots.contains(&chosen_root) {
        replica.create(&mut proot, File(OsStr::new(&chosen_root),
                                        &FileData::Directory(0o600)),
                       None)
            .chain_err(|| format!("Failed to create root '{}'", chosen_root))?;
        println!("Created new logical root '{}'", chosen_root);
    }

    let mut compression;
    println!(r#"
ensync supports transparent compression of files. This can substantially reduce
disk space usage on the remote side as well as reducing network bandwidth, but
does leak some information about the nature of the files to someone who gets
their hands on the encrypted files. It also has a minor performance hit, so you
may also wish to turn it off if you know virtually all your files are
uncompressible.
Valid values are "none", "fast", "default", "best"."#);
    loop {
        compression = prompt_line("best", "Desired compression level")?;
        if parse_compression_name(&config_file_name, &compression).is_ok() {
            break;
        }

        println!("Invalid compression level.");
    }

    let mut sync_mode;
    println!(r#"
The sync mode controls how changes are propagated between the local and remote
replicas. The sync mode is normally specified as a string like "cud/cud", where
each letter indicates a type of propagation being turned on, using hyphens to
indicate to not perform that type of propagation (i.e., "---/---" indicates to
sync nothing).

        ┌─────── "Sync inbound create"
        │        New remote files are downloaded to the local filesystem
        │┌────── "Sync inbound update"
        ││       Edits to remote files are applied to local files
        ││┌───── "Sync inbound delete"
        │││      Files deleted remotely are deleted in the local filesystem
        cud/cud
            │││
            ││└─ "Sync outbound delete"
            ││   Files deleted in the local filesystem are deleted remotely
            │└── "Sync outbound update"
            │    Edits to local files are applied to remote files
            └─── "Sync outbound create"
                 New local files are uploaded to remote storage

Making one of the letters upper-case means "force", i.e., implicitly resolve
conflicts by performing that propagation even if that means nominally losing
data. Setting both update flags to force causes edit conflicts to be resolved
in favour of the version with the later modified time.

You can also use one of the following aliases:

- "conservative-sync". Equivalent to "cud/cud". This will propagate all file
  changes bidirectionally. Conflicts are resolved conservatively; e.g., an edit
  conflict is handled by keeping both versions of the file.

- "aggressive-sync". Equivalent to "CUD/CUD". This is like "conservative-sync",
  but all conflicts are resolved automatically; e.g., an edit conflict results
  in only one version of the file being kept.

- "mirror". Equivalent to "---/CUD". This causes the remote to be a mirror of
  the local filesystem. That is, in any case where the remote disagrees with
  the local state, the remote is changed to match the local state. The local
  filesystem is never modified in this mode.

This will configure using the selected sync mode for all files. It is possible
to use different sync modes in different contexts by editing the configuration
later."#);

    loop {
        sync_mode = prompt_line("conservative-sync", "Desired sync mode")?;
        match SyncMode::from_str(&sync_mode) {
            Ok(_) => break,
            Err(e) => println!("{}", e),
        }
    }

    fs::File::create(&config_file_name).and_then(
        |mut f| writeln!(f, r#"# Ensync configuration file
# Generated by `ensync setup`.

# Relative file names in this file are relative to the directory containing
# this file.
#
# Fields which are references to directory trees to be synced are not generally
# safe to edit after syncing to point to a different directory tree. If you
# really want to do so, make sure to remove the `internal.ensync` directory in
# the same directory as this configuration, which will prevent the state from
# carrying over to the altered configuration.

[general]

# The path to the local files being synced. Only edit this if you actually move
# the directory tree itself; if you simply point it at another directory,
# ensync will think all the contents of the prior location had been deleted.
path = {path}

# What to use as the remote side of syncing. This can have the format
#     `path:/some/path`
# to write to another path on the local filesystem, or
#     `shell:some shell command`
# to execute a shell command. In the latter case, ensync will communicate with
# the remote process via standard input and output. `ensync server` is an
# appropriate command to run on the remote side; typically, this is used in
# conjunction with `ssh` to run the server component on a remote host.
#
# As with the `path` configuration, this should not be edited in a way that
# causes it to point to different content.
server = {server}
# The name of the logical root to use as the effective remote root. The same
# caveat for editing `server` also applies here.
server_root = {server_root}

# How to obtain the passphrase. Can be one of the following:
#       `prompt`        Read interactively from the controlling terminal
#       `string:xxx`    Use `xxx` as the passphrase
#       `file:somefile` Use the content of `somefile` as the passphrase
#       `shell:cmd`     Execute `cmd` in this directory and use its standard
#                       output as the passphrase.
passphrase = {passphrase}

# Whether to use compression, and if so, at what level.
compression = {compression}

# The sync rules output by `ensync setup` simply apply one sync mode to all
# files. The sync rules configuration is much more flexible than this; see
# the documentation for how to better control this if so desired.
[[rules.root.files]]
mode = {sync_mode}
"#,
                     path = toml::Value::String(
                         full_local.to_str().unwrap().to_owned()),
                     server = toml::Value::String(server_spec.clone()),
                     server_root = toml::Value::String(chosen_root.clone()),
                     passphrase = toml::Value::String(
                         passphrase.clone().relativise(&cwd).to_string_lossy()),
                     compression = toml::Value::String(compression.clone()),
                     sync_mode = toml::Value::String(sync_mode.clone()),
        ).and_then(|_| f.flush())).chain_err(
        || format!("Error writing to '{}'", config_file_name.display()))?;

    println!("Wrote configuration to '{}'", config_file_name.display());
    println!("Setup is complete. Edit the configuration if desired, or run\n\
              `ensync sync {}` to start syncing.", config.display());
    Ok(())
}

fn prompt_line(default: &str, prompt: &str) -> Result<String> {
    print!("{} [{}]: ", prompt, default);
    io::stdout().flush()?;

    let mut reply = String::new();
    io::stdin().read_line(&mut reply)?;
    reply = reply.trim().to_owned();

    if reply.is_empty() {
        Ok(default.to_owned())
    } else {
        Ok(reply)
    }
}

fn ask(msg: &str) -> Result<bool> {
    print!("{}", msg);
    io::stdout().flush()?;

    loop {
        let mut yes = String::new();
        io::stdin().read_line(&mut yes)?;

        match yes.chars().next() {
            Some('y') | Some('Y') => return Ok(true),
            Some('n') | Some('N') | None => return Ok(false),
            _ => {
                print!("Please enter 'y' or 'n': ");
                io::stdout().flush()?;
            }
        }
    }
}

fn confirm(msg: &str) -> Result<()> {
    ask(msg).and_then(|y| if y { Ok(()) } else { Err("Cancelled".into()) })
}

fn configure_server<R : Read, W : Write>(output: R, mut input: W,
                                         remote_path: &str)
                                         -> Result<Option<String>> {
    let mut output = io::BufReader::new(output);

    // Read the response from the `printf` at the beginning of the ssh shell
    // command.
    let is_ensync_shell = read_shell_response(&mut output)?;
    if SHELL_IDENTITY == &is_ensync_shell {
        if !remote_path.is_empty() {
            return Err("The server is running ensync as a shell; you cannot \
                        specify the server-side path (just write `host:` or \
                        `user@host:`".into());
        }
        return Ok(None);
    } else if "0" != &is_ensync_shell {
        return Err("Unexpected response from server shell".into());
    }

    sanity_check_remote(ShellPathAccess {
        input: &mut input, output: &mut output
    }, remote_path)?;

    // Try to find an existing ensync installation
    let found = remote_shell_command(
        &mut input, &mut output,
        "(which ensync >/dev/null && printf 'ensync\\0') || \
         (test -f .cargo/bin/ensync && printf '.cargo/bin/ensync\\0') || \
         (test -f ./ensync && printf './ensync\\0') || \
         printf 'nothing\\0'")?;

    if "nothing" != &found {
        return Ok(Some(found));
    }

    // Eventually, we should try some options here to set this up for the
    // server.
    Err("ensync does not appear to be installed on the server".into())
}

fn remote_shell_command<R : BufRead, W : Write>
    (mut input: W, output: R, command: &str) -> Result<String>
{
    writeln!(input, "{}", command).and_then(|_| input.flush())
        .chain_err(|| "Error writing shell command to server")?;
    read_shell_response(output)
}

fn read_shell_response<R : BufRead>(mut output: R) -> Result<String> {
    let mut buf = Vec::new();
    output.read_until(0, &mut buf).chain_err(
        || "Failed to read shell response from server")?;
    if Some(0) != buf.pop() {
        return Err("Unexpected EOF from server".into());
    }

    String::from_utf8(buf).chain_err(
        || "Failed to decode shell response from server")
}

fn shell_escape(s: &str) -> String {
    if s.is_empty() { return "''".to_owned(); }

    // Do nothing if safe (by a very conservative definition)
    let mut safe = true;
    for ch in s.chars() {
        match ch {
            'a'...'z' | 'A'...'Z' | '0'...'9' |
            '.' | '/' | ':' | '@' | '-' | '_' => (),
            _ => { safe = false; break; },
        }
    }

    if safe { return s.to_owned(); }

    let mut escaped = "'".to_owned();
    for ch in s.chars() {
        if '\'' == ch {
            escaped.push_str("'\"'\"'");
        } else {
            escaped.push(ch);
        }
    }
    escaped.push('\'');
    escaped
}

trait PathAccess {
    fn is_dir(&mut self, path: &Path) -> Result<bool>;
    fn exists(&mut self, path: &Path) -> Result<bool>;
}

struct LocalPathAccess;
impl PathAccess  for LocalPathAccess {
    fn is_dir(&mut self, path: &Path) -> Result<bool> {
        Ok(path.is_dir2())
    }

    fn exists(&mut self, path: &Path) -> Result<bool> {
        Ok(path.exists2())
    }
}

struct ShellPathAccess<R : BufRead, W : Write> {
    input: W,
    output: R,
}

impl<R : BufRead, W : Write> PathAccess for ShellPathAccess<R, W> {
    fn is_dir(&mut self, path: &Path) -> Result<bool> {
        if path.iter().next().is_none() { return Ok(true); }

        remote_shell_command(
            &mut self.input, &mut self.output,
            &format!("test -d {} && printf 'y\\0' || printf 'n\\0'",
                     shell_escape(path.to_str().unwrap())))
            .map(|ref s| "y" == s)
    }

    fn exists(&mut self, path: &Path) -> Result<bool> {
        if path.iter().next().is_none() { return Ok(true); }

        remote_shell_command(
            &mut self.input, &mut self.output,
            &format!("test -e {} && printf 'y\\0' || printf 'n\\0'",
                     shell_escape(path.to_str().unwrap())))
            .map(|ref s| "y" == s)
    }
}

fn sanity_check_remote<A : PathAccess, P : AsRef<Path>>
    (mut access: A, remote: P) -> Result<()>
{
    let remote = remote.as_ref();
    if remote.iter().next().is_none() {
        return Err("Remote path is empty".into());
    }

    if access.exists(remote)? {
        if !access.is_dir(remote)? {
            return Err(format!("Sync remote path '{}' is not a directory",
                               remote.display()).into());
        } else if !access.is_dir(&remote.join("dirs"))? {
            confirm(&format!("\
                Sync remote location '{}' already exists and does not look like an ensync
server directory.
Really use this location as the remote? ", remote.display()))?;
        }
    } else if !remote.parent().map_or(Ok(true), |p| access.exists(p))? {
        confirm(&format!("\
            Parent of sync remote location, '{}', does not exist;
Should it be created too? ", remote.parent().unwrap().display()))?;
    }

    Ok(())
}

#[cfg(test)]
mod test {
    use super::shell_escape;

    #[test]
    fn shell_escape_works() {
        assert_eq!("foo", &shell_escape("foo"));
        assert_eq!("azAZ09", &shell_escape("azAZ09"));
        assert_eq!("foo@bar.com", &shell_escape("foo@bar.com"));
        assert_eq!("/tmp/y.txt", &shell_escape("/tmp/y.txt"));
        assert_eq!("'foo bar'", &shell_escape("foo bar"));
        assert_eq!("'o'\"'\"'ryan'", &shell_escape("o'ryan"));
    }
}
