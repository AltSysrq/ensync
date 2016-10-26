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

pub mod rpc {
    use serde::bytes::ByteBuf;
    use serde::de::{Deserialize, Deserializer, Error as DesError,
                    Visitor as DesVisitor};
    use serde::ser::{Serialize, Serializer};

    use defs::HashId;
    use server::storage::Tx;

    pub const PROTOCOL_VERSION_MAJOR: u32 = 0;
    pub const PROTOCOL_VERSION_MINOR: u32 = 0;

    /// Wrapper for `HashId` that causes byte arrays to be written compactly.
    #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
    pub struct H(pub HashId);
    impl Serialize for H {
        fn serialize<S : Serializer>(&self, ser: &mut S)
                                     -> Result<(), S::Error> {
            ser.serialize_bytes(&self.0[..])
        }
    }
    impl Deserialize for H {
        fn deserialize<D : Deserializer>(des: &mut D)
                                         -> Result<Self, D::Error> {
            des.deserialize_bytes(H(Default::default()))
        }
    }
    impl DesVisitor for H {
        type Value = H;
        fn visit_bytes<E : DesError>(&mut self, v: &[u8])
                                     -> Result<H, E> {
            if v.len() != self.0.len() {
                return Err(E::invalid_length(v.len()));
            }
            self.0.copy_from_slice(v);
            Ok(*self)
        }
        fn visit_byte_buf<E : DesError>(&mut self, v: Vec<u8>)
                                        -> Result<H, E> {
            self.visit_bytes(&v[..])
        }
    }

    /// Identifies a client or server implementation.
    #[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
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
        /// A major version difference in the protocol indicates a change that
        /// both sides must be aware of. A minor version difference implies
        /// that the CBOR format is backwards-compatible, and that no new
        /// server behaviours will be triggered without explicit opt-in by the
        /// client.
        ///
        /// Note that the protocol version numbers both start at 0.
        pub protocol: (u32, u32),
    }

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
    #[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
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
        GetDir(H),
        /// `Storage::getobj`
        ///
        /// Response: One `ObjData` | `NotFound` | `Error`
        GetObj(H),
        /// `Storage::fordir`
        ///
        /// Response:
        /// - Any number of `DirEntry`
        /// - One `Done` | `Error`
        ForDir,
        /// `Storage::start_tx`
        ///
        /// No response.
        StartTx(Tx),
        /// `Storage::commit`
        ///
        /// Response: One `Done` | `Fail` | `Error`
        Commit(Tx),
        /// `Storage::mkdir`
        ///
        /// No response.
        Mkdir { tx: Tx, id: H, ver: H, data: ByteBuf, },
        /// `Storage::updir`
        ///
        /// No response.
        Updir { tx: Tx, id: H, ver: H, old_len: u32,
                append: ByteBuf, },
        /// `Storage::rmdir`
        ///
        /// No response.
        Rmdir { tx: Tx, id: H, ver: H, old_len: u32, },
        /// `Storage::linkobj`
        ///
        /// Response: One `Done` | `NotFound` | `Error`
        Linkobj { tx: Tx, id: H, linkid: H },
        /// `Storage::putobj`
        ///
        /// No response.
        Putobj { tx: Tx, id: H, linkid: H, data: ByteBuf, },
        /// `Storage::unlinkobj`
        ///
        /// No response.
        Unlinkobj { tx: Tx, id: H, linkid: H },
        /// `Storage::clean_up`
        ///
        /// Response: One `Done`.
        CleanUp,
    }

    /// Responses correspoinding to various `Request`s above.
    #[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
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
        /// Metadata (id, version, length) about a directory.
        DirEntry(H, H, u32),
        /// The version and full data of a directory.
        DirData(H, ByteBuf),
        /// The full data of an object.
        ObjData(ByteBuf),
        /// Identifies information about the server.
        ServerInfo {
            /// Information about the server implementation.
            implementation: ImplementationInfo,
            /// If set, the client should display the given plaintext message
            /// to the user.
            #[serde(default)]
            motd: Option<String>,
        },
    }


    #[cfg(test)]
    mod test {
        use serde_cbor;
        use super::*;

        #[test]
        fn hashid_serialised_efficiently() {
            let request = Request::GetObj(H([0x7fu8;32]));
            let data = serde_cbor::ser::to_vec(&request).unwrap();
            println!("Data: {:?}", data);
            assert!(data.len() <= 48);

            let request = Response::ObjData(vec![0x7fu8;64].into());
            let data = serde_cbor::ser::to_vec(&request).unwrap();
            println!("Data: {:?}", data);
            assert!(data.len() <= 80);
        }
    }
}

