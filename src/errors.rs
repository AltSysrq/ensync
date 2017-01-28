//-
// Copyright (c) 2016, Jason Lingle
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

use std::ffi;
use std::io;

use fourleaf;
use sqlite;
use tempfile;

use defs::{DisplayHash, HashId};
use server;

error_chain! {
    types {
        Error, ErrorKind, ResultExt, Result;
    }

    links { }

    foreign_links {
        Io(io::Error);
        Sqlite(sqlite::Error);
        NullInString(ffi::NulError);
        FourleafDeser(fourleaf::de::Error);
        FourleafSer(fourleaf::stream::Error);
    }

    errors {
        ExpectationNotMatched {
            description("File not in expected state")
            display("File not in expected state")
        }
        NotFound {
            description("File not found")
            display("File not found")
        }
        CreateExists {
            description("File already exists")
            display("File already exists")
        }
        RenameDestExists {
            description("Rename destination already exists")
            display("Rename destination already exists")
        }
        DirNotEmpty {
            description("Directory not empty")
            display("Directory not empty")
        }
        NotADirectory {
            description("Not a directory")
            display("Not a directory")
        }
        InvalidAncestorFileType(t: i64) {
            description("Bad file type in database")
            display("Bad file type in database: {}", t)
        }
        InvalidHash {
            description("Invalid hash value in database")
            display("Invalid hash value in database")
        }
        HmacMismatch(context: &'static str, expected: HashId, actual: HashId) {
            description("Block HMAC does not match content")
            display("Block HMAC does not match content (expected {}, got {})",
                    DisplayHash(*expected), DisplayHash(*actual))
        }
        AllSuffixesInUse {
            description("Shunt failed: All file suffixes in use")
            display("Shunt failed: All file suffixes in use")
        }
        MissingXfer {
            description("BUG: No xfer provided for file transfer")
            display("BUG: No xfer provided for file transfer")
        }
        RmdirRoot {
            description("Attempt to remove root directory")
            display("Attempt to remove root directory")
        }
        ChdirXDev {
            description("Attempt to cross filesystem boundary")
            display("Attempt to cross filesystem boundary")
        }
        PrivateXDev {
            description("Private directory is on different filesystem from \
                         sync root")
            display("Private directory is on different filesystem from \
                     sync root")
        }
        BadFilename(name: ffi::OsString) {
            description("Illegal file name")
            display("Illegal file name: {:?}", name)
        }
        NoSuchTransaction(tx: u64) {
            description("No such transaction")
            display("No such transaction: {}", tx)
        }
        TransactionAlreadyInUse(tx: u64) {
            description("Transaction already in use")
            display("Transaction {} already in use", tx)
        }
        DirectoryTooLarge {
            description("Directory too large")
            display("Directory too large")
        }
        InvalidRefVector {
            description("Invalid object reference vector")
            display("Invalid object reference vector")
        }
        InvalidObjectId {
            description("Invalid object id")
            display("Invalid object id")
        }
        InvalidServerDirEntry {
            description("Invalid server directory entry")
            display("Invalid server directory entry")
        }
        DanglingServerDirectoryRef {
            description("Dangling server directory reference")
            display("Dangling server directory reference")
        }
        ServerConnectionClosed {
            description("Server connection closed")
            display("Server connection closed")
        }
        ServerError(err: String) {
            description("Server error")
            display("Server error: {}", err)
        }
        UnexpectedServerResponse(response: server::rpc::Response) {
            description("Unexpected server response")
            display("Unexpected server response: {:?}", response)
        }
        DirectoryVersionRecessed(dir: ffi::OsString,
                                 latest_ver: u64, latest_len: u64,
                                 actual_ver: u64, actual_len: u64) {
            description("Server directory version recessed")
            display("Server directory version recessed from \
                     ({}, {}) to ({}, {}) for '{}'",
                    latest_ver, latest_len, actual_ver, actual_len,
                    dir.to_string_lossy())
        }
        DirectoryEmbeddedIdMismatch(dir: ffi::OsString) {
            description("Server directory content does not \
                         correspond to directory id")
            display("Server directory content does not \
                     correspond to directory id of '{}'",
                    dir.to_string_lossy())
        }
        DirectoryEmbeddedVerMismatch(dir: ffi::OsString) {
            description("Server directory content does not \
                         correspond to directory version")
            display("Server directory content does not \
                     correspond to directory version of '{}'",
                    dir.to_string_lossy())
        }
        UnsupportedServerDirectoryFormat(dir: ffi::OsString,
                                         supported: u32,
                                         actual: u32) {
            description("Server directory uses unsupported format")
            display("Server directory uses unsupported format \
                     version ({}; this version of ensync only supports \
                     up to {}) for '{}'", actual, supported,
                    dir.to_string_lossy())
        }
        ServerDirectoryCorrupt(dir: ffi::OsString, message: String) {
            description("Server directory corrupt")
            display("Server directory '{}' corrupt: {}",
                    dir.to_string_lossy(), message)
        }
        TooManyTxRetries {
            description("Transaction failed too many times")
            display("Transaction failed too many times")
        }
        DirectoryMissing {
            description("Directory missing")
            display("Directory missing")
        }
        SynthConflict {
            description("Conflict creating on-demand directory")
            display("Conflict creating on-demand directory")
        }
        ServerContentDeleted {
            description("Content deleted on server")
            display("Content deleted on server")
        }
        ServerContentUpdated {
            description("Content changed on server")
            display("Content changed on server")
        }

        // Errors related to setup/usage
        KdfListAlreadyExists {
            description("Key store already initialised \
                         (use `add-key` if you want to add more keys)")
        }
        KdfListNotExists {
            description("Key store not yet initialised \
                         (use `init` to do that)")
        }
        KeyNotInKdfList(name: String) {
            description("Key not found in key store")
            display("Key {} not found in key store", name)
        }
        PassphraseNotInKdfList {
            description("Passphrase not found in key store")
        }
        WouldRemoveLastKdfEntry {
            description("There is only one key in the key store, \
                         it cannot be removed")
        }
        AnonChangeKeyButMultipleKdfEntries {
            description("`change-key` requires the name of the key \
                         to change because there are multiple keys in \
                         the key store")
        }
        ChangeKeyWithPassphraseMismatch {
            description("The passphrase matches a key in the key store, \
                         but not the one named on the command-line; \
                         use `--force` if you really are sure you want \
                         to change the named key")
        }
        KeyNameAlreadyInUse(name: String) {
            description("Key name already in use")
            display("Key name '{}' already in use \
                     (use `change-key` if you want to edit it)", name)
        }
    }
}

impl From<tempfile::PersistError> for Error {
    fn from(e: tempfile::PersistError) -> Self {
        e.error.into()
    }
}
