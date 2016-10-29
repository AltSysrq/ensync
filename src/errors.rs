use std::ffi;
use std::io;

use serde_cbor;
use sqlite;
use tempfile;

use serde_types;

error_chain! {
    types {
        Error, ErrorKind, ChainErr, Result;
    }

    links { }

    foreign_links {
        io::Error, Io;
        sqlite::Error, Sqlite;
        ffi::NulError, NullInstring;
        serde_cbor::Error, SerdeCbor;
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
        HmacMismatch {
            description("HMAC does not match content")
            display("HMAC does not match content")
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
            display("{}", err)
        }
        UnexpectedServerResponse(response: serde_types::rpc::Response) {
            description("Unexpected server response")
            display("Unexpected server response: {:?}", response)
        }
    }
}

impl From<tempfile::PersistError> for Error {
    fn from(e: tempfile::PersistError) -> Self {
        e.error.into()
    }
}
