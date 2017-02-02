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

use std::io::{Write, stderr};
use std::process;
use std::sync::Arc;

use cli::config::*;
use errors::*;
use server::*;

/// Opens the `Storage` for the given server configuration.
///
/// If this spawns a process, there is no way to reap the process when it
/// terminates.
pub fn open_server_storage(config: &ServerConfig) -> Result<Arc<Storage>> {
    match *config {
        ServerConfig::Path(ref path) =>
            Ok(Arc::new(LocalStorage::open(path).chain_err(
                || "Failed to set up server in local filesystem")?)),

        ServerConfig::Shell(ref command, ref workdir) => {
            // Running the process this way doesn't fully play nicely with our
            // special handling of ^C (though it does work well enough). Like
            // this, if the user hits ^C, the server command is immediately
            // terminated, which causes `ssh` to hang up immediately
            // (regardless of what the remote process does), thus interrupting
            // any in-flight commands.
            //
            // We _could_ prefix `command` with `trap "" INT`, which fixes
            // that, but means that if this process is killed with two ^C
            // strokes, the server process continues running with no obvious
            // way to stop it.
            //
            // The former option seems the lesser of these two evils.
            let mut process = process::Command::new("/bin/sh");
            process
                .arg("-c")
                .arg(command)
                .stderr(process::Stdio::inherit())
                .stdin(process::Stdio::piped())
                .stdout(process::Stdio::piped());
            if let Some(ref workdir) = *workdir {
                process.current_dir(workdir);
            }

            let mut child = process.spawn().chain_err(
                || format!("Failed to start server command `{}`", command))?;
            let storage = RemoteStorage::new(
                child.stdout.take().expect("Missing stdout pipe on child"),
                child.stdin.take().expect("Missing stdin pipe on child"));

            match storage.exchange_client_info() {
                Ok((info, motd)) => {
                    let _ = writeln!(stderr(), "Connected to {} {}.{}.{} \
                                                (proto {}.{}) via `{}`",
                                     info.name, info.version.0, info.version.1,
                                     info.version.2, info.protocol.0,
                                     info.protocol.1, command);
                    if let Some(motd) = motd {
                        let _ = writeln!(stderr(), "{}", motd);
                    }
                },
                Err(e) => {
                    // Close the child's input and output and wait for it to
                    // finish. Most likely the remote process failed to start
                    // for some reason, so it is better to output that detail
                    // rather than some obscure protocol error, if possible.
                    drop(storage);
                    if let Ok(status) = child.wait() {
                        if !status.success() {
                            return Err(format!("Command `{}` failed with {}",
                                               command, status).into());
                        }
                    }

                    // Either the command succeeded unexpectedly, or we failed
                    // to wait for the child. All we can do is return the
                    // protocol error.
                    return Err(e).chain_err(
                        || format!("Protocol error communicating with `{}`",
                                   command));
                },
            }

            Ok(Arc::new(storage))
        },
    }
}

/// Opens a `ServerReplica` on top of the given storage, using parameters from
/// the given config.
///
/// If the caller already has a key chain, it may pass it in so that the user
/// is not prompted again. If `None`, the passphrase will be read within this
/// call.
pub fn open_server_replica(config: &Config, storage: Arc<Storage>,
                           key_chain: Option<Arc<KeyChain>>)
                           -> Result<ServerReplica<Storage>> {
    let key_chain = if let Some(key_chain) = key_chain {
        key_chain
    } else {
        let passphrase = config.passphrase.read_passphrase(
            "passphrase", false)?;
        Arc::new(keymgmt::derive_key_chain(&*storage, &passphrase[..])?)
    };

    Ok(ServerReplica::new(
        config.private_root.join("server-state.sqlite").to_str()
            .ok_or_else(|| format!("Path '{}' is not valid UTF-8",
                                   config.private_root.display()))?,
        key_chain, storage, &config.server_root, config.block_size as usize,
        config.compression)
       .chain_err(|| "Failed to set up server replica")?)
}
