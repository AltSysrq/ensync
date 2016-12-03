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

use std::io;
use std::sync::Arc;

use block_xfer::BlockFetch;
use defs::HashId;
use errors::*;
use server::storage::Storage;

pub struct ServerTransferOut<S : Storage> {
    storage: Arc<S>,
}

impl<S : Storage> ServerTransferOut<S> {
    pub fn new(storage: Arc<S>) -> Self {
        ServerTransferOut {
            storage: storage,
        }
    }
}

impl<S : Storage> BlockFetch for ServerTransferOut<S> {
    fn fetch(&self, block: &HashId) -> Result<Box<io::Read>> {
        Ok(Box::new(io::Cursor::new(
            self.storage.getobj(block)?
                .ok_or(ErrorKind::ServerContentDeleted)?)))
    }
}
