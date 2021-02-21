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

//! Miscelaneous utilities for working with SQLite.

use std::ffi::{CString, NulError, OsStr};
use std::fs;
use std::io::{self, Write};
use std::ops::{Deref, DerefMut};
// Because we need to be able to represent an `OsStr` as bytes in the database.
// This does not actually depend on anything POSIX-specific; if you're porting
// to non-POSIX, it might be worth trying to simply get the internal WTF-8
// encoding out and store that rather than using different behaviours for
// different platforms.
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::result::Result as StdResult;

use sqlite;
use sqlite::*;

/// Executes `f` within a transaction.
///
/// `f` must be able to access the same connection reference via capture (etc).
///
/// If `f` returns `Ok`, the transaction is committed. Otherwise, the
/// transaction is rolled back.
///
/// The return value from `f` is always returned, unless starting or committing
/// the transaction fails.
pub fn tx_gen<T, E, F: FnOnce() -> StdResult<T, E>>(
    cxn: &Connection,
    f: F,
) -> StdResult<T, E>
where
    E: From<sqlite::Error>,
{
    cxn.execute("BEGIN TRANSACTION")?;
    match f() {
        Ok(v) => {
            cxn.execute("COMMIT")?;
            Ok(v)
        }
        Err(e) => {
            // Silently drop errors from ROLLBACK, since it will fail if
            // the error above caused SQLite to roll back automatically.
            drop(cxn.execute("ROLLBACK"));
            Err(e.into())
        }
    }
}

/// A less general version of `tx_gen` which only deals in `sqlite::Error`s,
/// which makes it work better with type inferrence.
pub fn tx<T, F: FnOnce() -> sqlite::Result<T>>(
    cxn: &Connection,
    f: F,
) -> sqlite::Result<T> {
    tx_gen(cxn, f)
}

pub trait StatementEx: Sized {
    type Bound: StatementEx;

    /// Like `Statement::bind`, but returns a self-like value to allow chaining
    /// onto other methods in this trait.
    fn binding<T: Bindable>(self, i: usize, value: T) -> Self::Bound;

    /// Executes this statement (if possible), discarding all rows until `Done`
    /// is returned or an error is returned.
    fn run(self) -> Result<()>;

    /// Fetches the first result row. If there is one, `f` is called with the
    /// underlying statement to convert the value, and `Ok(Some(_))` is
    /// returned if `f` succeeds. `Ok(None)` is returned if there are no result
    /// rows.
    fn first<R, F: FnOnce(&Statement) -> Result<R>>(
        self,
        f: F,
    ) -> Result<Option<R>>;

    /// Returns whether there are any rows in the result set.
    fn exists(self) -> Result<bool> {
        self.first(|_| Ok(())).map(|o| o.is_some())
    }
}

impl<'l> StatementEx for Statement<'l> {
    type Bound = Result<Statement<'l>>;

    fn binding<T: Bindable>(
        mut self,
        i: usize,
        value: T,
    ) -> Result<Statement<'l>> {
        self.bind(i, value).map(|_| self)
    }

    fn run(mut self) -> Result<()> {
        while State::Done != self.next()? {}
        Ok(())
    }

    fn first<R, F: FnOnce(&Statement) -> Result<R>>(
        mut self,
        f: F,
    ) -> Result<Option<R>> {
        if State::Done == self.next()? {
            Ok(None)
        } else {
            f(&self).map(Some)
        }
    }
}

impl<'l> StatementEx for Result<Statement<'l>> {
    type Bound = Self;

    fn binding<T: Bindable>(self, i: usize, value: T) -> Self {
        self.and_then(|r| r.binding(i, value))
    }

    fn run(self) -> Result<()> {
        self.and_then(|r| r.run())
    }

    fn first<R, F: FnOnce(&Statement) -> Result<R>>(
        self,
        f: F,
    ) -> Result<Option<R>> {
        self.and_then(|r| r.first(f))
    }
}

/// Provides a conversion from `&[u8]` to `&OsStr` which checks for embedded
/// NUL bytes (which aren't actually legal in any syscall, but nonetheless
/// allowed in generic `OsStr`s).
pub trait AsNStr {
    fn as_nstr(&self) -> StdResult<&OsStr, NulError>;
}

impl AsNStr for [u8] {
    fn as_nstr(&self) -> StdResult<&OsStr, NulError> {
        if self.contains(&0u8) {
            // There's no public API for constructing a `NulError`, so let
            // `CString` do it for us.
            Err(CString::new(self).err().unwrap())
        } else {
            Ok(OsStr::from_bytes(self))
        }
    }
}

/// Extension for `OsStr` which converts it into a byte sequence which
/// `as_nstr()` can reverse.
pub trait AsNBytes {
    fn as_nbytes(&self) -> &[u8];
}

impl AsNBytes for OsStr {
    fn as_nbytes(&self) -> &[u8] {
        self.as_bytes()
    }
}

pub struct SendConnection(pub sqlite::Connection);
impl Deref for SendConnection {
    type Target = sqlite::Connection;

    fn deref(&self) -> &sqlite::Connection {
        &self.0
    }
}
impl DerefMut for SendConnection {
    fn deref_mut(&mut self) -> &mut sqlite::Connection {
        &mut self.0
    }
}

// `sqlite::Connection` cannot make itself `Send` because the optional callback
// it contains might not be `Send`. We do not use that feature, and SQLite
// itself is prepared for cross-thread requests, so it is safe to be `Send`.
unsafe impl Send for SendConnection {}

/// Wrapper for `Connection` which runs the database in non-synchronous mode.
///
/// The schema must create a table named `db_dirty` which contains a `dirty`
/// column and which is initially empty.
///
/// When the database is opened, the schema is executed, and then a test is
/// made to see whether the `db_dirty` table is non-empty. If it is, or any
/// error occurs in this process, `PRAGMA integrity_check(1)` is run. If that
/// fails or returns any errors, a warning is printed, the database file is
/// **deleted**, and then a new database is opened and the schema executed. If
/// the second try fails, an error is returned.
///
/// When the connection is successfully set up, the following statements are
/// run:
///
/// ```sql
/// PRAGMA synchronous = FULL;
/// INSERT INTO `db_dirty` (`dirty`) VALUES (1);
/// PRAGMA synchronous = OFF;
/// ```
///
/// Finally, when this value is dropped, the following is run, and any errors
/// are ignored:
///
/// ```sql
/// PRAGMA synchronous = FULL;
/// DELETE FROM `db_dirty`;
/// ```
///
/// Additionally, this type also has the same properties as `SendConnection`.
pub struct VolatileConnection(pub sqlite::Connection);
unsafe impl Send for VolatileConnection {}

impl Deref for VolatileConnection {
    type Target = sqlite::Connection;

    fn deref(&self) -> &sqlite::Connection {
        &self.0
    }
}
impl DerefMut for VolatileConnection {
    fn deref_mut(&mut self) -> &mut sqlite::Connection {
        &mut self.0
    }
}

macro_rules! eprintln {
    ($($e:expr),*) => { { let _ = writeln!(io::stderr(), $($e),*); } }
}

impl VolatileConnection {
    /// Sets up a new `VolatileConnection`, using the given path and
    /// database schema.
    ///
    /// `bad_stuff` specifies a message to display to describe what the
    /// consequences of nuking the database are.
    pub fn new<P: AsRef<Path>>(
        path: P,
        schema: &str,
        bad_stuff: &str,
    ) -> sqlite::Result<Self> {
        fn open_with_schema(
            path: &Path,
            schema: &str,
        ) -> sqlite::Result<sqlite::Connection> {
            let cxn = sqlite::Connection::open(path)?;
            cxn.execute(schema)?;
            Ok(cxn)
        }

        fn open_first_try(
            path: &Path,
            schema: &str,
        ) -> sqlite::Result<sqlite::Connection> {
            let cxn = open_with_schema(path, schema)?;
            if cxn.prepare("SELECT 1 FROM `db_dirty`").exists()? {
                eprintln!(
                    "Database '{}' not closed cleanly, performing \
                           integrity check...",
                    path.display()
                );
                let error: String = cxn
                    .prepare("PRAGMA integrity_check(1)")
                    .first(|s| s.read(0))?
                    .unwrap_or_else(|| "(not ok)".to_owned());
                if "ok" != &error {
                    return Err(sqlite::Error {
                        code: None,
                        message: Some(error),
                    });
                }
                if cxn.prepare("PRAGMA foreign_key_check").exists()? {
                    return Err(sqlite::Error {
                        code: None,
                        message: Some(
                            "foreign key constraint violated".to_owned(),
                        ),
                    });
                }
                eprintln!("Integrity check passed");
            }
            Ok(cxn)
        }

        let path = path.as_ref();
        let cxn = open_first_try(path, schema).or_else(|e| {
            eprintln!(
                "Database '{}' is corrupt: {}\n\
                       Deleting this file and creating a new, \
                       empty database.\n\
                       {}",
                path.display(),
                e,
                bad_stuff
            );
            let _ = fs::remove_file(path);
            open_with_schema(path, schema)
        })?;

        cxn.prepare("PRAGMA synchronous = FULL").run()?;
        cxn.prepare("INSERT INTO `db_dirty` (`dirty`) VALUES (1)")
            .run()?;
        cxn.prepare("PRAGMA synchronous = OFF").run()?;
        Ok(VolatileConnection(cxn))
    }
}

impl Drop for VolatileConnection {
    fn drop(&mut self) {
        self.0
            .prepare("PRAGMA synchronous = FULL")
            .run()
            .and_then(|_| self.0.prepare("DELETE FROM `db_dirty`").run())
            .unwrap_or_else(|e| {
                eprintln!("Failed to clean database up: {}", e);
            });
    }
}
