//-
// Copyright (c) 2016, 2017, Jason Lingle
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
use std::fmt;

/// Type for content hashes of regular files and for blob identifiers on the
/// server.
///
/// In practise, this is a 256-bit SHA-3 sum.
pub type HashId = [u8; 32];
/// The sentinal hash value indicating an uncomputed hash.
///
/// One does not compare hashes against this, since the hashes on files can be
/// out-of-date anyway and must be computed when the file is uploaded in any
/// case.
pub const UNKNOWN_HASH: HashId = [0; 32];
/// The name of the directory which is a sibling to the configuration and which
/// is the root of Ensync's private data.
pub const PRIVATE_DIR_NAME: &'static str = "internal.ensync";
/// Prefix of invasive temporary files (i.e., those created implicitly by the
/// sync process).
pub const INVASIVE_TMP_PREFIX: &'static str = "ensync_tmp_";

/// Wraps a `HashId` to display it in hexadecimal format.
#[derive(Clone, Copy)]
pub struct DisplayHash(pub HashId);
impl fmt::Display for DisplayHash {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}\
                   {:02x}{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}\
                   {:02x}{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}\
                   {:02x}{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
            self.0[0],
            self.0[1],
            self.0[2],
            self.0[3],
            self.0[4],
            self.0[5],
            self.0[6],
            self.0[7],
            self.0[8],
            self.0[9],
            self.0[10],
            self.0[11],
            self.0[12],
            self.0[13],
            self.0[14],
            self.0[15],
            self.0[16],
            self.0[17],
            self.0[18],
            self.0[19],
            self.0[20],
            self.0[21],
            self.0[22],
            self.0[23],
            self.0[24],
            self.0[25],
            self.0[26],
            self.0[27],
            self.0[28],
            self.0[29],
            self.0[30],
            self.0[31]
        )
    }
}

// These were originally defined to `mode_t`, `off_t`, `time_t`, and `ino_t`
// when we planned to use the POSIX API directly.
pub type FileMode = u32;
pub type FileSize = u64;
pub type FileTime = i64;
pub type FileInode = u64;

/// Shallow data about a file in the sync process, excluding its name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FileData {
    /// A directory. The only immediate data is its mode. In a file stream, the
    /// receiver must either push the new directory or request it to be
    /// discarded.
    Directory(FileMode),
    /// A regular file. Data is mode, size in bytes, last modified, content
    /// hash. Note that the content hash may be incorrect, and always will be
    /// for files freshly streamed off the client filesystem.
    Regular(FileMode, FileSize, FileTime, HashId),
    /// A symbolic link. The only data is its actual content.
    Symlink(OsString),
    /// Any other type of non-regular file.
    Special,
}

impl FileData {
    /// If both `self` and `other` have a `FileMode`, set `self`'s mode to
    /// `other`'s.
    pub fn transrich_unix_mode(&mut self, other: &FileData) {
        match *self {
            FileData::Directory(ref mut dst)
            | FileData::Regular(ref mut dst, _, _, _) => match *other {
                FileData::Directory(src) | FileData::Regular(src, _, _, _) => {
                    *dst = src
                }
                _ => (),
            },
            _ => (),
        }
    }

    /// Returns whether this `FileData` is a directory.
    pub fn is_dir(&self) -> bool {
        match *self {
            FileData::Directory(_) => true,
            _ => false,
        }
    }

    /// Returns whether both `self` and `other` are regular files and `self`'s
    /// modification time is greater than `other`'s.
    pub fn newer_than(&self, other: &Self) -> bool {
        match (self, other) {
            (
                &FileData::Regular(_, _, tself, _),
                &FileData::Regular(_, _, tother, _),
            ) => tself > tother,
            _ => false,
        }
    }

    /// Returns whether this file object and another one represent the same
    /// content.
    ///
    /// This is slightly less strict than a full equality test, ignoring some
    /// of the fields for regular files.
    pub fn matches(&self, that: &FileData) -> bool {
        use self::FileData::*;

        match (self, that) {
            (&Directory(m1), &Directory(m2)) => m1 == m2,
            (&Regular(m1, _, t1, ref h1), &Regular(m2, _, t2, ref h2)) => {
                m1 == m2 && t1 == t2 && *h1 == *h2
            }
            (&Symlink(ref t1), &Symlink(ref t2)) => *t1 == *t2,
            (&Special, &Special) => true,
            _ => false,
        }
    }

    /// Returns whether non-metadata about this file and another one match.
    pub fn matches_data(&self, that: &FileData) -> bool {
        use self::FileData::*;

        match (self, that) {
            (&Directory(m1), &Directory(m2)) => m1 == m2,
            (&Regular(m1, _, _, ref h1), &Regular(m2, _, _, ref h2)) => {
                m1 == m2 && *h1 == *h2
            }
            (&Symlink(ref t1), &Symlink(ref t2)) => *t1 == *t2,
            (&Special, &Special) => true,
            _ => false,
        }
    }

    /// Returns whether this file object and another one have the same content
    /// except for file mode.
    pub fn matches_content(&self, that: &FileData) -> bool {
        use self::FileData::*;

        match (self, that) {
            (&Directory(_), &Directory(_)) => true,
            (&Regular(_, _, _, ref h1), &Regular(_, _, _, ref h2)) => {
                *h1 == *h2
            }
            (&Symlink(ref t1), &Symlink(ref t2)) => *t1 == *t2,
            (&Special, &Special) => true,
            _ => false,
        }
    }
}

/// Convenience for passing a file name and data together.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct File<'a>(pub &'a OsStr, pub &'a FileData);

pub fn is_dir(fd: Option<&FileData>) -> bool {
    match fd {
        Some(&FileData::Directory(_)) => true,
        _ => false,
    }
}

#[cfg(test)]
pub mod test_helpers {
    use std::ffi::{OsStr, OsString};

    pub fn oss(s: &str) -> OsString {
        OsStr::new(s).to_owned()
    }
}

#[cfg(test)]
mod test {
    use super::test_helpers::*;
    use super::*;

    #[test]
    fn file_newer_than() {
        let older = FileData::Regular(0o777, 0, 42, [1; 32]);
        let newer = FileData::Regular(0o666, 0, 56, [2; 32]);

        assert!(newer.newer_than(&older));
        assert!(!older.newer_than(&newer));
        assert!(!FileData::Special.newer_than(&older));
        assert!(!newer.newer_than(&FileData::Special));
    }

    #[test]
    fn file_matches() {
        let f1 = FileData::Regular(0o777, 0, 42, [1; 32]);
        let f2 = FileData::Regular(0o666, 0, 56, [1; 32]);
        let f3 = FileData::Regular(0o777, 0, 42, [2; 32]);
        let f4 = FileData::Regular(0o777, 0, 42, [1; 32]);
        let d1 = FileData::Directory(0o777);
        let d2 = FileData::Directory(0o666);
        let s1 = FileData::Symlink(oss("foo"));
        let s2 = FileData::Symlink(oss("bar"));
        let s3 = FileData::Symlink(oss("foo"));
        let special = FileData::Special;

        assert!(f1.matches(&f1));
        assert!(f1.matches(&f4));
        assert!(!f1.matches(&f2));
        assert!(!f1.matches(&f3));
        assert!(!f1.matches(&d1));
        assert!(!f1.matches(&s1));
        assert!(!f1.matches(&special));

        assert!(d1.matches(&d1));
        assert!(!d1.matches(&d2));
        assert!(!d1.matches(&f1));

        assert!(s1.matches(&s1));
        assert!(s1.matches(&s3));
        assert!(!s1.matches(&s2));
        assert!(!s1.matches(&special));

        assert!(special.matches(&special));
        assert!(!special.matches(&f1));

        assert!(f1.matches_content(&f1));
        assert!(f1.matches_content(&f4));
        assert!(f1.matches_content(&f2));
        assert!(!f1.matches_content(&f3));
        assert!(!f1.matches_content(&d1));
        assert!(!f1.matches_content(&s1));
        assert!(!f1.matches_content(&special));

        assert!(d1.matches_content(&d1));
        assert!(d1.matches_content(&d2));
        assert!(!d1.matches_content(&f1));

        assert!(s1.matches_content(&s1));
        assert!(s1.matches_content(&s3));
        assert!(!s1.matches_content(&s2));
        assert!(!s1.matches_content(&special));

        assert!(special.matches_content(&special));
        assert!(!special.matches_content(&f1));
    }
}
