---
-- Copyright (c) 2016, Jason Lingle
--
-- Permission to  use, copy,  modify, and/or distribute  this software  for any
-- purpose  with or  without fee  is hereby  granted, provided  that the  above
-- copyright notice and this permission notice appear in all copies.
--
-- THE SOFTWARE  IS PROVIDED "AS  IS" AND  THE AUTHOR DISCLAIMS  ALL WARRANTIES
-- WITH  REGARD   TO  THIS  SOFTWARE   INCLUDING  ALL  IMPLIED   WARRANTIES  OF
-- MERCHANTABILITY AND FITNESS. IN NO EVENT  SHALL THE AUTHOR BE LIABLE FOR ANY
-- SPECIAL,  DIRECT,   INDIRECT,  OR  CONSEQUENTIAL  DAMAGES   OR  ANY  DAMAGES
-- WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS, WHETHER IN AN ACTION
-- OF  CONTRACT, NEGLIGENCE  OR OTHER  TORTIOUS ACTION,  ARISING OUT  OF OR  IN
-- CONNECTION WITH THE USE OR PERFORMANCE OF THIS SOFTWARE.

PRAGMA foreign_keys = ON;
PRAGMA journal_mode = WAL;
-- Running multiple processes on the same ancestor store is not useful and can
-- only cause problems, so prohibit. This also frees us from needing to handle
-- "database locked" errors, since such errors indicate that we're in a fatal
-- situation anyway.
PRAGMA locking_mode = EXCLUSIVE;

-- The "file" table stores all the data in the ancestor replica.
--
-- Files are addressed either by their surrogate integer identifier, or by the
-- identifier of their parent and their filename.
--
-- Deleting a file representing a directory recursively deletes all files in
-- that directory tree.
--
-- Note that the size and timestamp of regular files is *not* stored, and
-- always reported as zero, since it does not participate in the sync process.
-- For the same reason, there is no system of tracking the dirty/clean state of
-- directories.
CREATE TABLE IF NOT EXISTS "file" (
  -- The surrogate id of this file.
  --
  -- This can be used to directly address the file. More commonly, it is used
  -- for referring to the files in a directory (when this entry itself is a
  -- directory).
  --
  -- The root directory always has id 0.
  --
  -- This field is AUTOINCREMENT out of paranoia; it means that if a stale
  -- reference to a directory is kept after the directory is removed, there is
  -- no possibility that it will end up referring to some new, unrelated
  -- directory.
  "id"          INTEGER PRIMARY KEY AUTOINCREMENT,
  -- The parent directory of this file. The root directory is its own parent.
  "parent"      INTEGER NOT NULL
    CONSTRAINT "file_parent_fk_file_id"
    REFERENCES "file" ("id")
    ON DELETE CASCADE,
  -- The base name of this file. The root directory has an empty name.
  "name"        BLOB NOT NULL,
  -- The type of this file; 0 = regular, 1 = dir, 2 = symlink. There is no
  -- support for special files.
  "type"        INTEGER NOT NULL,
  -- For regular files and directories, the numeric POSIX permissions. 0 for
  -- symlinks and the root directory.
  "mode"        INTEGER NOT NULL,
  -- For regular files, the 32-byte hash of the content. For symlinks, the
  -- target. Empty for directories.
  "content"     BLOB NOT NULL
);
CREATE UNIQUE INDEX IF NOT EXISTS "file_parent_name"
ON "file" ("parent", "name");

-- Stores the condemnation set for each directory, to implement the `Condemn`
-- trait.
--
-- There is no associated data with entries in this table; their presence alone
-- indicates the condemnation of a name.
CREATE TABLE IF NOT EXISTS "condemnation" (
  -- The surrogate id of the directory to which this entry applies.
  "dir"         INTEGER NOT NULL
    CONSTRAINT "condemnation_dir_fk_file_id"
    REFERENCES "file" ("id")
    ON DELETE CASCADE,
  -- The condemned filename. This does not necessarily correspond to any
  -- existing file in the directory.
  "name"        BLOB NOT NULL,

  CONSTRAINT "condemnation_pk"
  PRIMARY KEY ("dir", "name")
) WITHOUT ROWID;

-- Create the root directory if not already there
INSERT OR IGNORE INTO "file" (id, parent, name, type, mode, content)
VALUES (0, 0, "", 0, 0, "");
