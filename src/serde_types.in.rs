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
