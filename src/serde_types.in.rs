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
    use serde::de::{Deserialize, Deserializer, Error as DesError,
                    Visitor as DesVisitor};
    use serde::ser::{Serialize, Serializer};

    use defs::HashId;

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
}

pub mod crypt {
    use std::collections::BTreeMap;

    use chrono::{DateTime, UTC};

    pub use super::rpc::H;

    /// Stored in cleartext CBOR as directory `[0u8;32]`.
    ///
    /// This stores the parameters used for the key-derivation function of each
    /// passphrase and how to move from a derived key to the master key.
    #[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
    pub struct KdfList {
        pub keys: BTreeMap<String, KdfEntry>,
    }

    /// A single passphrase which may be used to derive the master key.
    #[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
    pub struct KdfEntry {
        /// The time this (logical) entry was created.
        pub created: DateTime<UTC>,
        /// The time this (logical) entry was last updated.
        pub updated: Option<DateTime<UTC>>,
        /// The time this entry was last used to derive the master key.
        pub used: Option<DateTime<UTC>>,
        /// The algorithm used.
        ///
        /// This includes the parameters used. Note that these are not parsed;
        /// the possible combinations are hardwired.
        ///
        /// The only option right now is "scrypt-18-8-1".
        pub algorithm: String,
        /// The randomly-generated salt.
        pub salt: H,
        /// The SHA3 hash of the derived key, to determine whether the key is
        /// correct.
        pub hash: H,
        /// The pairwise XOR of the master key with this derived key, allowing
        /// the master key to be derived once this derived key is known.
        pub master_diff: H,
    }
}

pub mod dir {
    pub use super::rpc::H;

    /// Stored in the first chunk of directory contents to describe the
    /// directory.
    #[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
    pub struct Header {
        /// The id of this directory.
        ///
        /// This provides explicit protection against a directory file being
        /// replaced.
        pub dir_id: H,
        /// The numeric version of these contents.
        ///
        /// This must match what was decoded from the directory metadata. It
        /// prevents being able to roll a directory file back to an older
        /// version.
        ///
        /// This also perturbs the directory content from the very first block.
        pub ver: u64,
        /// The binary format for the rest of the directory. Each version
        /// corresponds to one of the `v*` submodules.
        pub fmt: u32,
    }

    pub mod v0 {
        use serde::bytes::ByteBuf;

        use super::H;
        use defs::*;

        /// Describes the content of a single file.
        #[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
        pub enum Entry {
            /// A subdirectory with the given mode, as with
            /// `FileData::Directory`, but also includes the id of that
            /// subdirectory.
            D(FileMode, H),
            /// A regular file. Mostly as with `FileData::Regular`, but also
            /// includes the block size and a list of alternating object ids
            /// comprising the file and their linkids.
            R(FileMode, FileSize, FileTime, H, u32, Vec<H>),
            /// A symlink, as per `FileData::Symlink`.
            S(ByteBuf),
            /// A deleted file.
            X,
        }

        /// Describes an edit for the given filename to the given content.
        ///
        /// Each chunk of a v0 directory (other than the first) is a sequence
        /// of `EntryPair`s.
        pub type EntryPair = (ByteBuf, Entry);
    }
}
