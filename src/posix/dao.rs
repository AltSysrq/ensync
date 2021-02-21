//-
// Copyright (c) 2016, 2017, 2021, Jason Lingle
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

use std::ffi::{OsStr, OsString};
use std::path::Path;

use sqlite::{State, Statement};

use crate::defs::*;
use crate::errors::*;
use crate::sql::*;

/// Front-end to SQLite for the POSIX (client-side) replica.
///
/// This is higher-level than the otherwise comparable ancestor DAO, in that it
/// handles all the semantics of the underlying database rather than being a
/// simple translation layer.
pub struct Dao(VolatileConnection);

/// Iterator over the directories defined in the `clean_dirs` table.
pub struct CleanDirs<'a>(Statement<'a>);

fn to_hashid(v: Vec<u8>) -> Result<HashId> {
    if 32 == v.len() {
        let mut ret = [0; 32];
        ret.copy_from_slice(&*v);
        Ok(ret)
    } else {
        Err(ErrorKind::InvalidHash.into())
    }
}

impl<'a> Iterator for CleanDirs<'a> {
    type Item = Result<(OsString, HashId)>;

    fn next(&mut self) -> Option<Result<(OsString, HashId)>> {
        match self.0.next() {
            Ok(State::Row) => {}
            Ok(State::Done) => return None,
            Err(err) => return Some(Err(err.into())),
        }

        Some(self.read_row())
    }
}

impl<'a> CleanDirs<'a> {
    fn read_row(&mut self) -> Result<(OsString, HashId)> {
        let path = self.0.read::<Vec<u8>>(0)?.as_nstr()?.to_owned();
        let hash = to_hashid(self.0.read::<Vec<u8>>(1)?)?;
        Ok((path, hash))
    }
}

/// The subset of `struct stat` that we actually care about here.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InodeStatus {
    pub ino: FileInode,
    pub mtime: FileTime,
    pub size: FileSize,
}

impl Dao {
    /// Opens a `Dao` using the given `path` as the backing database.
    ///
    /// `path` is passed verbatim to SQLite, so `":memory:"` can be used to
    /// open a temporary, in-memory database.
    ///
    /// The database is implicitly initialised and/or updated as needed.
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self> {
        Ok(Dao(VolatileConnection::new(
            path,
            include_str!("schema.sql"),
            "This sync may be slower than normal \
             while things are recalculated.",
        )?))
    }

    /// Returns an iterator over the whole `clean_dirs` table.
    ///
    /// Until this iterator is dropped, one should not call any methods that
    /// operate on the `clean_dirs` table other than `set_dir_dirty`.
    pub fn iter_clean_dirs(&self) -> Result<CleanDirs> {
        let stmt = self.0.prepare("SELECT `path`, `hash` FROM `clean_dirs`")?;

        Ok(CleanDirs(stmt))
    }

    /// Adds a record marking `path_str` as clean using the given `hash`.
    ///
    /// If there is already a record for `path_str`, it is updated to reference
    /// `hash`.
    pub fn set_dir_clean(&self, path_str: &OsStr, hash: &HashId) -> Result<()> {
        let path = path_str.as_nbytes();

        debug_assert!(b'/' == path[0]);
        debug_assert!(b'/' == path[path.len() - 1]);

        self.0
            .prepare(
                "INSERT OR REPLACE INTO `clean_dirs` (`path`, `hash`) \
             VALUES (?1, ?2)",
            )
            .binding(1, path)
            .binding(2, &hash[..])
            .run()?;
        Ok(())
    }

    /// Marks the directory named by `path_str` as dirty, as well as all its
    /// parent directories.
    ///
    /// NB This is usually called while the `SELECT` statement from
    /// `iter_clean_dirs` is still active. Since we don't use any particular
    /// iteration order over the directories, it is possible a parent of an
    /// entry deleted by this call is actually ordered after `path_str`. In
    /// such a case, whether that directory shows up in the `SELECT` results is
    /// unspecified (see http://sqlite.org/isolation.html) but produces
    /// reasonable results either way.
    pub fn set_dir_dirty(&self, path_str: &OsStr) -> Result<()> {
        let mut path = path_str.as_nbytes();

        loop {
            debug_assert!(b'/' == path[0]);
            debug_assert!(b'/' == path[path.len() - 1]);

            self.0
                .prepare("DELETE FROM `clean_dirs` WHERE `path` = ?1")
                .binding(1, path)
                .run()?;

            if let Some(slash) =
                path[0..path.len() - 1].iter().rposition(|c| b'/' == *c)
            {
                path = &path[0..slash + 1];
            } else {
                break;
            }
        }

        Ok(())
    }

    /// Returns whether the directory indicated by path `path_str` is
    /// considered clean.
    ///
    /// `path_str` must have both leading and trailing slash.
    pub fn is_dir_clean(&self, path_str: &OsStr) -> Result<bool> {
        let mut path = path_str.as_nbytes();

        loop {
            debug_assert!(b'/' == path[0]);
            debug_assert!(b'/' == path[path.len() - 1]);

            if self
                .0
                .prepare(
                    "SELECT 1 FROM `clean_dirs` \
                                    WHERE `path` = ?1",
                )
                .binding(1, path)
                .exists()?
            {
                return Ok(true);
            }

            if let Some(slash) =
                path[0..path.len() - 1].iter().rposition(|c| b'/' == *c)
            {
                path = &path[0..slash + 1];
            } else {
                break;
            }
        }

        Ok(false)
    }

    /// Clears all clean directory records.
    pub fn set_all_dirs_dirty(&self) -> Result<()> {
        Ok(self.0.prepare("DELETE FROM `clean_dirs`").run()?)
    }

    /// Determines the new generation number for the cache.
    pub fn next_generation(&self) -> Result<i64> {
        Ok(self
            .0
            .prepare(
                "SELECT MAX(`generation`) + 1 \
                                FROM `hash_cache`",
            )
            .first(|s| s.read(0))
            .map(|o| o.unwrap_or(0))?)
    }

    /// Populates the hash and block caches with the given file.
    ///
    /// `path_str` identifies the file whose contents are now known; `hash` is
    /// its overall hash value, whereas `blocks` lists each individual block in
    /// the file. `block_size` indicates the block size used in the
    /// calculation. `stat` indicates the status of the file at the time the
    /// hashes were computed.
    pub fn cache_file_hashes(
        &self,
        path_str: &OsStr,
        hash: &HashId,
        blocks: &[HashId],
        block_size: usize,
        stat: &InodeStatus,
        generation: i64,
    ) -> Result<()> {
        let path = path_str.as_nbytes();

        // Kill any existing entry to transitively remove any block caches
        // as well.
        self.0
            .prepare("DELETE FROM `hash_cache` WHERE `path` = ?1")
            .binding(1, path)
            .run()?;

        self.0
            .prepare(
                "INSERT INTO `hash_cache` ( \
                             path, hash, block_size, inode, size, mtime, \
                             generation \
                             ) VALUES ( \
                             ?1,   ?2,   ?3,         ?4,    ?5,   ?6, \
                             ?7)",
            )
            .binding(1, path)
            .binding(2, &hash[..])
            .binding(3, block_size as i64)
            .binding(4, stat.ino as i64)
            .binding(5, stat.size as i64)
            .binding(6, stat.mtime as i64)
            .binding(7, generation)
            .run()?;
        let id = self
            .0
            .prepare(
                "SELECT `id` FROM `hash_cache` \
                                      WHERE `path` = ?1",
            )
            .binding(1, path)
            .first(|s| s.read::<i64>(0))?
            .expect("Couldn't find the id of the row just inserted");
        for (ix, block) in blocks.iter().enumerate() {
            self.0
                .prepare(
                    "INSERT INTO `block_cache` ( \
                                 file, offset, hash \
                                 ) VALUES ( \
                                 ?1,   ?2,     ?3)",
                )
                .binding(1, id)
                .binding(2, ix as i64)
                .binding(3, &block[..])
                .run()?;
        }
        Ok(())
    }

    /// Looks up whether the hash of the file identified by `path` exists and
    /// still matches `stat`.
    ///
    /// If there is such an entry, the hash is returned. Note that this hash
    /// may have been computed with a different block size than what this sync
    /// will ultimately use.
    ///
    /// When an entry is matched, its generation is updated to `generation` so
    /// that it will survive pruning.
    pub fn cached_file_hash(
        &self,
        path: &OsStr,
        stat: &InodeStatus,
        generation: i64,
    ) -> Result<Option<HashId>> {
        if let Some((id, hashvec)) = self
            .0
            .prepare(
                "SELECT `id`, `hash` FROM `hash_cache` \
                 WHERE `path` = ?1 \
                 AND   `inode` = ?2 AND `size` = ?3 \
                 AND   `mtime` = ?4",
            )
            .binding(1, path.as_nbytes())
            .binding(2, stat.ino as i64)
            .binding(3, stat.size as i64)
            .binding(4, stat.mtime as i64)
            .first(|s| Ok((s.read::<i64>(0)?, s.read::<Vec<u8>>(1)?)))?
        {
            self.0
                .prepare(
                    "UPDATE `hash_cache` \
                 SET `generation` = ?2 \
                 WHERE `id` = ?1",
                )
                .binding(1, id)
                .binding(2, generation)
                .run()?;

            Ok(Some(to_hashid(hashvec)?))
        } else {
            Ok(None)
        }
    }

    /// Prunes from the file and block hash caches all entries beneath `root`
    /// whose generation predates `generation`.
    pub fn prune_hash_cache(
        &self,
        root: &OsStr,
        generation: i64,
    ) -> Result<()> {
        let mut lower = root.as_nbytes().to_vec();
        if Some(&b'/') != lower.last() {
            lower.push(b'/');
        }
        let mut upper = lower.clone();
        *upper.last_mut().unwrap() = b'/' + 1;

        self.0
            .prepare(
                "DELETE FROM `hash_cache` \
                             WHERE `path` >= ?1 AND path < ?2 \
                             AND `generation` < ?3",
            )
            .binding(1, &lower[..])
            .binding(2, &upper[..])
            .binding(3, generation)
            .run()?;
        Ok(())
    }

    /// Searches for a file whose hash equals `hash`.
    ///
    /// If found, returns the absolute path of that file. Note that there is no
    /// guarantee that the file still has that hash, or that it even exists for
    /// that matter.
    pub fn find_file_with_hash(
        &self,
        hash: &HashId,
    ) -> Result<Option<OsString>> {
        let pathvec = self
            .0
            .prepare(
                "SELECT `path` FROM `hash_cache` \
                                           WHERE `hash` = ?1 \
                                           LIMIT 1",
            )
            .binding(1, &hash[..])
            .first(|s| s.read::<Vec<u8>>(0))?;

        if let Some(pv) = pathvec {
            Ok(Some(pv.as_nstr()?.to_owned()))
        } else {
            Ok(None)
        }
    }

    /// Searches for a single block with the given hash.
    ///
    /// If there is one, returns the file containing the block and its size and
    /// block offset within that file. Note that the "size of the block" refers
    /// to the block size in use and not necessarily the byte length of the
    /// block, which must be determined by actually reading the file and seeing
    /// whether EOF occurs first.
    ///
    /// There is no guarantee that the block in that file still has that hash,
    /// that the file is actually large enough to contain that block, or even
    /// that the file still exists.
    pub fn find_block_with_hash(
        &self,
        hash: &HashId,
    ) -> Result<Option<(OsString, i64, i64)>> {
        let res = self
            .0
            .prepare(
                "SELECT `hash_cache`.`path`, `hash_cache`.`block_size`, \
                    `block_cache`.`offset` \
             FROM `block_cache` JOIN `hash_cache` \
             ON   `block_cache`.`file` = `hash_cache`.`id` \
             WHERE `block_cache`.`hash` = ?1 LIMIT 1",
            )
            .binding(1, &hash[..])
            .first(|s| {
                Ok((
                    s.read::<Vec<u8>>(0)?,
                    s.read::<i64>(1)?,
                    s.read::<i64>(2)?,
                ))
            })?;

        if let Some((pathvec, bs, off)) = res {
            Ok(Some((pathvec.as_nstr()?.to_owned(), bs, off)))
        } else {
            Ok(None)
        }
    }

    /// Updates the modified time on the hash cache entry (if any) for the
    /// given path.
    pub fn update_cache_mtime(
        &self,
        path: &OsStr,
        mtime: FileTime,
    ) -> Result<()> {
        Ok(self
            .0
            .prepare(
                "UPDATE `hash_cache` SET `mtime` = ?2 \
                           WHERE `path` = ?1",
            )
            .binding(1, path.as_nbytes())
            .binding(2, mtime as i64)
            .run()?)
    }

    /// Updates any cache for the file at path `old` to be at path `new`,
    /// preserving all caching information.
    pub fn rename_cache(&self, old: &OsStr, new: &OsStr) -> Result<()> {
        Ok(self
            .0
            .prepare(
                "UPDATE `hash_cache` SET `path` = ?2 \
                                WHERE `path` = ?1",
            )
            .binding(1, old.as_nbytes())
            .binding(2, new.as_nbytes())
            .run()?)
    }

    /// Deletes any cache entry for the file at `path`.
    pub fn delete_cache(&self, path: &OsStr) -> Result<()> {
        Ok(self
            .0
            .prepare("DELETE FROM `hash_cache` WHERE `path` = ?1")
            .binding(1, path.as_nbytes())
            .run()?)
    }

    /// Completely clears the hash cache.
    pub fn purge_hash_cache(&self) -> Result<()> {
        Ok(self.0.prepare("DELETE FROM `hash_cache`").run()?)
    }
}

#[cfg(test)]
mod test {
    use std::collections::HashSet;

    use super::*;
    use crate::defs::test_helpers::*;

    fn new() -> Dao {
        Dao::open(":memory:").unwrap()
    }

    #[test]
    fn empty_clean_dirs() {
        let dao = new();

        assert!(dao.iter_clean_dirs().unwrap().next().is_none());
        dao.set_dir_dirty(&oss("/foo/bar/baz/")).unwrap();
    }

    static H: HashId = [1; 32];

    #[test]
    fn clean_dir_crd() {
        let dao = new();

        dao.set_dir_clean(&oss("/foo/"), &H).unwrap();
        dao.set_dir_clean(&oss("/foo/bar/"), &H).unwrap();
        dao.set_dir_clean(&oss("/foo/bar/baz/"), &H).unwrap();
        dao.set_dir_clean(&oss("/foo/plugh/"), &H).unwrap();

        let mut seen = HashSet::new();
        for (name, hash) in dao.iter_clean_dirs().unwrap().map(|r| r.unwrap()) {
            if oss("/foo/") == name || oss("/foo/plugh/") == name {
                // Leave alone
            } else if oss("/foo/bar/") == name || oss("/foo/bar/baz/") == name {
                dao.set_dir_dirty(&name).unwrap();
            } else {
                panic!("Unexpected name returned: {:?}", name);
            }

            seen.insert(name);
            assert_eq!(H, hash);
        }

        assert!(seen.len() <= 4);
        assert!(seen.contains(&oss("/foo/bar/baz/")));
        assert!(seen.contains(&oss("/foo/plugh/")));
        // Whether /foo/ and /foo/bar/ are actually iterated is unspecified,
        // since that row may be deleted before the select gets there.

        let second = dao
            .iter_clean_dirs()
            .unwrap()
            .map(|r| r.unwrap())
            .collect::<Vec<_>>();
        assert_eq!(vec![(oss("/foo/plugh/"), H)], second);
    }

    #[test]
    fn clean_dir_test() {
        let dao = new();

        dao.set_dir_clean(&oss("/foo/bar/"), &H).unwrap();
        assert!(dao.is_dir_clean(&oss("/foo/bar/")).unwrap());
        assert!(dao.is_dir_clean(&oss("/foo/bar/baz/")).unwrap());
        assert!(!dao.is_dir_clean(&oss("/foo/plugh/")).unwrap());
        assert!(!dao.is_dir_clean(&oss("/foo/")).unwrap());
        assert!(!dao.is_dir_clean(&oss("/")).unwrap());
    }

    #[test]
    fn hash_cache_query_by_file() {
        let dao = new();
        let path = oss("/foo");
        let stat = InodeStatus {
            ino: 42,
            mtime: 56,
            size: 1024,
        };

        assert_eq!(0, dao.next_generation().unwrap());
        assert!(dao.cached_file_hash(&path, &stat, 0).unwrap().is_none());

        dao.cache_file_hashes(
            &path,
            &[1; 32],
            &[[2; 32], [3; 32]],
            2048,
            &stat,
            0,
        )
        .unwrap();
        assert_eq!(1, dao.next_generation().unwrap());
        assert_eq!(
            [1; 32],
            dao.cached_file_hash(&path, &stat, 99).unwrap().unwrap()
        );
        assert_eq!(100, dao.next_generation().unwrap());

        assert!(dao
            .cached_file_hash(&path, &InodeStatus { ino: 4, ..stat }, 0)
            .unwrap()
            .is_none());
        assert!(dao
            .cached_file_hash(&path, &InodeStatus { mtime: 42, ..stat }, 0)
            .unwrap()
            .is_none());
        assert!(dao
            .cached_file_hash(&path, &InodeStatus { size: 42, ..stat }, 0)
            .unwrap()
            .is_none());
        assert!(dao
            .cached_file_hash(&oss("/bar"), &stat, 0)
            .unwrap()
            .is_none());
    }

    #[test]
    fn updating_hash_cache_clears_old_blocks() {
        let dao = new();
        let path = oss("/foo");
        let stat = InodeStatus {
            ino: 42,
            mtime: 56,
            size: 1024,
        };

        dao.cache_file_hashes(
            &path,
            &[1; 32],
            &[[2; 32], [3; 32]],
            2048,
            &stat,
            0,
        )
        .unwrap();
        assert!(dao.find_block_with_hash(&[2; 32]).unwrap().is_some());
        dao.cache_file_hashes(&path, &[1; 32], &[], 2048, &stat, 0)
            .unwrap();
        assert!(dao.find_block_with_hash(&[2; 32]).unwrap().is_none());
    }

    #[test]
    fn query_hash_cache_by_hash() {
        let dao = new();
        let path = oss("/foo");
        let stat = InodeStatus {
            ino: 42,
            mtime: 56,
            size: 1024,
        };

        assert!(dao.find_file_with_hash(&[1; 32]).unwrap().is_none());
        assert!(dao.find_block_with_hash(&[2; 32]).unwrap().is_none());

        dao.cache_file_hashes(
            &path,
            &[1; 32],
            &[[2; 32], [3; 32]],
            2048,
            &stat,
            0,
        )
        .unwrap();

        assert_eq!(path, dao.find_file_with_hash(&[1; 32]).unwrap().unwrap());
        assert!(dao.find_file_with_hash(&[2; 32]).unwrap().is_none());

        assert_eq!(
            (path, 2048, 1),
            dao.find_block_with_hash(&[3; 32]).unwrap().unwrap()
        );
        assert!(dao.find_block_with_hash(&[4; 32]).unwrap().is_none());
    }

    #[test]
    fn prune_hash_cache() {
        let dao = new();
        let stat = InodeStatus {
            ino: 42,
            mtime: 56,
            size: 1024,
        };

        dao.cache_file_hashes(&oss("/foo/bar"), &[1; 32], &[], 2048, &stat, 0)
            .unwrap();
        dao.cache_file_hashes(&oss("/foo/new"), &[2; 32], &[], 2048, &stat, 1)
            .unwrap();
        dao.cache_file_hashes(&oss("/fooo"), &[3; 32], &[], 2048, &stat, 0)
            .unwrap();
        dao.prune_hash_cache(&oss("/foo"), 1).unwrap();

        // /foo/bar removed since it's under /foo and has a generation of 0.
        assert!(dao
            .cached_file_hash(&oss("/foo/bar"), &stat, 2)
            .unwrap()
            .is_none());
        // /foo/new survives since it has a generation of 1.
        assert!(dao
            .cached_file_hash(&oss("/foo/new"), &stat, 2)
            .unwrap()
            .is_some());
        // /fooo survives since it is not under /foo
        assert!(dao
            .cached_file_hash(&oss("/fooo"), &stat, 2)
            .unwrap()
            .is_some());
    }

    #[test]
    fn rename_delete_cached_files() {
        let dao = new();
        let stat = InodeStatus {
            ino: 42,
            mtime: 56,
            size: 1024,
        };

        dao.cache_file_hashes(&oss("/foo"), &[1; 32], &[], 2048, &stat, 0)
            .unwrap();
        assert_eq!(
            oss("/foo"),
            dao.find_file_with_hash(&[1; 32]).unwrap().unwrap()
        );
        dao.rename_cache(&oss("/foo"), &oss("/bar")).unwrap();
        assert_eq!(
            oss("/bar"),
            dao.find_file_with_hash(&[1; 32]).unwrap().unwrap()
        );
        dao.delete_cache(&oss("/bar")).unwrap();
        assert!(dao.find_file_with_hash(&[1; 32]).unwrap().is_none());
    }
}
