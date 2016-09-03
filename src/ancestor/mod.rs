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

//! This module contains the implementation of the ancestor replica.
//!
//! The ancestor replica is stored entirely in a single SQLite database. The
//! implementation is split into two pieces:
//!
//! - The `dao` submodule handles setting the database up and exposes primitive
//! operations on the store.
//!
//! - The `replica` submodule implements the `Replica` trait atop `dao`.
//!
//! The data structures of the ancestor replica are documented in the
//! definitions in `schema.sql`.

mod dao;
mod replica;

pub use self::replica::AncestorReplica;
