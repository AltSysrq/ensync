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

//! Implements the basic block transfer system.
//!
//! Specifically, this is concerned with taking a byte stream, breaking it into
//! blocks up to a maximum size, and providing a sequence of `HashId`s
//! representing those blocks and a final `HashId` representing the whole
//! stream.
//!
//! The `HashId` for each block is a SHA-3 HMAC using a caller-provided secret.
//! The `HashId` for the full file is the SHA-3 HMAC of all the `HashId`s
//! concatenated in order. (It does not really need to be an HMAC because the
//! directory blob that will contain the list needs to have its own
//! authentication system anyway, but this is more consistent and at worst
//! accomplishes nothing, but may improve security.) HMACs are verified when
//! deblocking.
//!
//! Totally empty inputs produce no blocks at all.
//!
//! Separating files into blocks, each of which is stored separately in the
//! server, has a number of benefits:
//!
//! - If only part of a large file has changed, we do not need to retransfer
//! the whole thing. This is particularly important for things like mbox files
//! (most edits occur at the very end), databases, and so forth.
//!
//! - The code is operationally simpler if we can load an entire transfer unit
//! into memory. This way, we can SHA-3 sum the data, see whether the server
//! already has it, and if not, transfer it. Without splitting into blocks, we
//! would need to make a second pass through the data when transferring to the
//! server, and this may yield a different SHA-3 sum in the case of concurrent
//! edits. It would also put more load on the disk cache.
//!
//! - Sparse files with sparse areas larger than the block size remain
//! essentially sparse, as all the sparse areas will be backed by the same blob
//! on the server. For this benefit to occur on both sides, the client also
//! needs to check for all-zero blocks and extend the file rather than
//! explicitly writing them.
//!
//! - An attacker which has copied files off the server is less able to
//! determine properties about the data based on blob sizes.
//!
//! This module does not handle encryption itself; the blocks it passes through
//! are still in cleartext.

#![allow(dead_code)]

use std::io;
use std::sync::Arc;

use keccak::Keccak;

use defs::*;
use errors::*;

/// The representation of a list of blocks into which an input stream was
/// split.
#[derive(Clone,Debug)]
pub struct BlockList {
    /// The SHA-3 sum of all elements of `blocks` concatenated.
    ///
    /// This identifies the file proper, and is used as the `HashId` in
    /// `FileData` and so forth.
    pub total: HashId,
    /// The HMACs of the component blocks of the stream. This will be empty if
    /// the stream itself was empty.
    pub blocks: Vec<HashId>,
    /// The total number of bytes that were read from the stream.
    pub size: FileSize,
}

/// Computes the hash of the given block using the same method used internally
/// in the block transfer system.
pub fn hash_block(secret: &[u8], block: &[u8]) -> HashId {
    let mut kc = Keccak::new_sha3_256();
    kc.update(secret);
    kc.update(block);
    let mut hash = [0;32];
    kc.finalize(&mut hash);
    hash
}

/// Breaks the input byte stream `input` into non-empty byte blocks up to size
/// `block_size`.
///
/// Each block is completely loaded into memory. Then, its HMAC is computed by
/// effectively prepending `secret` to it, and `block_out` is called with the
/// provided hash and the cleartext block data.
///
/// When everything succeeds, all the block HMACs are concatenated and input
/// into a SHA-3 hash representing the whole stream.
///
/// The hashes generated are guaranteed to be correct even if the underlying
/// byte source is being modified concurrently. However, the byte sequence
/// produced by this call (implicit in the concatenation of the blocks) may not
/// be coherent; i.e., by the very nature of how streaming the data from the
/// source works, it is not possible to get a coherent snapshot of the whole
/// input.
///
/// If some platform at some point does gain a way to read an entire file
/// coherently even in the presence of concurrent modification, a `Read`
/// implementation could be based on that, and then this function would
/// transitively provide a coherence guarantee as well.
pub fn stream_to_blocks<F : FnMut (&HashId, &[u8]) -> Result<()>,
                        R : io::Read>
    (mut input: R, block_size: usize, secret: &[u8],
     mut block_out: F) -> Result<BlockList>
{
    let mut blocks = Vec::new();
    let mut hash = [0u8;32];
    let mut size : FileSize = 0;
    let mut total_kc = Keccak::new_sha3_256();
    total_kc.update(secret);

    // Allocate in a vector so we don't blow 1MB of stack space
    let mut block_data: Vec<u8> = Vec::new();
    block_data.resize(block_size, 0);

    // Read blocks until we read an empty block.
    loop {
        let mut off = 0;
        // Fill the data for this block up to the maximum size or EOF.
        while off < block_size {
            match input.read(&mut block_data[off..]) {
                Ok(0) => break,
                Ok(nread) => off += nread,
                Err(ref e) if e.kind() == io::ErrorKind::Interrupted =>
                    continue,
                Err(e) => return Err(e.into()),
            };
        }

        // Empty block == EOF
        if 0 == off { break; }

        hash = hash_block(secret, &block_data[0..off]);

        try!(block_out(&hash, &block_data[0..off]));
        total_kc.update(&hash);
        blocks.push(hash);
        size += off as FileSize;
    }

    total_kc.finalize(&mut hash);
    Ok(BlockList {
        total: hash,
        blocks: blocks,
        size: size,
    })
}

/// Fetches the constituent blocks of a file, verifies them, and writes them to
/// a byte stream.
///
/// This is essentially the inverse of `stream_to_blocks`.
///
/// The total sum of `input` is verified. Then, each block in `input` is
/// fetched by invoking `block_fetch` to get a reader. The data for each block
/// is streamed (without the whole block being loaded into memory) into
/// `output`; at the same time, the HMAC is accumulated. Once the function
/// returns, `output` will have received a byte stream exactly equal to the one
/// read from `input` in `stream_to_blocks`.
///
/// If this returns an error, the data written to `output` must be considered
/// corrupt; no guarantees are made about it in this case.
pub fn blocks_to_stream<R : io::Read,
                        F : FnMut (&HashId) -> Result<R>,
                        W : io::Write>
    (input: &BlockList, mut output: W, secret: &[u8],
     mut block_fetch: F) -> Result<()>
{
    let mut hash = [0u8;32];
    let mut buf = [0u8;4096];

    // Sanity check the BlockList
    {
        let mut kc = Keccak::new_sha3_256();
        kc.update(secret);
        for h in &input.blocks {
            kc.update(h);
        }
        kc.finalize(&mut hash);

        if hash != input.total {
            return Err(ErrorKind::HmacMismatch.into());
        }
    }

    for id in &input.blocks {
        let mut reader = try!(block_fetch(id));
        let mut kc = Keccak::new_sha3_256();
        kc.update(secret);

        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(nread) => {
                    kc.update(&buf[0..nread]);
                    try!(output.write_all(&buf[0..nread]));
                },
                Err(ref e) if e.kind() == io::ErrorKind::Interrupted =>
                    continue,
                Err(e) => return Err(e.into()),
            }
        }

        kc.finalize(&mut hash);
        if hash != *id {
            return Err(ErrorKind::HmacMismatch.into());
        }
    }

    return Ok(())
}

/// A file data source (eg, for `Replica::TransferIn` or
/// `Replica::TransferOut`) representing a backing store which is not
/// content-addressable but instead presents files as linear byte streams.
pub trait StreamSource : io::Read {
    /// Resets the stream to the beginning of the file.
    fn reset(&mut self) -> Result<()>;
    /// Notifies the source of the final computed block list that was read from
    /// the source (once hitting EOF) so that it can update its own data
    /// structures accordinly.
    fn finish(&mut self, blocks: &BlockList) -> Result<()>;
}

/// A file data source representing a content-addressable backing store which
/// does not support inherent byte streams.
#[derive(Clone)]
pub struct ContentAddressableSource {
    /// The block list of the file being transferred.
    pub blocks: BlockList,
    /// The block size used for this object
    pub block_size: usize,
    /// Object from which to read actual block content.
    pub fetch: Arc<BlockFetch>,
}

/// Trait for fetching blocks found in a `ContentAddressableSource`.
pub trait BlockFetch {
    /// Fetches the block identified by `block`, returning a stream that can be
    /// used to obtain the data within.
    fn fetch(&self, block: &HashId) -> Result<Box<io::Read>>;
}

#[cfg(test)]
mod test {
    use std::collections::HashMap;

    use defs::*;
    use errors::*;
    use super::*;

    fn to_blocklist(text: &[u8], secret: &[u8])
                    -> (BlockList,HashMap<HashId,Vec<u8>>) {
        let mut blocks = HashMap::new();

        let blocklist = stream_to_blocks(
            text, 4, secret, |&id, data| {
                blocks.insert(id, data.to_vec());
                Ok(())
            }).unwrap();

        (blocklist, blocks)
    }

    fn to_stream(blocklist: &BlockList, blocks: &HashMap<HashId,Vec<u8>>,
                 secret: &[u8]) -> Result<Vec<u8>> {
        let mut output = Vec::new();
        try!(blocks_to_stream(&blocklist, &mut output, secret,
                              |h| Ok(&blocks[h][..])));
        Ok(output)
    }

    #[test]
    fn text_blocked_and_deblocked_correctly() {
        let text = &b"hello world"[..];

        let (blocklist, blocks) = to_blocklist(text, &b"secret"[..]);
        assert_eq!(3, blocks.len());
        assert_eq!(3, blocklist.blocks.len());
        assert_eq!(11, blocklist.size);

        assert_eq!(b"hell", &blocks[&blocklist.blocks[0]][..]);
        assert_eq!(b"o wo", &blocks[&blocklist.blocks[1]][..]);
        assert_eq!(b"rld",  &blocks[&blocklist.blocks[2]][..]);

        let output = to_stream(&blocklist, &blocks, &b"secret"[..])
            .unwrap();
        assert_eq!(text, &output[..]);
    }

    fn assert_hmac_mismatch<T>(r: Result<T>) {
        match r {
            Ok(_) => panic!("HMAC didn't fail!"),
            Err(e) => match *e.kind() {
                ErrorKind::HmacMismatch => (),
                _ => panic!("Unexpected error: {}", e),
            },
        }
    }

    #[test]
    fn hmac_fails_if_secret_wrong() {
        let text = &b"hello world"[..];
        let (blocklist, blocks) = to_blocklist(text, &b"secret"[..]);

        assert_hmac_mismatch(
            to_stream(&blocklist, &blocks, &b"geheimniss"[..]));
    }

    #[test]
    fn hmac_fails_if_blocklist_corrupted() {
        let text = &b"hello world"[..];
        let (mut blocklist, blocks) = to_blocklist(text, &b"secret"[..]);

        blocklist.blocks.swap(0, 1);

        assert_hmac_mismatch(
            to_stream(&blocklist, &blocks, &b"secret"[..]));
    }

    #[test]
    fn hmac_fails_if_data_corrupted() {
        let text = &b"hello world"[..];
        let (blocklist, mut blocks) = to_blocklist(text, &b"secret"[..]);

        blocks.get_mut(&blocklist.blocks[2]).unwrap()[0] ^= 1;

        assert_hmac_mismatch(
            to_stream(&blocklist, &blocks, &b"secret"[..]));
    }
}
