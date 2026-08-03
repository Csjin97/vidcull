use std::io::{self, Read};

use vidcull_fingerprint::content_hash::{CHUNK_SIZE, hash_file, hash_reader};

#[test]
fn chunk_size_is_64_kib() {
    assert_eq!(CHUNK_SIZE, 64 * 1024);
}

#[test]
fn identical_bytes_hash_identically_in_memory() {
    let data = vec![0xABu8; 250_000];
    let h1 = hash_reader(&data[..]).expect("baseline hash");
    let h2 = hash_reader(&data[..]).expect("repeat hash");
    assert_eq!(h1, h2);
}

#[test]
fn hash_is_invariant_to_read_chunking() {
    let data: Vec<u8> = (0..250_000u32).map(|i| (i % 251) as u8).collect();
    let baseline = hash_reader(&data[..]).expect("baseline");
    for chunk in [1usize, 4_096, CHUNK_SIZE, CHUNK_SIZE * 2] {
        let h = hash_reader(ChunkyReader::new(&data, chunk)).expect("chunked");
        assert_eq!(h, baseline, "digest diverged at read-chunk {chunk}");
    }
}

#[test]
fn single_byte_flip_changes_the_hash() {
    let mut data = vec![0u8; CHUNK_SIZE + 10];
    let before = hash_reader(&data[..]).expect("before");
    data[CHUNK_SIZE + 5] ^= 0x01;
    let after = hash_reader(&data[..]).expect("after");
    assert_ne!(before, after);
}

#[test]
fn streaming_loop_never_buffers_more_than_chunk_size() {
    let mut tracker = MaxReadTracker::new(2 * 1024 * 1024);
    hash_reader(&mut tracker).expect("hash should succeed");
    assert!(
        tracker.max_buf_requested > 0,
        "the streaming loop must request at least one read"
    );
    assert!(
        tracker.max_buf_requested <= CHUNK_SIZE,
        "hash_reader asked for {} bytes in a single read; max allowed is {CHUNK_SIZE}",
        tracker.max_buf_requested,
    );
}

#[test]
fn empty_input_produces_a_stable_digest() {
    let from_empty_reader = hash_reader(io::empty()).expect("io::empty");
    let from_empty_slice = hash_reader(&[][..]).expect("empty slice");
    assert_eq!(from_empty_reader, from_empty_slice);
}

#[test]
fn hash_file_matches_hash_reader_for_the_same_bytes() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("sample.bin");
    let data: Vec<u8> = (0..100_000u32).flat_map(u32::to_le_bytes).collect();
    std::fs::write(&path, &data).expect("write sample");

    let from_file = hash_file(&path).expect("hash file");
    let from_mem = hash_reader(&data[..]).expect("hash slice");
    assert_eq!(from_file, from_mem);
}

#[test]
fn distinct_payloads_yield_distinct_digests() {
    let mut seen = std::collections::HashSet::new();
    for tail in 0u8..32 {
        let mut buf = vec![0u8; 1024];
        buf[1023] = tail;
        let h = hash_reader(&buf[..]).expect("hash");
        assert!(seen.insert(h), "unexpected collision at tail byte {tail}");
    }
    assert_eq!(seen.len(), 32);
}

struct ChunkyReader<'a> {
    data: &'a [u8],
    chunk: usize,
    pos: usize,
}

impl<'a> ChunkyReader<'a> {
    fn new(data: &'a [u8], chunk: usize) -> Self {
        assert!(chunk > 0, "chunk must be positive");
        Self {
            data,
            chunk,
            pos: 0,
        }
    }
}

impl Read for ChunkyReader<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let remaining = self.data.len() - self.pos;
        let n = remaining.min(buf.len()).min(self.chunk);
        if n == 0 {
            return Ok(0);
        }
        buf[..n].copy_from_slice(&self.data[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

struct MaxReadTracker {
    remaining: usize,
    max_buf_requested: usize,
}

impl MaxReadTracker {
    fn new(remaining: usize) -> Self {
        Self {
            remaining,
            max_buf_requested: 0,
        }
    }
}

impl Read for MaxReadTracker {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.max_buf_requested = self.max_buf_requested.max(buf.len());
        let n = buf.len().min(self.remaining);
        for byte in &mut buf[..n] {
            *byte = 0xAB;
        }
        self.remaining -= n;
        Ok(n)
    }
}
