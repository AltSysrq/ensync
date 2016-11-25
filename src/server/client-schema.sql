---
-- Copyright (c) 2016, Jason Lingle
--
-- This file is part of Ensync.
--
-- Ensync is free software: you can  redistribute it and/or modify it under the
-- terms of  the GNU General Public  License as published by  the Free Software
-- Foundation, either version  3 of the License, or (at  your option) any later
-- version.
--
-- Ensync is distributed  in the hope that  it will be useful,  but WITHOUT ANY
-- WARRANTY; without  even the implied  warranty of MERCHANTABILITY  or FITNESS
-- FOR  A PARTICULAR  PURPOSE.  See the  GNU General  Public  License for  more
-- details.
--
-- You should have received a copy of the GNU General Public License along with
-- Ensync. If not, see <http://www.gnu.org/licenses/>.

-- Schema for the client-side state of a server replica.

PRAGMA foreign_keys = ON;
PRAGMA journal_mode = WAL;
-- Running multiple concurrent processes will only end in tears.
PRAGMA locking_mode = EXCLUSIVE;

-- Stores the greatest version that has ever been seen for each directory id.
--
-- The client will fail if it attempts to read a directory and the (encoded version,
-- length) pair is not greater than or equal to what is stored in this table.
--
-- This check prevents attackers with write access to the server filesystem
-- from reverting a directory to a previous version to cause client replicas to
-- destroy data.
CREATE TABLE IF NOT EXISTS latest_dir_ver (
  "id"          BLOB PRIMARY KEY,
  "ver"         INTEGER NOT NULL,
  "len"         INTEGER NOT NULL
) WITHOUT ROWID;
