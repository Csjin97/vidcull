use std::collections::{HashMap, HashSet};
use std::fmt;
use std::fs::File;
use std::io::{BufReader, Seek, SeekFrom};
use std::path::Path;

use vidcull_core::{Error, Result};

use crate::cancel::Cancel;
use crate::sparse::{SparseSample, SparseSampleSource, SparseStep};

pub struct Mp4SampleSource {
    file: BufReader<File>,
    idr_table: HashMap<u32, (u64, u32)>,
}

impl Mp4SampleSource {
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self> {
        Self::open_cancellable(path, Cancel::default())
    }

    pub fn open_cancellable<P: AsRef<Path>>(path: P, cancel: Cancel<'_>) -> Result<Self> {
        let context = crate::mp4::read_mp4_tolerant_cancellable(path.as_ref(), cancel)?;
        Self::from_context(&context, path.as_ref())
    }

    pub(crate) fn from_context(context: &mp4parse::MediaContext, path: &Path) -> Result<Self> {
        let track = context
            .tracks
            .iter()
            .find(|t| matches!(t.track_type, mp4parse::TrackType::Video))
            .ok_or_else(|| Error::Parse("mp4: no video track found".into()))?;

        let sync_samples = track.stss.as_ref();
        let sample_to_chunk = track
            .stsc
            .as_ref()
            .ok_or_else(|| Error::Parse("mp4: video track has no stsc".into()))?;
        let sample_sizes = track
            .stsz
            .as_ref()
            .ok_or_else(|| Error::Parse("mp4: video track has no stsz".into()))?;
        let chunk_offsets = track
            .stco
            .as_ref()
            .ok_or_else(|| Error::Parse("mp4: video track has no stco/co64".into()))?;

        let idr_table =
            build_idr_table(sync_samples, sample_to_chunk, sample_sizes, chunk_offsets)?;

        let file = BufReader::with_capacity(crate::bounded::READ_BUF_CAPACITY, File::open(path)?);

        Ok(Self { file, idr_table })
    }

    #[must_use]
    pub fn idr_count(&self) -> usize {
        self.idr_table.len()
    }
}

impl fmt::Debug for Mp4SampleSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Mp4SampleSource")
            .field("idr_count", &self.idr_table.len())
            .finish_non_exhaustive()
    }
}

impl SparseSampleSource for Mp4SampleSource {
    fn fetch(&mut self, step: &SparseStep) -> Result<SparseSample> {
        let &(offset, size) = self.idr_table.get(&step.locator).ok_or_else(|| {
            Error::Parse(format!(
                "mp4: locator {} is not an IDR sample number (not in stss)",
                step.locator
            ))
        })?;
        self.file.seek(SeekFrom::Start(offset))?;
        let bytes = crate::bounded::read_exact_bounded(
            &mut self.file,
            u64::from(size),
            "mp4: sparse sample",
        )?;
        Ok(SparseSample {
            timestamp_ms: step.timestamp_ms,
            bytes,
        })
    }
}

fn build_idr_table(
    sync_samples: Option<&mp4parse::SyncSampleBox>,
    sample_to_chunk: &mp4parse::SampleToChunkBox,
    sample_sizes: &mp4parse::SampleSizeBox,
    chunk_offsets: &mp4parse::ChunkOffsetBox,
) -> Result<HashMap<u32, (u64, u32)>> {
    let total_chunks = u32::try_from(chunk_offsets.offsets.len())
        .map_err(|_| Error::Parse("mp4: chunk count exceeds u32 range".into()))?;
    if total_chunks == 0 {
        return Err(Error::Parse("mp4: stco has zero chunks".into()));
    }

    let chunk_samples = build_chunk_samples_lookup(sample_to_chunk, total_chunks)?;

    let idr_set: Option<HashSet<u32>> = match sync_samples {
        Some(box_data) => {
            let set: HashSet<u32> = box_data.samples.iter().copied().collect();
            if set.is_empty() {
                return Err(Error::Parse(
                    "mp4: stss present but contains zero entries".into(),
                ));
            }
            Some(set)
        }
        None => None,
    };

    let uniform_size = sample_sizes.sample_size;
    let mut table: HashMap<u32, (u64, u32)> = HashMap::new();
    let mut sample_num: u32 = 1;

    for (chunk_idx, &chunk_offset) in chunk_offsets.offsets.iter().enumerate() {
        let mut offset = chunk_offset;
        let count = chunk_samples[chunk_idx];
        for _ in 0..count {
            let size = sample_size(uniform_size, sample_sizes, sample_num)?;
            let is_idr = match idr_set.as_ref() {
                Some(set) => set.contains(&sample_num),
                None => true,
            };
            if is_idr {
                table.insert(sample_num, (offset, size));
            }
            offset = offset
                .checked_add(u64::from(size))
                .ok_or_else(|| Error::Parse("mp4: chunk offset overflow during walk".into()))?;
            sample_num = sample_num
                .checked_add(1)
                .ok_or_else(|| Error::Parse("mp4: sample number overflow during walk".into()))?;
        }
    }

    match idr_set {
        Some(set) if table.len() != set.len() => {
            return Err(Error::Parse(format!(
                "mp4: stss has {} entries but only {} were resolved (stsc/stsz/stco inconsistency)",
                set.len(),
                table.len()
            )));
        }
        None if table.is_empty() => {
            return Err(Error::Parse(
                "mp4: video track has no stss and zero samples (nothing to index)".into(),
            ));
        }
        _ => {}
    }

    Ok(table)
}

fn build_chunk_samples_lookup(
    sample_to_chunk: &mp4parse::SampleToChunkBox,
    total_chunks: u32,
) -> Result<Vec<u32>> {
    let entries: Vec<(u32, u32)> = sample_to_chunk
        .samples
        .iter()
        .map(|s| (s.first_chunk, s.samples_per_chunk))
        .collect();
    if entries.is_empty() {
        return Err(Error::Parse("mp4: stsc has zero entries".into()));
    }
    if entries[0].0 != 1 {
        return Err(Error::Parse(format!(
            "mp4: stsc first entry's first_chunk = {}, expected 1",
            entries[0].0
        )));
    }

    let mut chunk_samples: Vec<u32> = vec![0; total_chunks as usize];
    for (idx, &(first_chunk, spc)) in entries.iter().enumerate() {
        if first_chunk == 0 || first_chunk > total_chunks {
            return Err(Error::Parse(format!(
                "mp4: stsc first_chunk {first_chunk} out of range [1, {total_chunks}]"
            )));
        }
        let last_chunk_excl = if idx + 1 < entries.len() {
            let next_first = entries[idx + 1].0;
            if next_first <= first_chunk {
                return Err(Error::Parse(format!(
                    "mp4: stsc first_chunk values not strictly ascending ({first_chunk} → {next_first})"
                )));
            }
            next_first
        } else {
            total_chunks + 1
        };
        for c in first_chunk..last_chunk_excl {
            chunk_samples[(c - 1) as usize] = spc;
        }
    }
    Ok(chunk_samples)
}

fn sample_size(
    uniform: u32,
    sample_sizes: &mp4parse::SampleSizeBox,
    sample_num: u32,
) -> Result<u32> {
    if uniform > 0 {
        return Ok(uniform);
    }
    let idx = (sample_num as usize).checked_sub(1).ok_or_else(|| {
        Error::Parse("mp4: stsz indexed with sample number 0 (samples are 1-based)".into())
    })?;
    sample_sizes
        .sample_sizes
        .get(idx)
        .copied()
        .ok_or_else(|| Error::Parse(format!("mp4: stsz too short at sample {sample_num}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sample_size_returns_uniform_when_set() {
        let box_data = mp4parse::SampleSizeBox {
            sample_size: 1024,
            sample_sizes: mp4parse::TryVec::new(),
        };
        assert_eq!(sample_size(1024, &box_data, 1).unwrap(), 1024);
        assert_eq!(sample_size(1024, &box_data, 999).unwrap(), 1024);
    }

    #[test]
    fn sample_size_consults_per_sample_table_when_uniform_zero() {
        let mut per_sample = mp4parse::TryVec::new();
        per_sample.push(100u32).unwrap();
        per_sample.push(200u32).unwrap();
        per_sample.push(300u32).unwrap();
        let box_data = mp4parse::SampleSizeBox {
            sample_size: 0,
            sample_sizes: per_sample,
        };
        assert_eq!(sample_size(0, &box_data, 1).unwrap(), 100);
        assert_eq!(sample_size(0, &box_data, 2).unwrap(), 200);
        assert_eq!(sample_size(0, &box_data, 3).unwrap(), 300);
    }

    #[test]
    fn sample_size_errors_when_per_sample_table_too_short() {
        let box_data = mp4parse::SampleSizeBox {
            sample_size: 0,
            sample_sizes: mp4parse::TryVec::new(),
        };
        assert!(sample_size(0, &box_data, 1).is_err());
    }

    struct SampleTables {
        sample_to_chunk: mp4parse::SampleToChunkBox,
        sample_sizes: mp4parse::SampleSizeBox,
        chunk_offsets: mp4parse::ChunkOffsetBox,
    }

    fn single_chunk_tables(count: u32, chunk_offset: u64, size: u32) -> SampleTables {
        let mut stsc_entries = mp4parse::TryVec::new();
        stsc_entries
            .push(mp4parse::SampleToChunk {
                first_chunk: 1,
                samples_per_chunk: count,
                sample_description_index: 1,
            })
            .unwrap();
        let mut offsets = mp4parse::TryVec::new();
        offsets.push(chunk_offset).unwrap();
        SampleTables {
            sample_to_chunk: mp4parse::SampleToChunkBox {
                samples: stsc_entries,
            },
            sample_sizes: mp4parse::SampleSizeBox {
                sample_size: size,
                sample_sizes: mp4parse::TryVec::new(),
            },
            chunk_offsets: mp4parse::ChunkOffsetBox { offsets },
        }
    }

    #[test]
    fn build_idr_table_absent_stss_resolves_every_sample() {
        let t = single_chunk_tables(4, 1000, 100);
        let table =
            build_idr_table(None, &t.sample_to_chunk, &t.sample_sizes, &t.chunk_offsets).unwrap();
        assert_eq!(table.len(), 4);
        assert_eq!(table[&1], (1000, 100));
        assert_eq!(table[&2], (1100, 100));
        assert_eq!(table[&3], (1200, 100));
        assert_eq!(table[&4], (1300, 100));
    }

    #[test]
    fn build_idr_table_present_stss_resolves_only_listed() {
        let t = single_chunk_tables(4, 0, 100);
        let mut samples = mp4parse::TryVec::new();
        samples.push(1u32).unwrap();
        samples.push(3u32).unwrap();
        let sync = mp4parse::SyncSampleBox { samples };
        let table = build_idr_table(
            Some(&sync),
            &t.sample_to_chunk,
            &t.sample_sizes,
            &t.chunk_offsets,
        )
        .unwrap();
        assert_eq!(table.len(), 2);
        assert_eq!(table[&1], (0, 100));
        assert_eq!(table[&3], (200, 100));
        assert!(!table.contains_key(&2));
    }

    #[test]
    fn build_idr_table_absent_stss_rejects_zero_sample_track() {
        let t = single_chunk_tables(0, 0, 100);
        assert!(
            build_idr_table(None, &t.sample_to_chunk, &t.sample_sizes, &t.chunk_offsets).is_err()
        );
    }
}
