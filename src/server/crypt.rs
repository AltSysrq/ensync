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
//! `KdfEntry` defined in `serde_types.in.rs`.
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
//! store. To do this, when we initialise the key store, we generate a random
//! master key. Each passphrase as already described produces a secondary key.
//! In the key store, we store the XOR of the master key with each secondary
//! key. In effect, each secondary key is used as a one-time pad to encrypt the
//! master key.
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
//! - XOR the derived key with the master key diff and we have the master key.
//!
//! Note that the keys here are actually 256 bits wide, twice the size of an
//! AES key. We thus actually have *two* independent master keys. The first 128
//! bits are used for encryption operations on directories; the second 128 bits
//! are used for encryption operations on objects. The full 256 bits is used as
//! the HMAC secret.
//!
//! # Encrypting objects
//!
//! Since objects are immutable, opaque blobs, their handling is reasonably
//! simple:
//!
//! - Generate a random 128-bit key and IV.
//!
//! - Encrypt those two (two AES blocks) with the object master key in CBC mode
//! with IV 0 (the mode and IV here don't matter since the cleartext is pure
//! entropy) and write that at the beginning of the object. Encrypt the object
//! data in CBC mode using the saved key and IV.
//!
//! Generating a random key for each object ensures that objects with similar
//! prefices are nonetheless different. The random IV may make this stronger,
//! but definitely does not hurt.
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
//! - Pad it with 8 zero bytes.
//!
//! - Encrypt it with AES in CBC mode using the directory key and an IV equal
//! to the directory id.
//!
//! Encrypting the integers this way obfuscates how often a directory is
//! usually updated. It does not alone prevent tampering since an attacker
//! could simply choose not to change the version. Because of this, we also
//! store the version in the directory. (This aspect is documented in the
//! directory format docs.)
//!
//! # Directory Contents
//!
//! The contents of a directory are prefixed with an encrypted key and IV in
//! the same way as objects, except that the directory master key is used.
//!
//! The directory itself is encrypted in CBC mode. Since directory edits
//! require simply appending data to the file, each chunk of data is padded
//! with surrogate 1-byte entries (see the directory format for more details).
//! Appending is done by using the last ciphertext block as the IV.

use std::cell::RefCell;
use std::fmt;
use std::io::{self, Read, Write};
use std::result::Result as StdResult;

use keccak;
use rand::{Rng, OsRng};
use rust_crypto::{aes, blockmodes, scrypt};
use rust_crypto::buffer::{BufferResult, ReadBuffer, WriteBuffer,
                          RefReadBuffer, RefWriteBuffer};
use rust_crypto::symmetriccipher::{Decryptor, Encryptor, SymmetricCipherError};

use defs::HashId;
use errors::*;
use serde_types::crypt::*;

const SCRYPT_18_8_1: &'static str = "scrypt-18-8-1";

thread_local! {
    static RANDOM: RefCell<OsRng> = RefCell::new(
        OsRng::new().expect("Failed to create OsRng"));
}

fn rand(buf: &mut [u8]) {
    RANDOM.with(|r| r.borrow_mut().fill_bytes(buf))
}

#[derive(Clone,Copy,PartialEq,Eq)]
pub struct MasterKey(HashId);

impl MasterKey {
    /// Generates a new, random master key.
    pub fn generate_new() -> Self {
        let mut this = MasterKey(Default::default());
        rand(&mut this.0);
        this
    }

    pub fn dir_key(&self) -> &[u8] {
        &self.0[0..16]
    }

    pub fn obj_key(&self) -> &[u8] {
        &self.0[16..32]
    }

    pub fn hmac_secret(&self) -> &[u8] {
        &self.0[..]
    }
}

impl fmt::Debug for MasterKey {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        // Write the SHA3 instead of the actual key so this can't be leaked out
        // accidentally.
        write!(f, "MasterKey(sha3={:?})", sha3(&self.0))
    }
}

fn scrypt_18_8_1(passphrase: &[u8], salt: &[u8]) -> HashId {
    // Scrypt paper recommends n=2**14, r=8, p=1
    // Slides in http://www.tarsnap.com/scrypt/scrypt-slides.pdf suggest
    // n=2**20 for file encryption, but that requires 1GB of memory.
    // As a compromise, we use n=2**18, which needs "only" 256MB.
    let sparms = scrypt::ScryptParams::new(18, 8, 1);
    let mut derived: HashId = Default::default();
    scrypt::scrypt(passphrase, &salt, &sparms, &mut derived);

    return derived;
}

fn sha3(data: &[u8]) -> HashId {
    let mut hash = HashId::default();
    let mut kc = keccak::Keccak::new_sha3_256();
    kc.update(&data);
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
/// to derive the master key.
pub fn create_key(passphrase: &[u8], master: &MasterKey)
                  -> KdfEntry {
    let mut salt = HashId::default();
    rand(&mut salt);

    let derived = scrypt_18_8_1(passphrase, &salt);
    KdfEntry {
        algorithm: SCRYPT_18_8_1.to_owned(),
        salt: H(salt),
        hash: H(sha3(&derived)),
        master_diff: H(hixor(&derived, &master.0)),
    }
}


fn try_derive_key_single(passphrase: &[u8], entry: &KdfEntry)
                         -> Option<MasterKey> {
    match entry.algorithm.as_str() {
        SCRYPT_18_8_1 => Some(scrypt_18_8_1(passphrase, &entry.salt.0)),
        _ => None,
    }.and_then(|derived| {
        if sha3(&derived) == entry.hash.0 {
            Some(MasterKey(hixor(&derived, &entry.master_diff.0)))
        } else {
            None
        }
    })
}

/// Attempts to derive the master key from the given passphrase and key list.
///
/// If successful, returns the derived master key. Otherwise, returns `None`.
pub fn try_derive_key<'a, IT : IntoIterator<Item = &'a KdfEntry>>(
    passphrase: &[u8], keys: IT) -> Option<MasterKey>
{
    keys.into_iter()
        .filter_map(|k| try_derive_key_single(passphrase, k))
        .next()
}

// Since rust-crypto uses two traits which are identical except for method
// names, for some reason, which are also incompatible with std::io
trait Cryptor {
    fn crypt(&mut self, output: &mut RefWriteBuffer, input: &mut RefReadBuffer,
             eof: bool) -> StdResult<BufferResult, SymmetricCipherError>;
}
struct WEncryptor(Box<Encryptor>);
impl Cryptor for WEncryptor {
    fn crypt(&mut self, output: &mut RefWriteBuffer, input: &mut RefReadBuffer,
             eof: bool) -> StdResult<BufferResult, SymmetricCipherError>
    { self.0.encrypt(input, output, eof) }
}
struct WDecryptor(Box<Decryptor>);
impl Cryptor for WDecryptor {
    fn crypt(&mut self, output: &mut RefWriteBuffer, input: &mut RefReadBuffer,
             eof: bool) -> StdResult<BufferResult, SymmetricCipherError>
    { self.0.decrypt(input, output, eof) }
}

/// Copy `src` into `dst` after passing input bytes through `crypt`.
fn crypt_stream<W : Write, R : Read, C : Cryptor>(
    mut dst: W, mut src: R, mut crypt: &mut C)
    -> Result<()>
{
    let mut src_buf = [0u8;4096];
    let mut dst_buf = [0u8;4112]; // Extra space for final padding block
    let mut eof = false;
    while !eof {
        let mut nread = 0;
        while !eof && nread < src_buf.len() {
            let n = try!(src.read(&mut src_buf[nread..]));
            eof |= 0 == n;
            nread += n;
        }

        // Passing src_buf through the cryptor should always result in the
        // entire thing being consumed, as either we have read a multiple of
        // the block size or EOF has been reached.
        let dst_len = {
            let mut dstrbuf = RefWriteBuffer::new(&mut dst_buf);
            let mut srcrbuf = RefReadBuffer::new(&mut src_buf[..nread]);
            try!(crypt.crypt(&mut dstrbuf, &mut srcrbuf, eof)
                 .map_err(|_| ErrorKind::CryptError));
            assert!(srcrbuf.is_empty());
            dstrbuf.position()
        };

        try!(dst.write_all(&dst_buf[..dst_len]));
    }

    Ok(())
}

fn split_key_and_iv(key_and_iv: &[u8;32]) -> ([u8;16],[u8;16]) {
    let mut key = [0u8;16];
    key.copy_from_slice(&key_and_iv[0..16]);
    let mut iv = [0u8;16];
    iv.copy_from_slice(&key_and_iv[16..32]);
    (key, iv)
}

/// Generates and writes the CBC encryption prefix to `dst`.
///
/// `master` is the portion of the master key used to encrypt this prefix.
fn write_cbc_prefix<W : Write>(dst: W, master: &[u8])
                               -> Result<([u8;16],[u8;16])> {
    let mut key_and_iv = [0u8;32];
    rand(&mut key_and_iv);

    let mut cryptor = WEncryptor(aes::cbc_encryptor(
        aes::KeySize::KeySize128, master, &[0u8;16],
        blockmodes::NoPadding));
    try!(crypt_stream(dst, &mut&key_and_iv[..], &mut cryptor));

    Ok(split_key_and_iv(&key_and_iv))
}

/// Reads out the data written by `write_cbc_prefix()`.
fn read_cbc_prefix<R : Read>(mut src: R, master: &[u8])
                             -> Result<([u8;16],[u8;16])> {
    let mut cipher_head = [0u8;32];
    try!(src.read_exact(&mut cipher_head));
    let mut cryptor = WDecryptor(aes::cbc_decryptor(
        aes::KeySize::KeySize128, master, &[0u8;16],
        blockmodes::NoPadding));

    let mut key_and_iv = [0u8;32];
    try!(crypt_stream(&mut&mut key_and_iv[..], &mut&cipher_head[..],
                      &mut cryptor));

    Ok(split_key_and_iv(&key_and_iv))
}

/// Encrypts the object data in `src` using the key from `master`, writing the
/// encrypted result to `dst`.
pub fn encrypt_obj<W : Write, R : Read>(mut dst: W, src: R,
                                        master: &MasterKey)
                                        -> Result<()> {
    let (key, iv) = try!(write_cbc_prefix(&mut dst, master.obj_key()));

    let mut cryptor = WEncryptor(aes::cbc_encryptor(
        aes::KeySize::KeySize128, &key, &iv, blockmodes::PkcsPadding));
    try!(crypt_stream(dst, src, &mut cryptor));
    Ok(())
}

/// Reverses `encrypt_obj()`.
pub fn decrypt_obj<W : Write, R : Read>(dst: W, mut src: R,
                                        master: &MasterKey)
                                        -> Result<()> {
    let (key, iv) = try!(read_cbc_prefix(&mut src, master.obj_key()));

    let mut cryptor = WDecryptor(aes::cbc_decryptor(
        aes::KeySize::KeySize128, &key, &iv, blockmodes::PkcsPadding));
    try!(crypt_stream(dst, src, &mut cryptor));
    Ok(())
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn generate_and_derive_keys() {
        let master = MasterKey::generate_new();
        let mut keys = Vec::new();
        keys.push(create_key(b"plugh", &master));
        keys.push(create_key(b"xyzzy", &master));

        assert_eq!(Some(master), try_derive_key(b"plugh", &keys));
        assert_eq!(Some(master), try_derive_key(b"xyzzy", &keys));
        assert_eq!(None, try_derive_key(b"foo", &keys));
    }
}

// Separate module so only the fast tess can be run when so desired
#[cfg(test)]
mod fast_test {
    use super::*;
    use super::rand;

    fn test_crypt_obj(data: &[u8]) {
        let master = MasterKey::generate_new();

        let mut ciphertext = Vec::new();
        {
            let mut dptr = data;
            encrypt_obj(&mut ciphertext, &mut dptr, &master).unwrap();
        }

        let mut cleartext = Vec::new();
        decrypt_obj(&mut cleartext, &mut&ciphertext[..], &master).unwrap();

        assert_eq!(data, &cleartext[..]);
    }

    #[test]
    fn crypt_empty_obj() {
        test_crypt_obj(&[]);
    }

    #[test]
    fn crypt_one_block() {
        test_crypt_obj(b"0123456789abcdef");
    }

    #[test]
    fn crypt_partial_single_block() {
        test_crypt_obj(b"hello");
    }

    #[test]
    fn crypt_partial_multi_block() {
        test_crypt_obj(b"This is longer than sixteen bytes.");
    }

    #[test]
    fn crypt_4096() {
        let mut data = [0u8;4096];
        rand(&mut data);
        test_crypt_obj(&data);
    }

    #[test]
    fn crypt_4097() {
        let mut data = [0u8;4097];
        rand(&mut data);
        test_crypt_obj(&data);
    }

    #[test]
    fn crypt_8191() {
        let mut data = [0u8;8191];
        rand(&mut data);
        test_crypt_obj(&data);
    }
}