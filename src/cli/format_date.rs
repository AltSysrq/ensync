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

use chrono::{DateTime, NaiveDateTime, UTC};

use crate::defs::FileTime;

#[allow(dead_code)]
pub const ZERO:  &'static str = "1970-01-01 00:00Z";
pub const EMPTY: &'static str = "                 ";

pub fn format_timestamp(mtime: FileTime) -> String {
    format_date(&DateTime::<UTC>::from_utc(
        NaiveDateTime::from_timestamp(mtime, 0), UTC))
}

pub fn format_date(date: &DateTime<UTC>) -> String {
    format!("{}", date.format("%Y-%m-%d %H:%MZ"))
}
