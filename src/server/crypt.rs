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

//! All the encryption stuff in Ensync.
//!
//! # Choice of encryption (Symmetric vs GPG)
//!
//! Ensync's original design was to use GPG to encrypt files. This mainly has
//! the advantage of being a very known quantity; using asymmetric encryption
//! also allows for interesting possibilities like allowing a semi-trusted
//! client to create files _but not read them back_. Ultimately, this design
//! was dropped in favour of using simple symmetric encryption, for a number of
//! reasons:
//!
//! - Clients always need to be able to re-read the directories they write.
//! Permitting the "write-but-not-read" model would thus require using
//! different keys for directories and files.
//!
//! - Using GPG for encryption would result in a proliferation of the master
//! key(s). There would be no easy way to change what keys had access to the
//! server store.
//!
//! - GPG is pretty slow at handling very large numbers of very small items.
//!
//! # Key derivation
//!
//! For this discussion, we'll consider all forms of seed input to the key
//! derivation system to be the "passphrase". Also look at `KdfList` and
//! `KdfEntry`.
//!
//! To protect the user's files, we encrypt them using a passphrase as a key in
//! some way. A trivial approach would be to simply hash the passphrase and use
//! that as the symmetric key. However, this is very vulnerable to brute-force
//! (especially dictionary) attacks and does not permit any form of rekeying.
//!
//! First, we use Scrypt to derive a secondary key. This requires a random
//! salt, as well as parameters that may change in the future; we need to store
//! these somewhere so that later invocations can see what parameters was used
//! to reproduce the key derivation. We thus store in cleartext these
//! parameters for each key.
//!
//! Where to store this? The server protocol already supports a way to store an
//! arbitrary blob with atomic updates -- directories. We thus [ab]use the
//! directory whose id is all zeroes to store this data.
//!
//! We also want to be able to tell whether a passphrase is correct with this
//! data. Even if we only allowed one passphrase and could simply plough ahead
//! with whatever key was derived, we would still want to be able to do this as
//! proceeding with an incorrect key would result in scary "data corrupt"
//! errors if the user mistyped their passphrase. To do this, we also store the
//! SHA-3 of the derived key (*not* the passphrase). Since the derived key
//! effectively has 256 bits of entropy, attempting to reverse this hash is
//! infeasible, and any attacks that reduce that entropy would almost certainly
//! still make it more feasible to break the key derivation function instead.
//!
//! Supporting multiple passphrases is important. There are some use-cases for
//! using multiple in tandem; but more importantly, this support also provides
//! a way to _change_ the passphrase without needing to rebuild the entire data
//! store.
//!
//! We also want to support multiple internal keys, which can be used to
//! isolate parts of the system. Each internal key a name and a randomly
//! generated 32-byte value. Each passphrase can be bound to any number of
//! internal keys, though in practise all passphrases must at least be bound to
//! the `everyone` internal key.
//!
//! Obviously, there needs to be a way to go from a secondary key to each
//! associated internal key. Thus each passphrase entry stores a 32-byte value
//! which is the XOR of the internal key with the HMAC of the internal key name
//! and the secondary key.
//!
//! To put it all together:
//!
//! - We start off with a passphrase and the key store we fetched.
//!
//! - For each key in the key store:
//!
//! - Apply Scrypt (or whatever algorithms we add in the future) to the
//! passphrase with the stored parameters. Ignore entries not supported.
//!
//! - Hash the derived key. If it does not match the stored hash, move to the
//! next entry.
//!
//! - For each internal key bound to the entry, take the HMAC of the internal
//! key name and the derived key, then XOR it with the internal key diff.
//!
//! Note that the keys here are actually 256 bits wide, twice the size of an
//! AES key. We thus actually have *two* independent internal keys. The first
//! 128 bits are used for encryption operations on directories. The second 128
//! bits is used as the HMAC secret.
//!
//! # Groups
//!
//! Internal keys are exposed in the CLI as a more abstract "group" concept.
//! Each group is essentially an internal key, though there are additional
//! semantics ascribed below.
//!
//! By default, there is an `everyone` group which all passphrases are a member
//! of, and a `root` group which one passphrase must be a member of.
//!
//! Every directory has a read group and a write group. For the root directory,
//! these are `everyone` and `root`, respectively. The read group determines
//! which internal key is used to encrypt directory contents. As discussed
//! below, this also by proxy determines which objects can be read. The write
//! group is used by the server to restrict writes to the directory.
//!
//! Groups are switched when entering a directory containing specific syntax in
//! its *name* (as defined by `dir_config.rs`). This may seem like a hacky
//! choice, since it's loud, sticky, and infectious. This is in fact exactly
//! why it is done this way, rather than as some kind of metadata. It means,
//! for example, that an attacker with sufficient access to edit a parent
//! directory but insufficient to read a child cannot replace the child
//! directory with one without protection, as the only way to do so would be to
//! change its name, which a potential victim would handle at worst by deleting
//! its files and at best by simply recreating the child with the same name
//! (and thus the same protection).
//!
//! # Encrypting objects
//!
//! Since objects are immutable, opaque blobs, their handling is for the most
//! part reasonably simple. The main consideration is how the encryption should
//! play with access control. What we need to guarantee is:
//!
//! - If you write an object to the server, you can be sure that all other
//! clients that do the same thing would use the same key.
//!
//! - If you can read a directory referencing an object, you can reference that
//! object.
//!
//! The solution is to derive the from the object content and the `everyone`
//! internal key. Specifically:
//!
//! - Object HMACs always use the HMAC secret of the `everyone` internal key.
//!
//! - Use the first 16 bytes of the object id as the AES key, and the second 16
//! bytes as the IV.
//!
//! - Encrypt the object data in CBC mode using the that key and IV.
//!
//! - Pass object ids through SHA-3 before passing them to the server so that
//! the server cannot derive the encryption keys.
//!
//! For writers, this system is easy. For readers, there's the conundrum that
//! they don't know the object content, and so cannot determine the key to
//! decrypt the content. However, the HMAC is actually stored in the directory
//! already as the client-side object id.
//!
//! Note that this does mean that the client ends up with a bunch of encryption
//! keys strewn about in cleartext. This is not too much of an issue, though,
//! as the cleartext of the objects they protect is also available on the same
//! file system, and the latter are more likely to be accessible to attackers
//! than the contents of the ensync private directory.
//!
//! Objects are padded to the block size with PKCS.
//!
//! # Directory Versions
//!
//! In order to detect reversion attacks, the opaque directory versions are
//! actually encrypted incrementing integers. This is performed as follows:
//!
//! - Encode the version as a little-endian 64-bit integer.
//!
//! - Pad it with 24 zero bytes (to make it the size of a `HashId`).
//!
//! - Encrypt it with AES in CBC mode using the `everyone` directory key and an
//! IV equal to the two halves of the directory id XORed with each other.
//!
//! Encrypting the integers this way obfuscates how often a directory is
//! usually updated. It does not alone prevent tampering since an attacker
//! could simply choose not to change the version. Because of this, we also
//! store the version in the directory. (This aspect is documented in the
//! directory format docs.)
//!
//! Using an IV based on the directory id means that every directory has a
//! different progression of encrypted version numbers. This prevents an
//! attacker contolling the server from sending a different directory's
//! contents by making an educated guess as to what might have a greater
//! version number than what clients have seen before, which could be used to
//! create an infinite directory tree as part of a padding oracle attack, etc.
//!
//! Versions need to be encrypted with the `everyone` key because there is not
//! always enough context to know what key should actually be used.
//!
//! Every directory version additionally has a secret value which is used to
//! protect writes. This is simply the HMAC of the encrypted id and the HMAC
//! secret of the write group. Note that this protection mechanism does not
//! have inherent cryptographic value, in that anyone with filesystem access
//! can trivially bypass the check, but it prevents an attacker with access to
//! only an ensync server socket from corrupting directories.
//!
//! # Directory Contents
//!
//! The content of a directory is prefixed with 32 bytes, encrypted in CBC mode
//! with the directory key of the read group, using an IV of 0. These 32 bytes
//! specify the key and IV with which the rest of the data is encrypted.
//!
//! The directory itself is encrypted in CBC mode. Since directory edits
//! require simply appending data to the file, each chunk of data is padded
//! with surrogate 1-byte entries (see the directory format for more details).
//! Appending is done by using the last ciphertext block as the IV.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::fmt;
use std::io::{Read, Write};
use std::result::Result as StdResult;

use chrono::{DateTime, NaiveDateTime, UTC};
use fourleaf::{self, UnknownFields};
use fourleaf::adapt::Copied;
use keccak;
use rand::{Rng, OsRng};
use rust_crypto::{aes, blockmodes, scrypt};
use rust_crypto::buffer::{BufferResult, ReadBuffer, WriteBuffer,
                          RefReadBuffer, RefWriteBuffer};
use rust_crypto::symmetriccipher::{Decryptor, Encryptor, SymmetricCipherError};

use defs::{HashId, UNKNOWN_HASH};
use errors::*;

const SCRYPT_18_14_12_8_1: &'static str = "scrypt-18/14/12-8-1";
pub const BLKSZ: usize = 16;
pub const GROUP_EVERYONE: &'static str = "everyone";
pub const GROUP_ROOT: &'static str = "root";

/// Stored in cleartext fourleaf as directory `[0u8;32]`.
///
/// This stores the parameters used for the key-derivation function of each
/// passphrase and how to move from a derived key to the internal keys.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KdfList {
    pub keys: BTreeMap<String, KdfEntry>,
    pub unknown: UnknownFields<'static>,
}

fourleaf_retrofit!(struct KdfList : {} {} {
    |_context, this|
    [1] keys: BTreeMap<String, KdfEntry> = &this.keys,
    (?) unknown: Copied<UnknownFields<'static>> = &this.unknown,
    { Ok(KdfList { keys: keys, unknown: unknown.0 }) }
});

/// A single passphrase which may be used to derive internal keys
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KdfEntry {
    /// The time this (logical) entry was created.
    pub created: DateTime<UTC>,
    /// The time this (logical) entry was last updated.
    pub updated: Option<DateTime<UTC>>,
    /// The algorithm used.
    ///
    /// This includes the parameters used. Note that these are not parsed;
    /// the possible combinations are hardwired.
    ///
    /// The only option right now is "scrypt-18-8-1".
    pub algorithm: String,
    /// The randomly-generated salt.
    pub salt: HashId,
    /// The SHA3 hash of the derived key, to determine whether the key is
    /// correct.
    pub hash: HashId,
    /// The groups to which this entry is associated. Each key is a group name,
    /// and the value is the XOR of the internal key of the group with the HMAC
    /// of the group name and the derived key.
    pub groups: BTreeMap<String, HashId>,
    pub unknown: UnknownFields<'static>,
}

#[derive(Clone, Copy)]
struct SerDt(DateTime<UTC>);
fourleaf_retrofit!(struct SerDt : {} {} {
    |context, this|
    [1] secs: i64 = this.0.naive_utc().timestamp(),
    [2] nsecs: u32 = this.0.naive_utc().timestamp_subsec_nanos(),
    { NaiveDateTime::from_timestamp_opt(secs, nsecs)
      .ok_or(fourleaf::de::Error::InvalidValueMsg(
          context.to_string(), "invalid timestamp"))
      .map(|ndt| SerDt(DateTime::from_utc(ndt, UTC))) }
});

fourleaf_retrofit!(struct KdfEntry : {} {} {
    |_context, this|
    [1] created: SerDt = SerDt(this.created),
    [2] updated: Option<SerDt> = this.updated.map(SerDt),
    [3] algorithm: String = &this.algorithm,
    [4] salt: HashId = this.salt,
    [5] hash: HashId = this.hash,
    [6] groups: BTreeMap<String, HashId> = &this.groups,
    (?) unknown: Copied<UnknownFields<'static>> = &this.unknown,
    { Ok(KdfEntry { created: created.0,
                    updated: updated.map(|v| v.0),
                    algorithm: algorithm, salt: salt, hash: hash,
                    groups: groups,
                    unknown: unknown.0 }) }
});

thread_local! {
    static RANDOM: RefCell<OsRng> = RefCell::new(
        OsRng::new().expect("Failed to create OsRng"));
}

pub fn rand(buf: &mut [u8]) {
    RANDOM.with(|r| r.borrow_mut().fill_bytes(buf))
}

pub fn rand_hashid() -> HashId {
    let mut h = HashId::default();
    rand(&mut h);
    h
}

/// A set of internal keys derived from a passphrase.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyChain {
    /// Map from group names to internal keys associated with this key chain.
    pub keys: BTreeMap<String, InternalKey>,
    /// The intermediate key derived from the passphrase.
    pub derived: InternalKey,
}

impl KeyChain {
    /// Generate a new key chain with the default groups bound to new internal
    /// keys.
    pub fn generate_new() -> Self {
        let mut keys = BTreeMap::new();
        keys.insert(GROUP_ROOT.to_owned(), InternalKey::generate_new());
        keys.insert(GROUP_EVERYONE.to_owned(), InternalKey::generate_new());

        KeyChain {
            keys: keys,
            derived: InternalKey(UNKNOWN_HASH),
        }
    }

    /// Returns a `KeyChain` with no keys and an unknown derived key.
    pub fn empty() -> Self {
        KeyChain {
            keys: BTreeMap::new(),
            derived: InternalKey(UNKNOWN_HASH),
        }
    }

    /// Returns the internal key with the given name, or an error if not
    /// associated to this key chain.
    pub fn key(&self, name: &str) -> Result<&InternalKey> {
        self.keys.get(name).ok_or_else(
            || ErrorKind::KeyNotInGroup(name.to_owned()).into())
    }

    /// Returns the HMAC secret to use for object hashing.
    pub fn obj_hmac_secret(&self) -> Result<&[u8]> {
        self.key(GROUP_EVERYONE).map(InternalKey::hmac_secret)
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct InternalKey(HashId);

impl InternalKey {
    /// Generates a new, random internal key.
    pub fn generate_new() -> Self {
        let mut this = InternalKey(Default::default());
        rand(&mut this.0);
        this
    }

    pub fn dir_key(&self) -> &[u8] {
        &self.0[0..BLKSZ]
    }

    pub fn hmac_secret(&self) -> &[u8] {
        &self.0[BLKSZ..BLKSZ*2]
    }
}

impl fmt::Debug for InternalKey {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        // Write the SHA3 instead of the actual key so this can't be leaked out
        // accidentally.
        write!(f, "InternalKey(sha3={:?})", sha3(&self.0))
    }
}

fn scrypt_18_14_12_8_1(passphrase: &[u8], salt: &[u8]) -> HashId {
    // Scrypt paper recommends n=2**14, r=8, p=1
    // Slides in http://www.tarsnap.com/scrypt/scrypt-slides.pdf suggest
    // n=2**20 for file encryption, but that requires 1GB of memory.
    // As a compromise, we use n=2**18, which needs "only" 256MB.
    //
    // For larger passphrases, we use the weaker n values of 14 and 12, since
    // at those points the input likely has a larger value space than the
    // derived hash anyway (i.e., a brute-force would be faster against the
    // derived key than the passphrase itself). We vary this at hashing time,
    // rather than selecting the algorithm when the key is created, so that we
    // don't leak information on the passphrase size while at the same time
    // making usage with a large random passphrase fast even in the presence of
    // other keys which had shorter passphrases with stronger hashes.
    //
    // For tests, we implicitly use weaker parameters because scrypt-18-8-1
    // takes forever in debug builds, and it wouldn't make sense to have
    // another algorithm option which exists only for tests to use.
    #[cfg(not(test))] const N18: u8 = 18; #[cfg(test)] const N18: u8 = 12;
    #[cfg(not(test))] const N14: u8 = 14; #[cfg(test)] const N14: u8 = 10;
    #[cfg(not(test))] const N12: u8 = 12; #[cfg(test)] const N12: u8 = 8;
    #[cfg(not(test))] const R:  u32 = 8;  #[cfg(test)] const R:  u32 = 4;
    let n = if passphrase.len() >= 256 {
        N12
    } else if passphrase.len() >= 64 {
        N14
    } else {
        N18
    };
    let sparms = scrypt::ScryptParams::new(n, R, 1);
    let mut derived: HashId = Default::default();
    scrypt::scrypt(passphrase, &salt, &sparms, &mut derived);

    return derived;
}

fn sha3(data: &[u8]) -> HashId {
    let mut hash = HashId::default();
    let mut kc = keccak::Keccak::new_sha3_256();
    kc.update(data);
    kc.finalize(&mut hash);
    hash
}

fn hmac(data: &[u8], secret: &[u8]) -> HashId {
    let mut hash = HashId::default();
    let mut kc = keccak::Keccak::new_sha3_256();
    kc.update(data);
    kc.update(secret);
    kc.finalize(&mut hash);
    hash
}

fn hixor(a: &HashId, b: &HashId) -> HashId {
    let mut out = HashId::default();

    for (out, (&a, &b)) in out.iter_mut().zip(a.iter().zip(b.iter())) {
        *out = a ^ b;
    }

    out
}

/// Creates a new key entry keyed off of the given passphrase which can be used
/// to derive the internal keys.
///
/// The new entry will have the same groups as the key chain. The chain will be
/// updated to hold the intermediate key derived from `passphrase`.
///
/// The caller must provide the logic for determining the various date-time
/// fields itself.
pub fn create_key(passphrase: &[u8], chain: &mut KeyChain,
                  created: DateTime<UTC>,
                  updated: Option<DateTime<UTC>>)
                  -> KdfEntry {
    let mut salt = HashId::default();
    rand(&mut salt);

    let algorithm = SCRYPT_18_14_12_8_1;
    let derived = scrypt_18_14_12_8_1(passphrase, &salt);

    chain.derived = InternalKey(derived);

    let mut entry = KdfEntry {
        created: created,
        updated: updated,
        algorithm: algorithm.to_owned(),
        salt: salt,
        hash: sha3(&derived),
        groups: BTreeMap::new(),
        unknown: UnknownFields::default(),
    };
    reassoc_keys(&mut entry, chain);
    entry
}

/// Clear the groups on `entry`, then repopulate to match the internal keys on
/// `chain`.
pub fn reassoc_keys(entry: &mut KdfEntry, chain: &KeyChain) {
    entry.groups.clear();
    for (name, internal_key) in &chain.keys {
        entry.groups.insert(name.to_owned(),
                            hixor(&hmac(name.as_bytes(), &chain.derived.0),
                                  &internal_key.0));
    }
}


/// Attempts to derive the internal keys from the given single KDF entry.
pub fn try_derive_key_single(passphrase: &[u8], entry: &KdfEntry)
                             -> Option<KeyChain> {
    match entry.algorithm.as_str() {
        SCRYPT_18_14_12_8_1 => Some(scrypt_18_14_12_8_1(passphrase, &entry.salt)),
        _ => None,
    }.and_then(|derived| {
        if sha3(&derived) == entry.hash {
            let mut keys = BTreeMap::new();
            for (name, diff) in &entry.groups {
                keys.insert(name.to_owned(), InternalKey(
                    hixor(&hmac(name.as_bytes(), &derived), diff)));
            }

            Some(KeyChain {
                keys: keys,
                derived: InternalKey(derived),
            })
        } else {
            None
        }
    })
}

/// Attempts to derive the internal keys from the given passphrase and key
/// list.
///
/// If successful, returns the derived key chain. Otherwise, returns `None`.
pub fn try_derive_key(passphrase: &[u8], keys: &BTreeMap<String, KdfEntry>)
                      -> Option<KeyChain> {
    keys.iter()
        .filter_map(|(_, k)| try_derive_key_single(passphrase, k))
        .next()
}

// Since rust-crypto uses two traits which are identical except for method
// names, for some reason, which are also incompatible with std::io
trait Cryptor {
    fn crypt(&mut self, output: &mut RefWriteBuffer, input: &mut RefReadBuffer,
             eof: bool) -> StdResult<BufferResult, SymmetricCipherError>;
}
struct WEncryptor(Box<dyn Encryptor>);
impl Cryptor for WEncryptor {
    fn crypt(&mut self, output: &mut RefWriteBuffer, input: &mut RefReadBuffer,
             eof: bool) -> StdResult<BufferResult, SymmetricCipherError>
    { self.0.encrypt(input, output, eof) }
}
struct WDecryptor(Box<dyn Decryptor>);
impl Cryptor for WDecryptor {
    fn crypt(&mut self, output: &mut RefWriteBuffer, input: &mut RefReadBuffer,
             eof: bool) -> StdResult<BufferResult, SymmetricCipherError>
    { self.0.decrypt(input, output, eof) }
}

/// Copy `src` into `dst` after passing input bytes through `crypt`.
///
/// If `panic_on_crypt_err` is true and the cryptor itself fails, the process
/// panics. Otherwise, any errors are handled by simply writing to `dst` a
/// number of zero bytes equal to the unconsumed bytes from that pass in `src`.
/// The latter is used on decryption to prevent the software from behaving
/// differently in the presence of invalid padding.
///
/// (The discussion below is not really particular to this function, but it's
/// here to keep it all in one place.)
///
/// Note that while this offers protection against padding oracle attacks,
/// there are still other timing-based attacks that could be performed based on
/// how the data is handled.
///
/// In the case of objects, there are no such attacks, since objects are
/// validated by feeding the whole thing into an HMAC function.
///
/// For directories, we validate each chunk's signature before attempting to
/// parse it. Even if an attacker knew the chunk boundaries (which could be
/// discovered via a sophisticated side-channel attack), a padding-oracle style
/// attack is infeasible since it is necessarily at least as difficult as
/// forging SHA-3 HMAC. (Also note again that directories do not use PKCS
/// padding.)
fn crypt_stream<W : Write, R : Read, C : Cryptor>(
    mut dst: W, mut src: R, crypt: &mut C,
    panic_on_crypt_err: bool) -> Result<()>
{
    let mut src_buf = [0u8;4096];
    let mut dst_buf = [0u8;4112]; // Extra space for final padding block
    let mut eof = false;
    while !eof {
        let mut nread = 0;
        while !eof && nread < src_buf.len() {
            let n = src.read(&mut src_buf[nread..])?;
            eof |= 0 == n;
            nread += n;
        }

        // Passing src_buf through the cryptor should always result in the
        // entire thing being consumed, as either we have read a multiple of
        // the block size or EOF has been reached.
        let dst_len = {
            let mut dstrbuf = RefWriteBuffer::new(&mut dst_buf);
            let mut srcrbuf = RefReadBuffer::new(&mut src_buf[..nread]);
            match crypt.crypt(&mut dstrbuf, &mut srcrbuf, eof) {
                Ok(_) => {
                    assert!(srcrbuf.is_empty());
                },
                Err(e) => {
                    if panic_on_crypt_err {
                        panic!("Crypt error: {:?}", e);
                    }

                    for d in dstrbuf.take_next(srcrbuf.remaining()) {
                        *d = 0;
                    }
                },
            };
            dstrbuf.position()
        };

        dst.write_all(&dst_buf[..dst_len])?;
    }

    Ok(())
}

fn split_key_and_iv(key_and_iv: &[u8;32]) -> ([u8;BLKSZ],[u8;BLKSZ]) {
    let mut key = [0u8;BLKSZ];
    key.copy_from_slice(&key_and_iv[0..BLKSZ]);
    let mut iv = [0u8;BLKSZ];
    iv.copy_from_slice(&key_and_iv[BLKSZ..32]);
    (key, iv)
}

/// Generates and writes the CBC encryption prefix to `dst`.
///
/// `master` is the portion of the master key used to encrypt this prefix.
fn write_cbc_prefix<W : Write>(dst: W, master: &[u8])
                               -> Result<([u8;BLKSZ],[u8;BLKSZ])> {
    let mut key_and_iv = [0u8;32];
    rand(&mut key_and_iv);

    let mut cryptor = WEncryptor(aes::cbc_encryptor(
        aes::KeySize::KeySize128, master, &[0u8;BLKSZ],
        blockmodes::NoPadding));
    crypt_stream(dst, &mut&key_and_iv[..], &mut cryptor, true)?;

    Ok(split_key_and_iv(&key_and_iv))
}

/// Reads out the data written by `write_cbc_prefix()`.
fn read_cbc_prefix<R : Read>(mut src: R, master: &[u8])
                             -> Result<([u8;BLKSZ],[u8;BLKSZ])> {
    let mut cipher_head = [0u8;32];
    src.read_exact(&mut cipher_head)?;
    let mut cryptor = WDecryptor(aes::cbc_decryptor(
        aes::KeySize::KeySize128, master, &[0u8;BLKSZ],
        blockmodes::NoPadding));

    let mut key_and_iv = [0u8;32];
    crypt_stream(&mut&mut key_and_iv[..], &mut&cipher_head[..],
                      &mut cryptor, false)?;

    Ok(split_key_and_iv(&key_and_iv))
}

/// Encrypts the object data in `src` using the key from the object's id,
/// writing the encrypted result to `dst`.
pub fn encrypt_obj<W : Write, R : Read>(dst: W, src: R, id: &HashId)
                                        -> Result<()> {
    let mut key = [0u8;BLKSZ];
    let mut iv = [0u8;BLKSZ];
    key.copy_from_slice(&id[..BLKSZ]);
    iv.copy_from_slice(&id[BLKSZ..]);

    let mut cryptor = WEncryptor(aes::cbc_encryptor(
        aes::KeySize::KeySize128, &key, &iv, blockmodes::PkcsPadding));
    crypt_stream(dst, src, &mut cryptor, true)?;
    Ok(())
}

/// Reverses `encrypt_obj()`.
pub fn decrypt_obj<W : Write, R : Read>(dst: W, src: R, id: &HashId)
                                        -> Result<()> {
    let mut key = [0u8;BLKSZ];
    let mut iv = [0u8;BLKSZ];
    key.copy_from_slice(&id[..BLKSZ]);
    iv.copy_from_slice(&id[BLKSZ..]);

    let mut cryptor = WDecryptor(aes::cbc_decryptor(
        aes::KeySize::KeySize128, &key, &iv, blockmodes::PkcsPadding));
    crypt_stream(dst, src, &mut cryptor, false)?;
    Ok(())
}

/// Transforms the given object id to be safe to send to the server.
///
/// This is not reversible.
pub fn xform_obj_id(id: &HashId) -> HashId {
    sha3(id)
}

/// Encrypt a whole directory file.
///
/// This generates a new session key and iv. The session key is returned, which
/// can be used with `encrypt_append_dir()` to append more data to the file.
///
/// `src` must produce data which is a multiple of BLKSZ bytes long.
pub fn encrypt_whole_dir<W : Write, R : Read>(mut dst: W, src: R,
                                              key: &InternalKey)
                                              -> Result<[u8;BLKSZ]> {
    let (key, iv) = write_cbc_prefix(&mut dst, key.dir_key())?;

    let mut cryptor = WEncryptor(aes::cbc_encryptor(
        aes::KeySize::KeySize128, &key, &iv, blockmodes::NoPadding));
    crypt_stream(dst, src, &mut cryptor, true)?;
    Ok(key)
}

/// Encrypts data to be appended to a directory.
///
/// `key` is the session key returned by `encrypt_whole_dir()` or
/// `decrypt_whole_dir()`. `iv` is the append-IV returned by `dir_append_iv()`.
pub fn encrypt_append_dir<W : Write, R : Read>(dst: W, src: R,
                                               key: &[u8;BLKSZ], iv: &[u8;BLKSZ])
                                               -> Result<()> {
    let mut cryptor = WEncryptor(aes::cbc_encryptor(
        aes::KeySize::KeySize128, key, iv, blockmodes::NoPadding));
    crypt_stream(dst, src, &mut cryptor, true)?;
    Ok(())
}

/// Inverts `encrypt_whole_dir()` and any subsequent calls to
/// `encrypt_append_dir()`.
pub fn decrypt_whole_dir<W : Write, R : Read>(dst: W, mut src: R,
                                              key: &InternalKey)
                                              -> Result<[u8;BLKSZ]> {
    let (key, iv) = read_cbc_prefix(&mut src, key.dir_key())?;

    let mut cryptor = WDecryptor(aes::cbc_decryptor(
        aes::KeySize::KeySize128, &key, &iv, blockmodes::NoPadding));
    crypt_stream(dst, src, &mut cryptor, false)?;
    Ok(key)
}

/// Given a suffix of the full ciphertext content of a directory `data`, return
/// the IV to pass to `encrypt_append_dir` to append more data to that
/// directory.
pub fn dir_append_iv(data: &[u8]) -> [u8;BLKSZ] {
    let mut iv = [0u8;BLKSZ];
    iv.copy_from_slice(&data[data.len() - BLKSZ..]);
    iv
}

fn dir_ver_iv(dir: &HashId) -> [u8;BLKSZ] {
    let mut iv = [0u8;BLKSZ];
    for ix in 0..8 {
        iv[ix] = dir[ix] ^ dir[ix+BLKSZ];
    }
    iv
}

/// Encrypts the version of the given directory.
pub fn encrypt_dir_ver(dir: &HashId, mut ver: u64, key: &KeyChain)
                       -> HashId {
    let key = key.key(GROUP_EVERYONE).expect(
        "Key chain does not have `everyone` group");

    let mut cleartext = HashId::default();
    for ix in 0..8 {
        cleartext[ix] = (ver & 0xFF) as u8;
        ver >>= 8;
    }

    let mut res = HashId::default();
    let mut cryptor = WEncryptor(aes::cbc_encryptor(
        aes::KeySize::KeySize128, key.dir_key(),
        &dir_ver_iv(dir),
        blockmodes::NoPadding));
    crypt_stream(&mut res[..], &cleartext[..], &mut cryptor, true)
        .expect("Directory version encryption failed");

    res
}

/// Inverts `encrypt_dir_ver()`.
///
/// If `ciphertext` is invalid, 0 is silently returned instead.
pub fn decrypt_dir_ver(dir: &HashId, ciphertext: &HashId, key: &KeyChain)
                       -> u64 {
    let key = key.key(GROUP_EVERYONE).expect(
        "Key chain does not have `everyone` group");

    // Initialise to something that we'd reject below
    let mut cleartext = [255u8;32];
    let mut cryptor = WDecryptor(aes::cbc_decryptor(
        aes::KeySize::KeySize128, key.dir_key(),
        &dir_ver_iv(dir),
        blockmodes::NoPadding));
    // Ignore any errors and simply leave `cleartext` initialised to the
    // invalid value.
    let _ = crypt_stream(&mut cleartext[..], &ciphertext[..],
                         &mut cryptor, false);

    // If the padding is invalid, silently return 0 so it gets rejected the
    // same way as simply receding the version.
    for &padding in &cleartext[8..] {
        if 0 != padding {
            return 0;
        }
    }

    let mut ver = 0u64;
    for ix in 0..8 {
        ver |= (cleartext[ix] as u64) << (ix * 8);
    }

    ver
}

/// Returns the secret version corresponding to the encrypted version `v`.
///
/// The secret version is simply the HMAC of the normal version with the key's
/// HMAC secret.
pub fn secret_dir_ver(v: &HashId, key: &InternalKey) -> HashId {
    hmac(v, key.hmac_secret())
}

#[cfg(test)]
mod test {
    use chrono::UTC;

    use std::collections::BTreeMap;

    use super::*;

    fn ck(passphrase: &[u8], keychain: &mut KeyChain) -> KdfEntry {
        create_key(passphrase, keychain, UTC::now(), None)
    }

    #[test]
    fn generate_and_derive_keys() {
        let mut keychain = KeyChain::generate_new();
        let mut keys = BTreeMap::new();
        keys.insert("a".to_owned(), ck(b"plugh", &mut keychain));
        keys.insert("b".to_owned(), ck(b"xyzzy", &mut keychain));

        assert_eq!(Some(&keychain.keys),
                   try_derive_key(b"plugh", &keys).as_ref().map(|c| &c.keys));
        assert_eq!(Some(&keychain.keys),
                   try_derive_key(b"xyzzy", &keys).as_ref().map(|c| &c.keys));
        assert_eq!(None, try_derive_key(b"foo", &keys));
    }

    #[test]
    fn generate_and_derive_keys_long_passphrase() {
        let pw_a = [b'a';1024];
        let pw_b = [b'b';1024];
        let pw_c = [b'c';1024];

        let mut keychain = KeyChain::generate_new();
        let mut keys = BTreeMap::new();
        keys.insert("a".to_owned(), ck(&pw_a, &mut keychain));
        keys.insert("b".to_owned(), ck(&pw_b, &mut keychain));

        assert_eq!(Some(&keychain.keys),
                   try_derive_key(&pw_a, &keys).as_ref().map(|c| &c.keys));
        assert_eq!(Some(&keychain.keys),
                   try_derive_key(&pw_b, &keys).as_ref().map(|c| &c.keys));
        assert_eq!(None, try_derive_key(&pw_c, &keys));

    }
}

// Separate module so only the fast tess can be run when so desired
#[cfg(test)]
mod fast_test {
    use defs::HashId;

    use super::hmac;
    use super::*;

    fn test_crypt_obj(data: &[u8]) {
        let keychain = KeyChain::generate_new();
        let id = hmac(data, keychain.obj_hmac_secret().unwrap());

        let mut ciphertext = Vec::new();
        encrypt_obj(&mut ciphertext, data, &id).unwrap();

        let mut cleartext = Vec::new();
        decrypt_obj(&mut cleartext, &ciphertext[..], &id).unwrap();

        assert_eq!(data, &cleartext[..]);
    }

    #[test]
    fn crypt_obj_empty() {
        test_crypt_obj(&[]);
    }

    #[test]
    fn crypt_obj_one_block() {
        test_crypt_obj(b"0123456789abcdef");
    }

    #[test]
    fn crypt_obj_partial_single_block() {
        test_crypt_obj(b"hello");
    }

    #[test]
    fn crypt_obj_partial_multi_block() {
        test_crypt_obj(b"This is longer than sixteen bytes.");
    }

    #[test]
    fn crypt_obj_4096() {
        let mut data = [0u8;4096];
        rand(&mut data);
        test_crypt_obj(&data);
    }

    #[test]
    fn crypt_obj_4097() {
        let mut data = [0u8;4097];
        rand(&mut data);
        test_crypt_obj(&data);
    }

    #[test]
    fn crypt_obj_8191() {
        let mut data = [0u8;8191];
        rand(&mut data);
        test_crypt_obj(&data);
    }

    #[test]
    fn crypt_dir_oneshot() {
        let key = InternalKey::generate_new();

        let orig = b"0123456789abcdef0123456789ABCDEF";
        let mut ciphertext = Vec::new();
        let sk1 = encrypt_whole_dir(&mut ciphertext, &orig[..], &key).unwrap();

        let mut cleartext = Vec::new();
        let sk2 = decrypt_whole_dir(&mut cleartext, &ciphertext[..],
                                    &key).unwrap();

        assert_eq!(orig, &cleartext[..]);
        assert_eq!(sk1, sk2);
    }

    #[test]
    fn crypt_dir_appended() {
        let key = InternalKey::generate_new();

        let mut ciphertext = Vec::new();
        let sk = encrypt_whole_dir(
            &mut ciphertext, &b"0123456789abcdef"[..], &key).unwrap();
        let iv = dir_append_iv(&ciphertext);
        encrypt_append_dir(
            &mut ciphertext, &b"0123456789ABCDEF"[..], &sk, &iv).unwrap();

        let mut cleartext = Vec::new();
        decrypt_whole_dir(&mut cleartext, &ciphertext[..], &key).unwrap();

        assert_eq!(&b"0123456789abcdef0123456789ABCDEF"[..], &cleartext[..]);
    }

    #[test]
    fn crypt_dir_version() {
        let keychain = KeyChain::generate_new();

        let mut dir = HashId::default();
        rand(&mut dir);

        assert_eq!(42u64, decrypt_dir_ver(
            &dir, &encrypt_dir_ver(&dir, 42u64, &keychain), &keychain));
    }

    #[test]
    fn corrupt_dir_version_decrypted_to_0() {
        let keychain = KeyChain::generate_new();

        assert_eq!(0u64, decrypt_dir_ver(
            &HashId::default(), &HashId::default(), &keychain));
    }
}
