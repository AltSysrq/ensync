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

//! Various things supporting CLI usage of Ensync, such as subcommand
//! implementations, interpretation of configuration, etc.

pub mod config;
pub mod open_server;
pub mod format_date;
pub use self::open_server::*;

pub mod cmd_server;
pub mod cmd_keymgmt;
pub mod cmd_sync;
pub mod cmd_manual;
pub mod cmd_setup;
