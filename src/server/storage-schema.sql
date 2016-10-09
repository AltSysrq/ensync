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
-- Ensync. If not, see <http:--www.gnu.org/licenses/>.

-- Schema for the persistent SQLite database for server-side storage.

PRAGMA foreign_keys = ON;
-- Don't use WAL mode since it doesn't guarantee that a read lock can be
-- promoted to a write lock in the presence of concurrent writers.
PRAGMA journal_mode = PERSIST;
PRAGMA journal_size_limit = 65536;
-- Default locking mode; we need to support concurrent access.
PRAGMA busy_timeout = 5000;

-- Stores the ids, versions, and committed lengths of all directories.
--
-- The actual filename for each directory is derived from `sha3(id .. ver)` so
-- that version replacements can be performed without having the effect of
-- mutating the committed prefix of the old version. (The create-rename trick
-- doesn't help here, because we could do the rename operation between another
-- process reading the dir from the database and actually opening the file.)
--
-- Note that readers in this scheme may see a directory in this table, then
-- find that the expected file does not exist. When this happens, the reader
-- must re-read this table to find out if there is a new version of the
-- directory.
CREATE TABLE IF NOT EXISTS "dirs" (
  "id"          BLOB    NOT NULL        PRIMARY KEY,
  "ver"         BLOB    NOT NULL,
  "length"      INTEGER NOT NULL
) WITHOUT ROWID;

-- Stores the associated reference accumulator for each object in storage.
CREATE TABLE IF NOT EXISTS "objs" (
  "id"          BLOB    NOT NULL        PRIMARY KEY,
  "refs"        BLOB    NOT NULL
) WITHOUT ROWID;

CREATE INDEX IF NOT EXISTS "obj_refs"
ON "objs" ("refs");
