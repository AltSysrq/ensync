//-
// Copyright (c) 2016, 2017, Jason Lingle
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

use std::io::{self, BufRead, Read, Write};
use std::sync::{Condvar, Mutex};
use std::sync::atomic::{AtomicBool, Ordering};

use fourleaf;

use defs::HashId;
use errors::*;
use server::storage::*;

pub const PROTOCOL_VERSION_MAJOR: u32 = 0;
pub const PROTOCOL_VERSION_MINOR: u32 = 0;

/// Identifies a client or server implementation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImplementationInfo {
    /// The name of the implementation, eg, "ensync"
    pub name: String,
    /// The major, minor, patch version of the implementation.
    pub version: (u32, u32, u32),
    /// The protocol version the implementation supports.
    ///
    /// Clients SHOULD send the latest protocol they support. The server
    /// MUST respond to `ClientInfo` with `Error` if it does not support
    /// the stated version. A server which supports a later version but
    /// also supports that version MUST respond with a protocol version of
    /// the same major version, but may include a different minor version.
    /// A server which only supports protocol versions earlier than the
    /// client's SHOULD respond with the latest protocol version it
    /// supports; in this case, whether to continue is up to the client.
    ///
    /// A major version difference in the protocol indicates a change that both
    /// sides must be aware of. A minor version difference implies that the
    /// fourleaf format is backwards-compatible, and that no new server
    /// behaviours will be triggered without explicit opt-in by the client.
    ///
    /// Note that the protocol version numbers both start at 0.
    pub protocol: (u32, u32),
}

fourleaf_retrofit!(struct ImplementationInfo : {} {} {
    |_context, this|
    [1] name: String = &this.name,
    [2] version: (u32, u32, u32) = this.version,
    [3] protocol: (u32, u32) = this.protocol,
    { Ok(ImplementationInfo { name: name, version: version,
                              protocol: protocol }) }
});

impl ImplementationInfo {
    /// Returns an `ImplementationInfo` for this implementation.
    pub fn this_implementation() -> Self {
        ImplementationInfo {
            name: env!("CARGO_PKG_NAME").to_owned(),
            version: (
                env!("CARGO_PKG_VERSION_MAJOR").parse().unwrap(),
                env!("CARGO_PKG_VERSION_MINOR").parse().unwrap(),
                env!("CARGO_PKG_VERSION_PATCH").parse().unwrap(),
            ),
            protocol: (PROTOCOL_VERSION_MAJOR, PROTOCOL_VERSION_MINOR),
        }
    }
}

/// A RPC request sent to a remote server.
///
/// Different request types merit different responses; some involve no
/// response at all.
///
/// See also `SERVER-WIRE.md` in the repository root.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Request {
    /// Does not correspond to a `Storage` method.
    ///
    /// Informs the server about client properties.
    ///
    /// Clients SHOULD send this as their first request.
    ///
    /// Response: One `ServerInfo` | `Error`.
    ClientInfo {
        /// The implementation information about the client.
        implementation: ImplementationInfo,
    },
    /// `Storage::getdir`.
    ///
    /// Response: One `DirData` | `NotFound` | `Error`
    GetDir(HashId),
    /// `Storage::getobj`
    ///
    /// Response: One `ObjData` | `NotFound` | `Error`
    GetObj(HashId),
    /// `Storage::check_dir_dirty`
    ///
    /// No response.
    CheckDirDirty(HashId, HashId, u32),
    /// `Storgae::for_dirty_dir`
    ///
    /// - Any number of `DirtyDir`
    /// - One `Done` | `Error`
    ForDirtyDir,
    /// `Storage::start_tx`
    ///
    /// No response.
    StartTx(Tx),
    /// `Storage::commit`
    ///
    /// Response: One `Done` | `Fail` | `Error`
    Commit(Tx),
    /// `Storage::abort`
    ///
    /// Response: One `Done` | `Error`
    Abort(Tx),
    /// `Storage::mkdir`
    ///
    /// No response.
    Mkdir { tx: Tx, id: HashId, ver: HashId, sver: HashId, data: Vec<u8>, },
    /// `Storage::updir`
    ///
    /// No response.
    Updir { tx: Tx, id: HashId, sver: HashId, old_len: u32,
            append: Vec<u8>, },
    /// `Storage::rmdir`
    ///
    /// No response.
    Rmdir { tx: Tx, id: HashId, sver: HashId, old_len: u32, },
    /// `Storage::linkobj`
    ///
    /// Response: One `Done` | `NotFound` | `Error`
    Linkobj { tx: Tx, id: HashId, linkid: HashId },
    /// `Storage::putobj`
    ///
    /// No response.
    Putobj { tx: Tx, id: HashId, linkid: HashId, data: Vec<u8>, },
    /// `Storage::unlinkobj`
    ///
    /// No response.
    Unlinkobj { tx: Tx, id: HashId, linkid: HashId },
    /// `Storage::clean_up`
    ///
    /// Response: One `Done`.
    CleanUp,
}

fourleaf_retrofit!(enum Request : {} {} {
    |_context|
    [1] Request::ClientInfo { ref implementation } => {
        [1] implementation: ImplementationInfo = implementation,
        { Ok(Request::ClientInfo { implementation: implementation }) }
    },
    [2] Request::GetDir(ref id) => {
        [1] id: HashId = id,
        { Ok(Request::GetDir(id)) }
    },
    [3] Request::GetObj(ref id) => {
        [1] id: HashId = id,
        { Ok(Request::GetObj(id)) }
    },
    [4] Request::CheckDirDirty(ref id, ref ver, len) => {
        [1] id: HashId = id,
        [2] ver: HashId = ver,
        [3] len: u32 = len,
        { Ok(Request::CheckDirDirty(id, ver, len)) }
    },
    [5] Request::ForDirtyDir => {
        { Ok(Request::ForDirtyDir) }
    },
    [6] Request::StartTx(tx) => {
        [1] tx: Tx = tx,
        { Ok(Request::StartTx(tx)) }
    },
    [7] Request::Commit(tx) => {
        [1] tx: Tx = tx,
        { Ok(Request::Commit(tx)) }
    },
    [8] Request::Abort(tx) => {
        [1] tx: Tx = tx,
        { Ok(Request::Abort(tx)) }
    },
    [9] Request::Mkdir { tx, ref id, ref ver, ref sver, ref data } => {
        [1] tx: Tx = tx,
        [2] id: HashId = id,
        [3] ver: HashId = ver,
        [4] sver: HashId = sver,
        [5] data: Vec<u8> = data,
        { Ok(Request::Mkdir { tx: tx, id: id, ver: ver, sver: sver, data: data }) }
    },
    [10] Request::Updir { tx, ref id, ref sver, old_len, ref append } => {
        [1] tx: Tx = tx,
        [2] id: HashId = id,
        [3] sver: HashId = sver,
        [4] old_len: u32 = old_len,
        [5] append: Vec<u8> = append,
        { Ok(Request::Updir { tx: tx, id: id, sver: sver,
                              old_len: old_len, append: append }) }
    },
    [11] Request::Rmdir { tx, ref id, ref sver, old_len } => {
        [1] tx: Tx = tx,
        [2] id: HashId = id,
        [3] sver: HashId = sver,
        [4] old_len: u32 = old_len,
        { Ok(Request::Rmdir { tx: tx, id: id, sver: sver, old_len: old_len }) }
    },
    [12] Request::Linkobj { tx, ref id, ref linkid } => {
        [1] tx: Tx = tx,
        [2] id: HashId = id,
        [3] linkid: HashId = linkid,
        { Ok(Request::Linkobj { tx: tx, id: id, linkid: linkid }) }
    },
    [13] Request::Putobj { tx, ref id, ref linkid, ref data } => {
        [1] tx: Tx = tx,
        [2] id: HashId = id,
        [3] linkid: HashId = linkid,
        [4] data: Vec<u8> = data,
        { Ok(Request::Putobj { tx: tx, id: id, linkid: linkid, data: data }) }
    },
    [14] Request::Unlinkobj { tx, ref id, ref linkid } => {
        [1] tx: Tx = tx,
        [2] id: HashId = id,
        [3] linkid: HashId = linkid,
        { Ok(Request::Unlinkobj { tx: tx, id: id, linkid: linkid }) }
    },
    [15] Request::CleanUp => {
        { Ok(Request::CleanUp) }
    },
});

/// Responses correspoinding to various `Request`s above.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Response {
    /// The request was carried out successfully, and there is no
    /// information to return.
    Done,
    /// The request was not executed due to conditions on the request.
    Fail,
    /// The server tried to execute the request, but failed to do so.
    Error(String),
    /// The server encountered an error and cannot continue.
    ///
    /// This is mainly for things like protocol faults where recovering
    /// from a malformed stream is not generally possible. (Even if the
    /// framing mechanism is still working correctly, if the request that
    /// was not understood did not take a response, returning an error
    /// would desynchronise the request/response pairing.)
    FatalError(String),
    /// The item referenced by the request does not exist.
    NotFound,
    /// A directory id which is to be considered dirty.
    DirtyDir(HashId),
    /// The version and full data of a directory.
    DirData(HashId, Vec<u8>),
    /// The full data of an object.
    ObjData(Vec<u8>),
    /// Identifies information about the server.
    ServerInfo {
        /// Information about the server implementation.
        implementation: ImplementationInfo,
        /// If set, the client should display the given plaintext message
        /// to the user.
        motd: Option<String>,
    },
}

fourleaf_retrofit!(enum Response : {} {} {
    |_context|
    [1] Response::Done => { { Ok(Response::Done) } },
    [2] Response::Fail => { { Ok(Response::Fail) } },
    [3] Response::Error(ref msg) => {
        [1] msg: String = msg,
        { Ok(Response::Error(msg)) }
    },
    [4] Response::FatalError(ref msg) => {
        [1] msg: String = msg,
        { Ok(Response::FatalError(msg)) }
    },
    [5] Response::NotFound => { { Ok(Response::NotFound) } },
    [6] Response::DirtyDir(ref id) => {
        [1] id: HashId = id,
        { Ok(Response::DirtyDir(id)) }
    },
    [7] Response::DirData(ref ver, ref data) => {
        [1] ver: HashId = ver,
        [2] data: Vec<u8> = data,
        { Ok(Response::DirData(ver, data)) }
    },
    [8] Response::ObjData(ref data) => {
        [1] data: Vec<u8> = data,
        { Ok(Response::ObjData(data)) }
    },
    [9] Response::ServerInfo { ref implementation, ref motd } => {
        [1] implementation: ImplementationInfo = implementation,
        [2] motd: Option<String> = motd,
        { Ok(Response::ServerInfo { implementation: implementation,
                                    motd: motd }) }
    },
});

fn read_frame<R : BufRead,
              T : fourleaf::Deserialize<R, fourleaf::de::style::Copying> + ::std::fmt::Debug>
    (mut sin: R) -> Result<Option<T>>
{
    if sin.fill_buf().chain_err(|| ErrorKind::ServerProtocolError)?.is_empty() {
        return Ok(None);
    }

    let mut config = fourleaf::DeConfig::default();
    config.max_blob = 128 * 1024 * 1024;
    let value = fourleaf::from_reader(sin, &config)
        .chain_err(|| ErrorKind::ServerProtocolError)?;
    Ok(Some(value))
}

fn send_frame<W : Write, T : fourleaf::Serialize>(mut out: W, obj: T)
                                                  -> Result<()> {
    fourleaf::to_writer(&mut out, obj)
        .chain_err(|| ErrorKind::ServerProtocolError)?;
    out.flush().chain_err(|| ErrorKind::ServerProtocolError)?;
    Ok(())
}

enum RequestResponse {
    None,
    AsyncError(Error),
    SyncResponse(Response),
}

/// Runs the RPC server on the given input and output.
///
/// Requests will be read from `unbuf_sin`, executed, and responses written to
/// `unbuf_sout` until either input returns EOF or a fatal error occurs.
pub fn run_server_rpc<S : Storage, R : Read, W : Write>(
    storage: S, unbuf_sin: R, unbuf_sout: W) -> Result<()>
{
    let mut sin = io::BufReader::new(unbuf_sin);
    let mut sout = io::BufWriter::new(unbuf_sout);
    let mut fatal_error = None;
    loop {
        let request: Request = match read_frame(&mut sin) {
            Ok(Some(r)) => r,
            Ok(None) => break,
            Err(e) => {
                let _ = send_frame(&mut sout,
                                   Response::FatalError(format!("{}", e)));
                break;
            },
        };

        match process_frame(&storage, request, &mut sout) {
            RequestResponse::None => { },
            // We can't immediately send a `FatalError` when an async command
            // fails, because the client might have more async commands to
            // send, which would lead to deadlock if it blocked on the socket
            // while we were waiting for it to notice the `FatalError` message,
            // so instead save the error away.
            RequestResponse::AsyncError(e) => if fatal_error.is_none() {
                fatal_error = Some(e)
            },
            RequestResponse::SyncResponse(response) => {
                // Now that we know the client is paying attention and is
                // expecting some kind of response, we can return any deferred
                // error.
                //
                // It is admittedly kind of weird that we determine this
                // dynamically by actually processing the request and
                // inspecting whether it produced a result and then discarding
                // that result, but this keeps the code simpler since these
                // conditions do not happen with high frequency.
                let response = fatal_error.take()
                    .map(|e| Response::FatalError(e.to_string()))
                    .unwrap_or(response);
                let fatal = if let Response::FatalError(_) = response {
                    true
                } else {
                    false
                };
                try!(write_response(&mut sout, response));

                if fatal {
                    return Ok(());
                }
            },
        }
    }

    fn process_frame<S : Storage, W : Write>(
        storage: &S, request: Request, sout: &mut W)
        -> RequestResponse
    {
        macro_rules! err {
            ($e:expr) => { RequestResponse::SyncResponse(
                Response::Error(format!("{}", $e))) }
        };

        macro_rules! none_or_fatal {
            ($e:expr) => { match $e {
                Ok(_) => RequestResponse::None,
                Err(e) => RequestResponse::AsyncError(e),
            } }
        }

        match request {
            Request::ClientInfo { .. } =>
                RequestResponse::SyncResponse(Response::ServerInfo {
                    implementation: ImplementationInfo::this_implementation(),
                    motd: None,
                }),

            Request::GetDir(id) => match storage.getdir(&id) {
                Ok(Some((v, data))) =>
                    RequestResponse::SyncResponse(Response::DirData(v, data)),
                Ok(None) => RequestResponse::SyncResponse(Response::NotFound),
                Err(err) => err!(err),
            },

            Request::GetObj(id) => match storage.getobj(&id) {
                Ok(Some(data)) =>
                    RequestResponse::SyncResponse(Response::ObjData(data)),
                Ok(None) => RequestResponse::SyncResponse(Response::NotFound),
                Err(err) => err!(err),
            },

            Request::CheckDirDirty(ref id, ref ver, len) =>
                none_or_fatal!(storage.check_dir_dirty(id, ver, len)),

            Request::ForDirtyDir => {
                match storage.for_dirty_dir(&mut |id| {
                    write_response(sout, Response::DirtyDir(*id))
                }) {
                    Ok(()) => RequestResponse::SyncResponse(Response::Done),
                    Err(err) => err!(err),
                }
            },

            Request::StartTx(tx) => none_or_fatal!(storage.start_tx(tx)),

            Request::Commit(tx) => match storage.commit(tx) {
                Ok(true) => RequestResponse::SyncResponse(Response::Done),
                Ok(false) => RequestResponse::SyncResponse(Response::Fail),
                Err(err) => err!(err),
            },

            Request::Abort(tx) => match storage.abort(tx) {
                Ok(_) => RequestResponse::SyncResponse(Response::Done),
                Err(err) => err!(err),
            },

            Request::Mkdir { tx, id, ver, sver, data } =>
                none_or_fatal!(storage.mkdir(tx, &id, &ver, &sver, &data[..])),

            Request::Updir { tx, id, sver, old_len, append } =>
                none_or_fatal!(storage.updir(tx, &id, &sver, old_len,
                                             &append[..])),

            Request::Rmdir { tx, id, sver, old_len } =>
                none_or_fatal!(storage.rmdir(tx, &id, &sver, old_len)),

            Request::Linkobj { tx, id, linkid } => {
                match storage.linkobj(tx, &id, &linkid) {
                    Ok(true) => RequestResponse::SyncResponse(Response::Done),
                    Ok(false) =>
                        RequestResponse::SyncResponse(Response::NotFound),
                    Err(err) => err!(err),
                }
            },

            Request::Putobj { tx, id, linkid, data } =>
                none_or_fatal!(storage.putobj(tx, &id, &linkid, &data[..])),

            Request::Unlinkobj { tx, id, linkid } =>
                none_or_fatal!(storage.unlinkobj(tx, &id, &linkid)),

            Request::CleanUp => {
                storage.clean_up();
                RequestResponse::SyncResponse(Response::Done)
            },
        }
    }

    fn write_response<W : Write>(out: &mut W, response: Response)
                                 -> Result<()> {
        send_frame(out, response)
    }

    Ok(())
}

/// `Storage` implementation which uses the RPC mechanism to communicate with
/// another storage implementation over a pipe.
pub struct RemoteStorage {
    // The input and output streams. When a request is sent which needs a
    // response, it increments the value associated with `sout` and remembers
    // the old value. Then, it locks `sin`. As long as the integer associated
    // with `sin` is not equal to the integer above, it waits on `cond`. Once
    // it does match, the response is read and then the integer on `sin` is
    // incremented and all waiters on `cond` are notified.
    sout: Mutex<(Box<Write + Send>, u64)>,
    sin: Mutex<(Box<BufRead + Send>, u64)>,
    cond: Condvar,
    fatal: AtomicBool,
}

macro_rules! handle_response {
    ($this:expr, $term:expr => { $($pat:pat => $res:expr,)* }) => {
        match $term {
            Response::Error(e) =>
                return Err(ErrorKind::ServerError(e).into()),
            Response::FatalError(e) => {
                $this.fatal.store(true, Ordering::Relaxed);
                return Err(ErrorKind::ServerFatalError(e).into());
            },
            $($pat => $res,)*
            r => {
                $this.fatal.store(true, Ordering::Relaxed);
                return Err(ErrorKind::UnexpectedServerResponse(r).into());
            },
        }
    }
}

macro_rules! tryf {
    ($this:expr, $e:expr) => { match $e {
        Ok(v) => v,
        Err(e) => {
            let error: Error = e.into();
            if error.is_fatal() {
                $this.fatal.store(true, Ordering::Relaxed);
            }
            return Err(error);
        },
    } }
}

impl RemoteStorage {
    pub fn new<R : Read + Send + 'static, W : Write + Send + 'static>
        (sin: R, sout: W) -> Self
    {
        RemoteStorage {
            sout: Mutex::new((Box::new(io::BufWriter::new(sout)), 0)),
            sin: Mutex::new((Box::new(io::BufReader::new(sin)), 0)),
            cond: Condvar::new(),
            fatal: AtomicBool::new(false),
        }
    }

    fn send_async_request(&self, req: Request) -> Result<()> {
        let mut sout = self.sout.lock().unwrap();
        tryf!(self, send_frame(&mut sout.0, req).chain_err(
            || "Error writing request to server"));
        Ok(())
    }

    fn send_sync_request<T, F : FnOnce (&mut BufRead) -> Result<T>>(
        &self, req: Request, read: F) -> Result<T>
    {
        let ticket = {
            let mut sout = self.sout.lock().unwrap();
            tryf!(self, send_frame(&mut sout.0, req).chain_err(
                || "Error writing request to server"));
            let t = sout.1;
            sout.1 += 1;
            t
        };

        {
            let mut sin = self.sin.lock().unwrap();
            while ticket != sin.1 {
                sin = self.cond.wait(sin).unwrap();
            }
            sin.1 += 1;

            let ret = read(&mut*sin.0).chain_err(
                || "Error reading server response");
            self.cond.notify_all();
            ret
        }
    }

    fn send_single_sync_request(&self, req: Request) -> Result<Response> {
        self.send_sync_request(req, |r| {
            read_frame(r).and_then(
                |r| r.ok_or(ErrorKind::ServerConnectionClosed.into()))
        })
    }

    pub fn exchange_client_info
        (&self) -> Result<(ImplementationInfo, Option<String>)>
    {
        handle_response!(self, tryf!(self, self.send_single_sync_request(
            Request::ClientInfo {
                implementation: ImplementationInfo::this_implementation()
            }
        )) => {
            Response::ServerInfo { implementation, motd } =>
                Ok((implementation, motd)),
        })
    }
}

impl Storage for RemoteStorage {
    fn is_fatal(&self) -> bool {
        self.fatal.load(Ordering::Relaxed)
    }

    fn getdir(&self, id: &HashId) -> Result<Option<(HashId, Vec<u8>)>> {
        handle_response!(self, tryf!(self, self.send_single_sync_request(
            Request::GetDir(*id)
        )) => {
            Response::DirData(v, data) =>
                Ok(Some((v, data.into()))),
            Response::NotFound => Ok(None),
        })
    }

    fn getobj(&self, id: &HashId) -> Result<Option<Vec<u8>>> {
        handle_response!(self, tryf!(self, self.send_single_sync_request(
            Request::GetObj(*id)
        )) => {
            Response::ObjData(data) => Ok(Some(data.into())),
            Response::NotFound => Ok(None),
        })
    }

    fn check_dir_dirty(&self, id: &HashId, ver: &HashId, len: u32)
                       -> Result<()> {
        self.send_async_request(Request::CheckDirDirty(*id, *ver, len))
    }

    fn for_dirty_dir(&self, f: &mut FnMut (&HashId) -> Result<()>)
                     -> Result<()> {
        self.send_sync_request(Request::ForDirtyDir, |mut sin| {
            let mut error = None;
            loop {
                handle_response!(self, tryf!(self, read_frame(&mut sin).and_then(
                    |r| r.ok_or(ErrorKind::ServerConnectionClosed.into())
                )) => {
                    Response::Done => break,
                    Response::DirtyDir(id) => match f(&id) {
                        Ok(()) => { },
                        // We can't return early on failure here as there are
                        // still more responses we need to consume.
                        Err(err) => error = Some(err),
                    },
                })
            }

            if let Some(error) = error {
                Err(error)
            } else {
                Ok(())
            }
        })
    }

    fn start_tx(&self, tx: Tx) -> Result<()> {
        self.send_async_request(Request::StartTx(tx))
    }

    fn abort(&self, tx: Tx) -> Result<()> {
        handle_response!(self, tryf!(self, self.send_single_sync_request(
            Request::Abort(tx)
        )) => {
            Response::Done => Ok(()),
        })
    }

    fn commit(&self, tx: Tx) -> Result<bool> {
        handle_response!(self, tryf!(self, self.send_single_sync_request(
            Request::Commit(tx)
        )) => {
            Response::Done => Ok(true),
            Response::Fail => Ok(false),
        })
    }

    fn mkdir(&self, tx: Tx, id: &HashId, v: &HashId, sv: &HashId, data: &[u8])
             -> Result<()> {
        self.send_async_request(Request::Mkdir {
            tx: tx, id: *id, ver: *v, sver: *sv, data: data.to_owned()
        })
    }

    fn updir(&self, tx: Tx, id: &HashId, sv: &HashId, old_len: u32,
             data: &[u8]) -> Result<()> {
        self.send_async_request(Request::Updir {
            tx: tx, id: *id, sver: *sv, old_len: old_len,
            append: data.to_owned()
        })
    }

    fn rmdir(&self, tx: Tx, id: &HashId, sv: &HashId, old_len: u32)
             -> Result<()> {
        self.send_async_request(Request::Rmdir {
            tx: tx, id: *id, sver: *sv, old_len: old_len
        })
    }

    fn linkobj(&self, tx: Tx, id: &HashId, linkid: &HashId)
               -> Result<bool> {
        handle_response!(self, tryf!(self, self.send_single_sync_request(
            Request::Linkobj { tx: tx, id: *id, linkid: *linkid }
        )) => {
            Response::Done => Ok(true),
            Response::NotFound => Ok(false),
        })
    }

    fn putobj(&self, tx: Tx, id: &HashId, linkid: &HashId, data: &[u8])
              -> Result<()> {
        self.send_async_request(Request::Putobj {
            tx: tx, id: *id, linkid: *linkid,
            data: data.to_owned()
        })
    }

    fn unlinkobj(&self, tx: Tx, id: &HashId, linkid: &HashId)
                 -> Result<()> {
        self.send_async_request(Request::Unlinkobj {
            tx: tx, id: *id, linkid: *linkid,
        })
    }

    fn clean_up(&self) {
        let _ = self.send_single_sync_request(Request::CleanUp);
    }
}

#[cfg(test)]
mod test {
    include!("storage_tests.rs");

    use std::fs;
    use std::thread;

    use os_pipe;

    use server::local_storage::LocalStorage;
    use super::*;

    #[test]
    fn exchanging_impl_info_works() {
        init!(_dir, storage);

        let (imp, motd) = storage.exchange_client_info().unwrap();
        assert_eq!(ImplementationInfo::this_implementation(), imp);
        assert_eq!(None, motd);
    }

    fn create_storage(dir: &Path) -> RemoteStorage {
        fn pipe() -> (fs::File, fs::File) {
            let p = os_pipe::pipe().unwrap();
            (p.read, p.write)
        }

        let (read_from_client, write_to_server) = pipe();
        let (read_from_server, write_to_client) = pipe();
        let local_storage = LocalStorage::open(dir).unwrap();

        thread::spawn(move || run_server_rpc(
            local_storage, read_from_client, write_to_client));

        RemoteStorage::new(read_from_server, write_to_server)
    }
}
