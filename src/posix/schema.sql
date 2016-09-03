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

PRAGMA foreign_keys = ON;
PRAGMA journal_mode = WAL;
-- Running multiple processes on the same client store is not useful and can
-- only cause problems, so prohibit. This also frees us from needing to handle
-- "database locked" errors, since such errors indicate that we're in a fatal
-- situation anyway.
PRAGMA locking_mode = EXCLUSIVE;

-- Caches the computed hashes of whole regular files.
--
-- This is used in testing whether a given file has really changed. Since a
-- strict comparison would require rereading the whole filesystem every time,
-- we instead use a heuristic based on high-level attributes of the file.
CREATE TABLE IF NOT EXISTS "hash_cache" (
  -- Surrogate id for this entry.
  "id"          INTEGER NOT NULL PRIMARY KEY,
  -- The absolute path of this regular file.
  "path"        BLOB NOT NULL UNIQUE,
  -- The hash that was computed for the whole file. This field is indexed.
  "hash"        BLOB NOT NULL,
  -- The block size that was used for computing `hash`; used to assign meaning
  -- to the `offset` field of `block_cache`.
  "block_size"  INTEGER NOT NULL,
  -- The inode of the file when we computed the hash. If this changes, we
  -- assume the cache is stale.
  "inode"       INTEGER NOT NULL,
  -- The size, in bytes, of the file when we computed the hash. If this
  -- changes, we assume the cache is stale.
  "size"        INTEGER NOT NULL,
  -- The modified time (a `time_t`) of the file when we computed the hash. If
  -- this changes, we assume the cache is stale.
  "mtime"       INTEGER NOT NULL,
  -- The last "generation" at which this entry was used. Each sync uses the
  -- maximum generation in any entry plus one for all files it encounters. When
  -- a sync completes, all entries under the sync root whose generation is less
  -- than the new generation is cleared.
  "generation"  INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS "hash_cache_hash"
ON "hash_cache" ("hash");

-- Caches the hashes of individual blocks within a file.
--
-- This is used for speeding updates for larger files which typically are only
-- modified partially; for example, mbox mailboxes or sqlite databases.
--
-- The validity of this cache is intrinsically tied to the corresponding
-- entries in `hash_cache`.
CREATE TABLE IF NOT EXISTS "block_cache" (
  -- The id of the file this block resides within.
  "file"        INTEGER NOT NULL
    CONSTRAINT "block_cache_fk_hash_cache_id"
    REFERENCES "hash_cache" ("id")
    ON DELETE CASCADE,
  -- The offset, in blocks, of this block within that file.
  "offset"      INTEGER NOT NULL,
  -- The hash of this block.
  "hash"        BLOB NOT NULL,
  PRIMARY KEY ("file", "offset")
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS "block_cache_hash"
ON "block_cache" ("hash");

-- Tracks what directories are marked clean.
--
-- When a directory is marked clean, a hash of what its contents should be at
-- that time (based on `list()` and any modifications made thereafter) and that
-- is recorded here. When the replica later initialises, it check each path
-- listed in this table and recomputes the hash, deleting entries whose hash no
-- longer matches, as well as entries for paths that contain the affected entry.
--
-- A directory is considered clean during the syncing process if there exists
-- any path in this table which is a prefix of that directory's path. This
-- odd-sounding semantic is a result of the fact that the reconciler might not
-- actually recurse into all directories (eg, if the rules dictate to not sync
-- a subdirectory) and not being able to track that fact would result in syncs
-- spuriously descending down to those directories every time.
CREATE TABLE IF NOT EXISTS "clean_dirs" (
  -- The absolute path of this directory, including trailing '/'.
  "path"        BLOB NOT NULL PRIMARY KEY,
  -- The hash of the directory status. How this is is calculated is found in
  -- `replica.rs`.
  "hash"        BLOB NOT NULL
) WITHOUT ROWID;
