//-
// Copyright (c) 2016, Jason Lingle
//
// Permission to  use, copy,  modify, and/or distribute  this software  for any
// purpose  with or  without fee  is hereby  granted, provided  that the  above
// copyright notice and this permission notice appear in all copies.
//
// THE SOFTWARE  IS PROVIDED "AS  IS" AND  THE AUTHOR DISCLAIMS  ALL WARRANTIES
// WITH  REGARD   TO  THIS  SOFTWARE   INCLUDING  ALL  IMPLIED   WARRANTIES  OF
// MERCHANTABILITY AND FITNESS. IN NO EVENT  SHALL THE AUTHOR BE LIABLE FOR ANY
// SPECIAL,  DIRECT,   INDIRECT,  OR  CONSEQUENTIAL  DAMAGES   OR  ANY  DAMAGES
// WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS, WHETHER IN AN ACTION
// OF  CONTRACT, NEGLIGENCE  OR OTHER  TORTIOUS ACTION,  ARISING OUT  OF OR  IN
// CONNECTION WITH THE USE OR PERFORMANCE OF THIS SOFTWARE.

use std::ffi::{CStr,CString};

/// Type for content hashes of regular files and for blob identifiers on the
/// server.
///
/// In practise, this is a 256-bit SHA-3 sum.
pub type HashId = [u8;32];
/// The sentinal hash value indicating an uncomputed hash.
///
/// One does not compare hashes against this, since the hashes on files can be
/// out-of-date anyway and must be computed when the file is uploaded in any
/// case.
pub const UNKNOWN_HASH : HashId = [0;32];

// These were originally defined to `mode_t`, `off_t`, `time_t`, and `ino_t`
// when we planned to use the POSIX API directly.
pub type FileMode = u32;
pub type FileSize = u64;
pub type FileTime = i64;
pub type FileInode = u64;

/// Shallow data about a file in the sync process, excluding its name.
#[derive(Clone,Debug,PartialEq,Eq)]
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
    Symlink(CString),
    /// Any other type of non-regular file.
    Special,
}

impl FileData {
    /// Returns whether both `self` and `other` are regular files and `self`'s
    /// modification time is greater than `other`'s.
    pub fn newer_than(&self, other: &Self) -> bool {
        match (self, other) {
            (&FileData::Regular(_, _, tself, _),
             &FileData::Regular(_, _, tother, _)) =>
                tself > tother,
            _ => false,
        }
    }

    /// Returns whether this file object and another one represent the same
    /// content.
    ///
    /// This is slightly less strict than a full equality test, ignoring some
    /// of the fields for regular fiels.
    pub fn matches(&self, that: &FileData) -> bool {
        use self::FileData::*;

        match (self, that) {
            (&Directory(m1), &Directory(m2)) => m1 == m2,
            (&Regular(m1, _, _, ref h1), &Regular(m2, _, _, ref h2)) =>
                m1 == m2 && *h1 == *h2,
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
            (&Regular(_, _, _, ref h1), &Regular(_, _, _, ref h2)) =>
                *h1 == *h2,
            (&Symlink(ref t1), &Symlink(ref t2)) => *t1 == *t2,
            (&Special, &Special) => true,
            _ => false,
        }
    }
}

/// Convenience for passing a file name and data together.
#[derive(Clone,Debug,PartialEq,Eq)]
pub struct File<'a> (pub &'a CStr, pub &'a FileData);

pub fn is_dir(fd: Option<&FileData>) -> bool {
    match fd {
        Some(&FileData::Directory(_)) => true,
        _ => false,
    }
}

#[cfg(test)]
pub mod test_helpers {
    use std::ffi::CString;

    pub fn oss(s: &str) -> CString {
        CString::new(s).unwrap()
    }
}

#[cfg(test)]
mod test {
    use std::ffi::CString;

    use super::*;

    #[test]
    fn file_newer_than() {
        let older = FileData::Regular(0o777, 0, 42, [1;32]);
        let newer = FileData::Regular(0o666, 0, 56, [2;32]);

        assert!(newer.newer_than(&older));
        assert!(!older.newer_than(&newer));
        assert!(!FileData::Special.newer_than(&older));
        assert!(!newer.newer_than(&FileData::Special));
    }

    #[test]
    fn file_matches() {
        let f1 = FileData::Regular(0o777, 0, 42, [1;32]);
        let f2 = FileData::Regular(0o666, 0, 56, [1;32]);
        let f3 = FileData::Regular(0o777, 0, 42, [2;32]);
        let f4 = FileData::Regular(0o777, 0, 42, [1;32]);
        let d1 = FileData::Directory(0o777);
        let d2 = FileData::Directory(0o666);
        let s1 = FileData::Symlink(CString::new("foo").unwrap());
        let s2 = FileData::Symlink(CString::new("bar").unwrap());
        let s3 = FileData::Symlink(CString::new("foo").unwrap());
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
