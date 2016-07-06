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
