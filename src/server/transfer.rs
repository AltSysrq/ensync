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

use std::io;
use std::sync::Arc;

use flate2;

use block_xfer::BlockFetch;
use defs::HashId;
use errors::*;
use server::crypt::{KeyChain, decrypt_obj};
use server::storage::Storage;

pub struct ServerTransferOut<S : Storage + ?Sized> {
    storage: Arc<S>,
    key: Arc<KeyChain>,
}

impl<S : Storage + ?Sized> ServerTransferOut<S> {
    pub fn new(storage: Arc<S>, key: Arc<KeyChain>) -> Self {
        ServerTransferOut {
            storage: storage,
            key: key,
        }
    }
}

impl<S : Storage + ?Sized> BlockFetch for ServerTransferOut<S> {
    fn fetch(&self, block: &HashId) -> Result<Box<io::Read>> {
        let ciphertext = self.storage.getobj(block)?
            .ok_or(ErrorKind::ServerContentDeleted)?;
        let mut cleartext = Vec::<u8>::with_capacity(
            ciphertext.len() * 3 / 2);
        decrypt_obj(&mut cleartext, &ciphertext[..], &self.key)?;

        Ok(Box::new(flate2::read::GzDecoder::new(
            io::Cursor::new(cleartext))?))
    }
}
