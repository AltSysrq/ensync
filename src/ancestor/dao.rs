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

use std::borrow::Cow;
use std::path::Path;

use sqlite::*;

use crate::sql::*;

/// Mid-layer interface atop SQLite for the ancestor store.
///
/// It specifically provides a set of atomic operations on the ancestor store,
/// and handles mapping data to and from SQLite. Unexpected errors from SQLite
/// are passed through; proper results are always returned in `Ok`.
///
/// The DAO is oblivious to the specific semantics of most of the data fields;
/// particularly, it assigns no meaning to file type, mode, or content.
pub struct Dao(VolatileConnection);

/// A single row in the ancestor store. This is used both as a result from
/// reading from the database as well as an input for filtering and writing.
///
/// See `schema.sql` for a description of these fields and their semantics.
#[derive(Debug)]
pub struct FileEntry<'a> {
    pub id: i64,
    pub parent: i64,
    pub name: Cow<'a, [u8]>,
    pub typ: i64,
    pub mode: i64,
    pub mtime: i64,
    pub content: Cow<'a, [u8]>,
}

/// Status code returned by `Dao::rename`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RenameStatus {
    /// The rename succeeded.
    Ok,
    /// The rename failed because the file to be renamed does not exist.
    SourceNotFound,
    /// The rename failed because the new name for the file is already in use
    /// by another file.
    DestExists,
}

/// Status code returned by `Dao::update`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdateStatus {
    /// The update succeeded.
    Ok,
    /// The update failed because no such file exists.
    NotFound,
    /// The update failed because, while a file with the given parent and name
    /// does exist, it does not match the expected old version.
    NotMatched,
}

/// Status code returned by `Dao::delete`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeleteStatus {
    /// The delete succeeded.
    Ok,
    /// The delete failed because there exist files whose `parent` equals the
    /// `id` of the file to be deleted.
    DirNotEmpty,
    /// The delete had no effect because no file with the given parent and name
    /// exists.
    NotFound,
    /// The delete failed because, while a file with the given parent and name
    /// does exist, it does not match the expected old version.
    NotMatched,
}

impl FileEntry<'static> {
    fn read(stmt: &Statement) -> Result<Self> {
        Ok(FileEntry {
            id: stmt.read(0)?,
            parent: stmt.read(1)?,
            name: stmt.read::<Vec<u8>>(2)?.into(),
            typ: stmt.read(3)?,
            mode: stmt.read(4)?,
            mtime: stmt.read(5)?,
            content: stmt.read::<Vec<u8>>(6)?.into(),
        })
    }
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
            "This sync will be much more conservative than normal, as all \
             files will be treated as new. (E.g., deleted files will be \
             recreated, edited files will appear as conflicts, etc.)",
        )?))
    }

    /// Retrieves a directory list.
    ///
    /// If `purge_condemned` is true, any condemned names in the directory are
    /// deleted recursively, and the condemnation list for the directory is
    /// cleared.
    ///
    /// After condemnation, each `FileEntry` is read and fed into `f`. Note
    /// that `f` has no way to signal failure; it should simply store failure
    /// itself and then the caller handle it when this call returns.
    ///
    /// On success, returns whether any file with id `dir` actually exists.
    ///
    /// This call is completely atomic.
    pub fn list<F: FnMut(FileEntry<'static>)>(
        &self,
        purge_condemned: bool,
        dir: i64,
        mut f: F,
    ) -> Result<bool> {
        tx(&self.0, || {
            if purge_condemned {
                self.0
                    .prepare(
                        "DELETE FROM `file` \
                     WHERE `parent` = ?1 \
                     AND   `name` IN ( \
                       SELECT `name` FROM `condemnation` \
                       WHERE `parent` = ?1)",
                    )
                    .binding(1, dir)
                    .run()?;

                self.0
                    .prepare(
                        "DELETE FROM `condemnation` \
                     WHERE `dir` = ?1",
                    )
                    .binding(1, dir)
                    .run()?;
            }

            let mut stmt = self
                .0
                .prepare(
                    "SELECT `id`, `parent`, `name`, `type`, `mode`, \
                        `mtime`, `content` \
                 FROM `file` \
                 WHERE `parent` = ?1 \
                 -- Exclude the root directory from seeing itself \n\
                 AND `id` != 0",
                )
                .binding(1, dir)?;

            while State::Done != stmt.next()? {
                f(FileEntry::read(&stmt)?);
            }

            self.0
                .prepare("SELECT 1 FROM `file` WHERE `id` = ?1")
                .binding(1, dir)
                .exists()
        })
    }

    /// Returns the surrogate id of the entry with the exact parameters given
    /// exists.
    ///
    /// The surrogate id on `e` has no effect.
    pub fn get_matching(&self, e: &FileEntry) -> Result<Option<i64>> {
        self.0
            .prepare(
                "SELECT `id` FROM `file` \
             WHERE `parent` = ?1 \
             AND   `name` = ?2 \
             AND   `type` = ?3 \
             AND   `mode` = ?4 \
             AND   `mtime` = ?5 \
             AND   `content` = ?6",
            )
            .binding(1, e.parent)
            .binding(2, &*e.name)
            .binding(3, e.typ)
            .binding(4, e.mode)
            .binding(5, e.mtime)
            .binding(6, &*e.content)
            .first(|s| s.read(0))
    }

    /// Returns whether any file in the given directory with the given name
    /// exists.
    pub fn exists(&self, parent: i64, name: &[u8]) -> Result<bool> {
        self.0
            .prepare("SELECT 1 FROM `file` WHERE `parent` = ?1 AND `name` = ?2")
            .binding(1, parent)
            .binding(2, name)
            .exists()
    }

    /// Returns the id of the file in the given directory with the given name,
    /// or None if there is no such file.
    pub fn get_id_of(&self, parent: i64, name: &[u8]) -> Result<Option<i64>> {
        self.0
            .prepare(
                "SELECT `id` FROM `file` \
                        WHERE `parent` = ?1 AND `name` = ?2",
            )
            .binding(1, parent)
            .binding(2, name)
            .first(|s| s.read(0))
    }
    /// Returns the entry with the given parent and name, or None if there is
    /// no such file.
    pub fn get_by_name(
        &self,
        parent: i64,
        name: &[u8],
    ) -> Result<Option<FileEntry<'static>>> {
        self.0
            .prepare(
                "SELECT `id`, `parent`, `name`, `type`, `mode`, \
                    `mtime`, `content` \
             FROM `file` \
             WHERE `parent` = ?1 AND `name` = ?2",
            )
            .binding(1, parent)
            .binding(2, name)
            .first(|s| FileEntry::read(s))
    }

    /// Returns whether there exist any direct children of the given directory.
    pub fn has_children(&self, parent: i64) -> Result<bool> {
        self.0
            .prepare("SELECT 1 FROM `file` WHERE `parent` = ?1 LIMIT 1")
            .binding(1, parent)
            .exists()
    }

    /// Atomically renames the file in `parent` identified by `old` to have the
    /// name `new`.
    ///
    /// Fails if `old` does not exist in `parent` or if `new` does.
    pub fn rename(
        &self,
        parent: i64,
        old: &[u8],
        new: &[u8],
    ) -> Result<RenameStatus> {
        tx(&self.0, || {
            if !self.exists(parent, old)? {
                Ok(RenameStatus::SourceNotFound)
            } else if self.exists(parent, new)? {
                Ok(RenameStatus::DestExists)
            } else {
                self.0
                    .prepare(
                        "UPDATE `file` SET `name` = ?3 \
                     WHERE `parent` = ?1 AND `name` = ?2",
                    )
                    .binding(1, parent)
                    .binding(2, old)
                    .binding(3, new)
                    .run()?;
                Ok(RenameStatus::Ok)
            }
        })
    }

    /// Creates a file matching `e` (but with a new surrogate id).
    ///
    /// If a file was created, returns its id. If a file with the chosen name
    /// and parent already exists, returns None.
    pub fn create(&self, e: &FileEntry) -> Result<Option<i64>> {
        tx(&self.0, || {
            if self.exists(e.parent, &*e.name)? {
                Ok(None)
            } else {
                self.0
                    .prepare(
                        "INSERT INTO `file` ( \
                       `parent`, `name`, `type`, `mode`, `mtime`, `content` \
                     ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    )
                    .binding(1, e.parent)
                    .binding(2, &*e.name)
                    .binding(3, e.typ)
                    .binding(4, e.mode)
                    .binding(5, e.mtime)
                    .binding(6, &*e.content)
                    .run()?;
                Ok(self.get_id_of(e.parent, &*e.name)?)
            }
        })
    }

    /// Updates a file matching `old` to the data in `new`.
    ///
    /// Iff a file matching `old` exists, its data fields are updated to match
    /// `new`. The `id`, `parent`, and `name` of `new` have no effect.
    pub fn update(
        &self,
        old: &FileEntry,
        new: &FileEntry,
    ) -> Result<UpdateStatus> {
        tx(&self.0, || {
            if let Some(id) = self.get_matching(old)? {
                self.0
                    .prepare(
                        "UPDATE `file` \
                     SET `type` = ?2, `mode` = ?3, \
                         `mtime` = ?4, `content` = ?5 \
                     WHERE id = ?1",
                    )
                    .binding(1, id)
                    .binding(2, new.typ)
                    .binding(3, new.mode)
                    .binding(4, new.mtime)
                    .binding(5, &*new.content)
                    .run()?;
                Ok(UpdateStatus::Ok)
            } else if self.exists(old.parent, &*old.name)? {
                Ok(UpdateStatus::NotMatched)
            } else {
                Ok(UpdateStatus::NotFound)
            }
        })
    }

    /// Deletes the file matching the given entry.
    ///
    /// Iff a file matching `e` (as per `get_matching()`) exists and has no
    /// children, it is removed.
    ///
    /// The surrogate id on `e` is not used.
    pub fn delete(&self, e: &FileEntry) -> Result<DeleteStatus> {
        tx(&self.0, || {
            if let Some(id) = self.get_matching(e)? {
                self.delete_raw_impl(id)
            } else if self.exists(e.parent, &*e.name)? {
                Ok(DeleteStatus::NotMatched)
            } else {
                Ok(DeleteStatus::NotFound)
            }
        })
    }

    /// Deletes a file directly by its id.
    ///
    /// This is still subject to the empty directory restriction, but otherwise
    /// will always remove anything there.
    ///
    /// If `id` does not refer to an existing file, this call silently
    /// succeeds.
    pub fn delete_raw(&self, id: i64) -> Result<DeleteStatus> {
        tx(&self.0, || self.delete_raw_impl(id))
    }

    fn delete_raw_impl(&self, id: i64) -> Result<DeleteStatus> {
        if self.has_children(id)? {
            Ok(DeleteStatus::DirNotEmpty)
        } else {
            self.0
                .prepare("DELETE FROM `file` WHERE `id` = ?1")
                .binding(1, id)
                .run()?;
            Ok(DeleteStatus::Ok)
        }
    }

    /// Condemns the name `name` within the directory identified by `parent`.
    ///
    /// This fails if `parent` does not exist or if `name` is already condmned.
    /// This is a situation that ordinarily should not happen, since both cases
    /// imply concurrent modification by another process (since this process
    /// would have cleared all condemnations in `parent` when it invoked
    /// `list`).
    pub fn condemn(&self, parent: i64, name: &[u8]) -> Result<()> {
        self.0
            .prepare(
                "INSERT INTO `condemnation` (`dir`,`name`) \
                        VALUES (?1, ?2)",
            )
            .binding(1, parent)
            .binding(2, name)
            .run()
    }

    /// Clears the condemnation for the name `name` in directory `parent`.
    ///
    /// If no such condemnation exists, this call quietly succeeds.
    pub fn uncondemn(&self, parent: i64, name: &[u8]) -> Result<()> {
        self.0
            .prepare(
                "DELETE FROM `condemnation`  \
                        WHERE `dir` = ?1 AND `name` = ?2",
            )
            .binding(1, parent)
            .binding(2, name)
            .run()
    }
}

#[cfg(test)]
mod test {
    use std::borrow::Cow;

    use super::*;

    fn mkdao() -> Dao {
        Dao::open(":memory:").unwrap()
    }

    #[test]
    fn list_empty_root_dir() {
        let dao = mkdao();
        let mut listed = Vec::new();

        let exists = dao.list(true, 0, |e| listed.push(e)).unwrap();
        assert_eq!(0, listed.len());
        assert!(exists);
    }

    #[test]
    fn list_nx_dir() {
        let dao = mkdao();

        let exists = dao
            .list(true, 42, |_| panic!("Element found in nx dir"))
            .unwrap();
        assert!(!exists);
    }

    #[test]
    fn create_and_list_item() {
        let dao = mkdao();

        assert!(dao
            .create(&FileEntry {
                id: -1,
                parent: 0,
                name: Cow::Borrowed(b"foo"),
                typ: 2,
                mode: 0o777,
                mtime: 1234,
                content: Cow::Borrowed(b"baz"),
            })
            .unwrap()
            .is_some());

        let mut listed = Vec::new();
        assert!(dao.list(true, 0, |e| listed.push(e)).unwrap());
        assert_eq!(1, listed.len());

        let e = listed.into_iter().next().unwrap();
        assert!(e.id >= 1);
        assert_eq!(0, e.parent);
        assert_eq!(b"foo", &*e.name);
        assert_eq!(2, e.typ);
        assert_eq!(0o777, e.mode);
        assert_eq!(1234, e.mtime);
        assert_eq!(b"baz", &*e.content);

        // Test again with a subdir so we're sure parent is stored correctly
        assert!(dao
            .create(&FileEntry {
                id: -1,
                parent: e.id,
                // Same name in case it somehow collides
                name: Cow::Borrowed(b"foo"),
                typ: 1,
                mode: 0o666,
                mtime: 5678,
                content: Cow::Borrowed(b"xyzzy"),
            })
            .unwrap()
            .is_some());

        let mut listed = Vec::new();
        assert!(dao.list(true, e.id, |s| listed.push(s)).unwrap());
        assert_eq!(1, listed.len());

        let f = listed.into_iter().next().unwrap();
        assert_eq!(e.id, f.parent);
    }

    #[test]
    fn create_name_conflict() {
        let dao = mkdao();

        let e = FileEntry {
            id: -1,
            parent: 0,
            name: Cow::Borrowed(b"foo"),
            typ: 2,
            mode: 0o777,
            mtime: 1234,
            content: Cow::Borrowed(b""),
        };

        assert!(dao.create(&e).unwrap().is_some());
        assert!(!dao.create(&e).unwrap().is_some());
    }

    fn se(
        parent: i64,
        name: &'static [u8],
        mode: i64,
        mtime: i64,
    ) -> FileEntry<'static> {
        FileEntry {
            id: -1,
            parent: parent,
            name: Cow::Borrowed(name),
            typ: 0,
            mode: mode,
            mtime: mtime,
            content: Cow::Borrowed(b""),
        }
    }

    fn list(dao: &Dao, purge: bool, dir: i64) -> Vec<FileEntry<'static>> {
        let mut listed = Vec::new();
        assert!(dao.list(purge, dir, |e| listed.push(e)).unwrap());
        listed
    }

    #[test]
    fn update_nx_file() {
        let dao = mkdao();

        assert_eq!(
            UpdateStatus::NotFound,
            dao.update(&se(0, b"foo", 0, 0), &se(0, b"foo", 0, 0))
                .unwrap()
        );
    }

    #[test]
    fn update_mismatch() {
        let dao = mkdao();

        assert!(dao.create(&se(0, b"foo", 0, 0)).unwrap().is_some());
        assert_eq!(
            UpdateStatus::NotMatched,
            dao.update(&se(0, b"foo", 1, 0), &se(0, b"foo", 2, 0))
                .unwrap()
        );

        let listed = list(&dao, true, 0);
        assert_eq!(1, listed.len());
        assert_eq!(0, listed[0].mode);
    }

    #[test]
    fn update_success() {
        let dao = mkdao();

        let foo_id = dao.create(&se(0, b"foo", 0, 0)).unwrap().unwrap();
        // Extra files to make sure filtering happens
        assert!(dao.create(&se(foo_id, b"foo", 0, 32)).unwrap().is_some());
        assert!(dao.create(&se(0, b"bar", 0, 0)).unwrap().is_some());

        assert_eq!(
            UpdateStatus::Ok,
            dao.update(&se(0, b"foo", 0, 0), &se(0, b"foo", 1, 42))
                .unwrap()
        );
        let root_list = list(&dao, true, 0);
        assert_eq!(2, root_list.len());
        for r in root_list {
            if b"foo" == &*r.name {
                assert_eq!(1, r.mode);
                assert_eq!(42, r.mtime);
            } else {
                assert_eq!(b"bar", &*r.name);
                assert_eq!(0, r.mode);
                assert_eq!(0, r.mtime);
            }
        }

        let foo_list = list(&dao, true, foo_id);
        assert_eq!(1, foo_list.len());
        assert_eq!(0, foo_list[0].mode);
        assert_eq!(32, foo_list[0].mtime);
    }

    #[test]
    fn delete_nx() {
        let dao = mkdao();

        assert_eq!(
            DeleteStatus::NotFound,
            dao.delete(&se(0, b"foo", 0, 0)).unwrap()
        );
    }

    #[test]
    fn delete_not_matched() {
        let dao = mkdao();

        assert!(dao.create(&se(0, b"foo", 1, 0)).unwrap().is_some());
        assert_eq!(
            DeleteStatus::NotMatched,
            dao.delete(&se(0, b"foo", 2, 0)).unwrap()
        );
    }

    #[test]
    fn delete_not_empty_then_success() {
        let dao = mkdao();

        let foo_id = dao.create(&se(0, b"foo", 0, 0)).unwrap().unwrap();
        assert!(dao.create(&se(foo_id, b"foo", 1, 0)).unwrap().is_some());

        assert_eq!(
            DeleteStatus::DirNotEmpty,
            dao.delete(&se(0, b"foo", 0, 0)).unwrap()
        );
        assert_eq!(
            DeleteStatus::Ok,
            dao.delete(&se(foo_id, b"foo", 1, 0)).unwrap()
        );
        assert_eq!(DeleteStatus::Ok, dao.delete(&se(0, b"foo", 0, 0)).unwrap());
    }

    #[test]
    fn rename_source_nx() {
        let dao = mkdao();

        assert_eq!(
            RenameStatus::SourceNotFound,
            dao.rename(0, b"foo", b"bar").unwrap()
        );
    }

    #[test]
    fn rename_dest_exists() {
        let dao = mkdao();

        assert!(dao.create(&se(0, b"foo", 0, 0)).unwrap().is_some());
        assert!(dao.create(&se(0, b"bar", 0, 0)).unwrap().is_some());
        assert_eq!(
            RenameStatus::DestExists,
            dao.rename(0, b"foo", b"bar").unwrap()
        );
    }

    #[test]
    fn rename_success() {
        let dao = mkdao();

        let foo_id = dao.create(&se(0, b"foo", 0, 0)).unwrap().unwrap();
        // Create child and sibling to ensure filtering is applied correctly
        assert!(dao.create(&se(foo_id, b"foo", 0, 0)).unwrap().is_some());
        assert!(dao.create(&se(0, b"quux", 0, 0)).unwrap().is_some());
        assert_eq!(RenameStatus::Ok, dao.rename(0, b"foo", b"bar").unwrap());

        let l = list(&dao, true, 0);
        assert_eq!(2, l.len());
        for e in l {
            if e.id == foo_id {
                assert_eq!(b"bar", &*e.name);
            } else {
                assert_eq!(b"quux", &*e.name);
            }
        }

        let l = list(&dao, true, foo_id);
        assert_eq!(1, l.len());
        assert_eq!(b"foo", &*l[0].name);
    }

    #[test]
    fn condemnation() {
        let dao = mkdao();

        assert!(dao.create(&se(0, b"foo", 0, 0)).unwrap().is_some());
        assert!(dao.create(&se(0, b"bar", 0, 0)).unwrap().is_some());
        assert_eq!(2, list(&dao, true, 0).len());
        dao.condemn(0, b"foo").unwrap();

        // Condmnation has no effect if condemn disabled on list
        let l = list(&dao, false, 0);
        assert_eq!(2, l.len());

        // List with condemn deletes condemned names
        let l = list(&dao, true, 0);
        assert_eq!(1, l.len());
        assert_eq!(b"bar", &*l[0].name);

        // List with condemn clears condemnation
        assert!(dao.create(&se(0, b"foo", 0, 0)).unwrap().is_some());
        assert_eq!(2, list(&dao, true, 0).len());
    }

    #[test]
    fn condemned_dir_removed_recursively() {
        let dao = mkdao();

        dao.condemn(0, b"foo").unwrap();

        assert!(dao.create(&se(0, b"foo", 0, 0)).unwrap().is_some());
        let foo_id = list(&dao, false, 0)[0].id;
        assert!(dao.create(&se(foo_id, b"bar", 0, 0)).unwrap().is_some());
        let bar_id = list(&dao, false, foo_id)[0].id;
        assert!(dao.create(&se(bar_id, b"quux", 0, 0)).unwrap().is_some());

        assert!(list(&dao, true, 0).is_empty());
        assert!(!dao.exists(foo_id, b"bar").unwrap());
        assert!(!dao.exists(bar_id, b"quux").unwrap());
    }

    #[test]
    fn uncondemn_clears_condemnation() {
        let dao = mkdao();

        assert!(dao.create(&se(0, b"foo", 0, 0)).unwrap().is_some());
        assert!(dao.create(&se(0, b"bar", 0, 0)).unwrap().is_some());
        dao.condemn(0, b"foo").unwrap();
        dao.condemn(0, b"bar").unwrap();
        dao.uncondemn(0, b"foo").unwrap();

        let l = list(&dao, true, 0);
        assert_eq!(1, l.len());
        assert_eq!(b"foo", &*l[0].name);
    }
}
