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

use std::fmt;
use std::io::{self, Read, Write};
use std::sync::{Condvar, Mutex};

use serde;
use serde::bytes::ByteBuf;
use serde_cbor;

use defs::HashId;
use errors::*;
use serde_types::rpc::*;
use server::storage::*;

#[allow(unused_parens)]
fn read_frame<R : Read, T : serde::Deserialize + fmt::Debug>
    (mut sin: R) -> Result<Option<T>>
{
    let mut frame_len_arr = [0u8;4];
    match sin.read_exact(&mut frame_len_arr) {
        Ok(_) => (),
        Err(ref err) if io::ErrorKind::UnexpectedEof == err.kind() =>
            return Ok(None),
        Err(err) => return Err(err.into()),
    }
    let frame_len = (((frame_len_arr[0] as u32)      ) |
                     ((frame_len_arr[1] as u32) <<  8) |
                     ((frame_len_arr[2] as u32) << 16) |
                     ((frame_len_arr[3] as u32) << 24));
    let mut frame_data = sin.by_ref().take(frame_len as u64);
    let res: serde_cbor::Result<T> = serde_cbor::from_reader(&mut frame_data);

    // Discard any extraneous frame data
    {
        let mut bytes = frame_data.bytes();
        while let Some(Ok(_)) = bytes.next() { }
    }

    Ok(Some(try!(res)))
}

fn send_frame<W : Write, T : serde::Serialize>(mut out: W, obj: T)
                                               -> Result<()> {
    let data = try!(serde_cbor::to_vec(&obj));
    let length: [u8;4] = [
        ((data.len()      ) & 0xFF) as u8,
        ((data.len() >>  8) & 0xFF) as u8,
        ((data.len() >> 16) & 0xFF) as u8,
        ((data.len() >> 24) & 0xFF) as u8,
    ];
    try!(out.write_all(&length));
    try!(out.write_all(&data));
    try!(out.flush());
    Ok(())
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

        let response = process_frame(&storage, request, &mut sout);
        if let Some(response) = response {
            try!(write_response(&mut sout, response));
        }
    }

    fn process_frame<S : Storage, W : Write>(
        storage: &S, request: Request, sout: &mut W)
        -> Option<Response>
    {
        macro_rules! err {
            ($e:expr) => { Some(Response::Error(format!("{}", $e))) }
        };

        match request {
            Request::ClientInfo { .. } =>
                Some(Response::ServerInfo {
                    implementation: ImplementationInfo::this_implementation(),
                    motd: None,
                }),

            Request::GetDir(H(id)) => match storage.getdir(&id) {
                Ok(Some((v, data))) =>
                    Some(Response::DirData(H(v), ByteBuf::from(data))),
                Ok(None) => Some(Response::NotFound),
                Err(err) => err!(err),
            },

            Request::GetObj(H(id)) => match storage.getobj(&id) {
                Ok(Some(data)) =>
                    Some(Response::ObjData(ByteBuf::from(data))),
                Ok(None) => Some(Response::NotFound),
                Err(err) => err!(err),
            },

            Request::ForDir => {
                match storage.for_dir(|id, v, len| {
                    write_response(sout, Response::DirEntry(H(*id), H(*v), len))
                }) {
                    Ok(()) => Some(Response::Done),
                    Err(err) => err!(err),
                }
            },

            Request::StartTx(tx) => {
                let _ = storage.start_tx(tx);
                None
            },

            Request::Commit(tx) => match storage.commit(tx) {
                Ok(true) => Some(Response::Done),
                Ok(false) => Some(Response::Fail),
                Err(err) => err!(err),
            },

            Request::Abort(tx) => match storage.abort(tx) {
                Ok(_) => Some(Response::Done),
                Err(err) => err!(err),
            },

            Request::Mkdir { tx, id, ver, data } => {
                let _ = storage.mkdir(tx, &id.0, &ver.0, &data[..]);
                None
            },

            Request::Updir { tx, id, ver, old_len, append } => {
                let _ = storage.updir(tx, &id.0, &ver.0, old_len, &append[..]);
                None
            },

            Request::Rmdir { tx, id, ver, old_len } => {
                let _ = storage.rmdir(tx, &id.0, &ver.0, old_len);
                None
            },

            Request::Linkobj { tx, id, linkid } => {
                match storage.linkobj(tx, &id.0, &linkid.0) {
                    Ok(true) => Some(Response::Done),
                    Ok(false) => Some(Response::NotFound),
                    Err(err) => err!(err),
                }
            },

            Request::Putobj { tx, id, linkid, data } => {
                let _ = storage.putobj(tx, &id.0, &linkid.0, &data[..]);
                None
            },

            Request::Unlinkobj { tx, id, linkid } => {
                let _ = storage.unlinkobj(tx, &id.0, &linkid.0);
                None
            },

            Request::CleanUp => {
                storage.clean_up();
                Some(Response::Done)
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
    sin: Mutex<(Box<Read + Send>, u64)>,
    cond: Condvar,
}

macro_rules! handle_response {
    ($term:expr => { $($pat:pat => $res:expr,)* }) => {
        match $term {
            Response::Error(e) | Response::FatalError(e) =>
                return Err(ErrorKind::ServerError(e).into()),
            $($pat => $res,)*
            r =>
                return Err(ErrorKind::UnexpectedServerResponse(r).into()),
        }
    }
}

impl RemoteStorage {
    pub fn new<R : Read + Send + 'static, W : Write + Send + 'static>
        (sin: R, sout: W) -> Self
    {
        RemoteStorage {
            sout: Mutex::new((Box::new(io::BufWriter::new(sout)), 0)),
            sin: Mutex::new((Box::new(io::BufReader::new(sin)), 0)),
            cond: Condvar::new(),
        }
    }

    fn send_async_request(&self, req: Request) -> Result<()> {
        let mut sout = self.sout.lock().unwrap();
        try!(send_frame(&mut sout.0, req));
        Ok(())
    }

    fn send_sync_request<T, F : FnOnce (&mut Read) -> Result<T>>(
        &self, req: Request, read: F) -> Result<T>
    {
        let ticket = {
            let mut sout = self.sout.lock().unwrap();
            try!(send_frame(&mut sout.0, req));
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

            let ret = read(&mut*sin.0);
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
        handle_response!(try!(self.send_single_sync_request(
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
    fn getdir(&self, id: &HashId) -> Result<Option<(HashId, Vec<u8>)>> {
        handle_response!(try!(self.send_single_sync_request(
            Request::GetDir(H(*id))
        )) => {
            Response::DirData(H(v), data) =>
                Ok(Some((v, data.into()))),
            Response::NotFound => Ok(None),
        })
    }

    fn getobj(&self, id: &HashId) -> Result<Option<Vec<u8>>> {
        handle_response!(try!(self.send_single_sync_request(
            Request::GetObj(H(*id))
        )) => {
            Response::ObjData(data) => Ok(Some(data.into())),
            Response::NotFound => Ok(None),
        })
    }

    fn for_dir<F : FnMut (&HashId, &HashId, u32) -> Result<()>>
        (&self, mut f: F) -> Result<()>
    {
        self.send_sync_request(Request::ForDir, |mut sin| {
            let mut error = None;
            loop {
                handle_response!(try!(read_frame(&mut sin).and_then(
                    |r| r.ok_or(ErrorKind::ServerConnectionClosed.into())
                )) => {
                    Response::Done => break,
                    Response::DirEntry(H(id), H(v), l) => match f(&id, &v, l) {
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
        handle_response!(try!(self.send_single_sync_request(
            Request::Abort(tx)
        )) => {
            Response::Done => Ok(()),
        })
    }

    fn commit(&self, tx: Tx) -> Result<bool> {
        handle_response!(try!(self.send_single_sync_request(
            Request::Commit(tx)
        )) => {
            Response::Done => Ok(true),
            Response::Fail => Ok(false),
        })
    }

    fn mkdir(&self, tx: Tx, id: &HashId, v: &HashId, data: &[u8])
             -> Result<()> {
        self.send_async_request(Request::Mkdir {
            tx: tx, id: H(*id), ver: H(*v), data: ByteBuf::from(data)
        })
    }

    fn updir(&self, tx: Tx, id: &HashId, v: &HashId, old_len: u32,
             data: &[u8]) -> Result<()> {
        self.send_async_request(Request::Updir {
            tx: tx, id: H(*id), ver: H(*v), old_len: old_len,
            append: ByteBuf::from(data)
        })
    }

    fn rmdir(&self, tx: Tx, id: &HashId, v: &HashId, old_len: u32)
             -> Result<()> {
        self.send_async_request(Request::Rmdir {
            tx: tx, id: H(*id), ver: H(*v), old_len: old_len
        })
    }

    fn linkobj(&self, tx: Tx, id: &HashId, linkid: &HashId)
               -> Result<bool> {
        handle_response!(try!(self.send_single_sync_request(
            Request::Linkobj { tx: tx, id: H(*id), linkid: H(*linkid) }
        )) => {
            Response::Done => Ok(true),
            Response::NotFound => Ok(false),
        })
    }

    fn putobj(&self, tx: Tx, id: &HashId, linkid: &HashId, data: &[u8])
              -> Result<()> {
        self.send_async_request(Request::Putobj {
            tx: tx, id: H(*id), linkid: H(*linkid),
            data: ByteBuf::from(data)
        })
    }

    fn unlinkobj(&self, tx: Tx, id: &HashId, linkid: &HashId)
                 -> Result<()> {
        self.send_async_request(Request::Unlinkobj {
            tx: tx, id: H(*id), linkid: H(*linkid),
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

    use serde_types::rpc::ImplementationInfo;
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
