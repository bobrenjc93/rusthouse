use std::mem::size_of;

/// Number of values in every sealed Int64 chunk.
pub const INT64_CHUNK_SIZE: usize = 1_024;

/// Observable storage details for an [`Int64Column`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Int64StorageStats {
    pub values: usize,
    pub sealed_chunks: usize,
    pub constant_chunks: usize,
    pub delta_chunks: usize,
    pub raw_chunks: usize,
    pub tail_values: usize,
    /// Bytes required by the selected encodings, excluding container overhead.
    pub encoded_bytes: usize,
    /// Heap bytes currently allocated by the column, including container overhead.
    pub allocated_bytes: usize,
    /// Bytes the values would occupy in a contiguous `Vec<i64>` without spare capacity.
    pub logical_bytes: usize,
}

/// An appendable Int64 column with adaptively compressed immutable chunks.
#[derive(Debug, Clone, Default)]
pub struct Int64Column {
    sealed: Vec<SealedInt64Chunk>,
    tail: Vec<i64>,
    len: usize,
}

#[derive(Debug, Clone)]
enum SealedInt64Chunk {
    Constant(i64),
    Delta {
        base: i64,
        bit_width: u8,
        packed: Box<[u64]>,
    },
    Raw(Box<[i64]>),
}

impl Int64Column {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.len
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn push(&mut self, value: i64) {
        self.tail.push(value);
        self.len += 1;
        if self.tail.len() == INT64_CHUNK_SIZE {
            let values = std::mem::take(&mut self.tail);
            self.sealed.push(SealedInt64Chunk::encode(values));
        }
    }

    /// Returns a value by logical row index, decoding sealed chunks as needed.
    #[must_use]
    pub fn get(&self, index: usize) -> Option<i64> {
        if index >= self.len {
            return None;
        }

        let sealed_values = self.sealed.len() * INT64_CHUNK_SIZE;
        if index < sealed_values {
            let chunk = &self.sealed[index / INT64_CHUNK_SIZE];
            Some(chunk.value(index % INT64_CHUNK_SIZE))
        } else {
            Some(self.tail[index - sealed_values])
        }
    }

    #[must_use]
    pub fn iter(&self) -> Int64Iter<'_> {
        Int64Iter {
            column: self,
            index: 0,
        }
    }

    #[must_use]
    pub fn storage_stats(&self) -> Int64StorageStats {
        let mut stats = Int64StorageStats {
            values: self.len,
            sealed_chunks: self.sealed.len(),
            constant_chunks: 0,
            delta_chunks: 0,
            raw_chunks: 0,
            tail_values: self.tail.len(),
            encoded_bytes: self.tail.len() * size_of::<i64>(),
            allocated_bytes: self.sealed.capacity() * size_of::<SealedInt64Chunk>()
                + self.tail.capacity() * size_of::<i64>(),
            logical_bytes: self.len * size_of::<i64>(),
        };

        for chunk in &self.sealed {
            stats.encoded_bytes += chunk.encoded_bytes();
            stats.allocated_bytes += chunk.heap_bytes();
            match chunk {
                SealedInt64Chunk::Constant(_) => stats.constant_chunks += 1,
                SealedInt64Chunk::Delta { .. } => stats.delta_chunks += 1,
                SealedInt64Chunk::Raw(_) => stats.raw_chunks += 1,
            }
        }
        stats
    }

    pub(crate) fn value(&self, index: usize) -> i64 {
        self.get(index).expect("Int64 row index out of bounds")
    }
}

impl Extend<i64> for Int64Column {
    fn extend<T: IntoIterator<Item = i64>>(&mut self, iter: T) {
        for value in iter {
            self.push(value);
        }
    }
}

impl FromIterator<i64> for Int64Column {
    fn from_iter<T: IntoIterator<Item = i64>>(iter: T) -> Self {
        let mut column = Self::new();
        column.extend(iter);
        column
    }
}

impl<'a> IntoIterator for &'a Int64Column {
    type Item = i64;
    type IntoIter = Int64Iter<'a>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

/// Exact-size iterator over decoded Int64 values.
#[derive(Debug, Clone)]
pub struct Int64Iter<'a> {
    column: &'a Int64Column,
    index: usize,
}

impl Iterator for Int64Iter<'_> {
    type Item = i64;

    fn next(&mut self) -> Option<Self::Item> {
        let value = self.column.get(self.index)?;
        self.index += 1;
        Some(value)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let remaining = self.column.len - self.index;
        (remaining, Some(remaining))
    }

    fn fold<B, F>(mut self, mut accumulator: B, mut fold: F) -> B
    where
        Self: Sized,
        F: FnMut(B, Self::Item) -> B,
    {
        let sealed_values = self.column.sealed.len() * INT64_CHUNK_SIZE;
        while self.index < sealed_values {
            let chunk_index = self.index / INT64_CHUNK_SIZE;
            let offset = self.index % INT64_CHUNK_SIZE;
            accumulator = self.column.sealed[chunk_index].fold(offset, accumulator, &mut fold);
            self.index = (chunk_index + 1) * INT64_CHUNK_SIZE;
        }

        if self.index < self.column.len {
            let tail_offset = self.index - sealed_values;
            for &value in &self.column.tail[tail_offset..] {
                accumulator = fold(accumulator, value);
            }
            self.index = self.column.len;
        }
        accumulator
    }
}

impl ExactSizeIterator for Int64Iter<'_> {}

impl SealedInt64Chunk {
    fn encode(values: Vec<i64>) -> Self {
        debug_assert_eq!(values.len(), INT64_CHUNK_SIZE);

        let mut minimum = values[0];
        let mut maximum = values[0];
        for &value in &values[1..] {
            minimum = minimum.min(value);
            maximum = maximum.max(value);
        }
        if minimum == maximum {
            return Self::Constant(minimum);
        }

        let maximum_delta = (i128::from(maximum) - i128::from(minimum)) as u64;
        let bit_width = (u64::BITS - maximum_delta.leading_zeros()) as u8;
        let packed_words = (values.len() * usize::from(bit_width)).div_ceil(u64::BITS as usize);
        let delta_bytes = size_of::<i64>() + size_of::<u8>() + packed_words * size_of::<u64>();
        let raw_bytes = values.len() * size_of::<i64>();

        if delta_bytes >= raw_bytes {
            return Self::Raw(values.into_boxed_slice());
        }

        let mut packed = vec![0_u64; packed_words];
        for (index, value) in values.into_iter().enumerate() {
            let delta = (i128::from(value) - i128::from(minimum)) as u64;
            pack(&mut packed, index, bit_width, delta);
        }
        Self::Delta {
            base: minimum,
            bit_width,
            packed: packed.into_boxed_slice(),
        }
    }

    fn value(&self, index: usize) -> i64 {
        debug_assert!(index < INT64_CHUNK_SIZE);
        match self {
            Self::Constant(value) => *value,
            Self::Delta {
                base,
                bit_width,
                packed,
            } => {
                let delta = unpack(packed, index, *bit_width);
                base.wrapping_add(delta as i64)
            }
            Self::Raw(values) => values[index],
        }
    }

    fn fold<B, F>(&self, offset: usize, mut accumulator: B, fold: &mut F) -> B
    where
        F: FnMut(B, i64) -> B,
    {
        match self {
            Self::Constant(value) => {
                for _ in offset..INT64_CHUNK_SIZE {
                    accumulator = fold(accumulator, *value);
                }
            }
            Self::Delta {
                base,
                bit_width,
                packed,
            } => {
                let width = usize::from(*bit_width);
                let mask = (1_u64 << bit_width) - 1;
                let first_bit = offset * width;
                let first_word = first_bit / u64::BITS as usize;
                let bit_offset = first_bit % u64::BITS as usize;
                let mut next_word = first_word + 1;
                let mut buffer = packed[first_word] >> bit_offset;
                let mut available = u64::BITS as usize - bit_offset;
                for _ in offset..INT64_CHUNK_SIZE {
                    let delta = if available >= width {
                        let delta = buffer & mask;
                        buffer >>= width;
                        available -= width;
                        delta
                    } else {
                        let word = packed[next_word];
                        next_word += 1;
                        let delta = (buffer | (word << available)) & mask;
                        let consumed = width - available;
                        buffer = word >> consumed;
                        available = u64::BITS as usize - consumed;
                        delta
                    };
                    let value = base.wrapping_add(delta as i64);
                    accumulator = fold(accumulator, value);
                }
            }
            Self::Raw(values) => {
                for &value in &values[offset..] {
                    accumulator = fold(accumulator, value);
                }
            }
        }
        accumulator
    }

    fn encoded_bytes(&self) -> usize {
        match self {
            Self::Constant(_) => size_of::<i64>(),
            Self::Delta { packed, .. } => {
                size_of::<i64>() + size_of::<u8>() + std::mem::size_of_val(packed.as_ref())
            }
            Self::Raw(values) => std::mem::size_of_val(values.as_ref()),
        }
    }

    fn heap_bytes(&self) -> usize {
        match self {
            Self::Constant(_) => 0,
            Self::Delta { packed, .. } => std::mem::size_of_val(packed.as_ref()),
            Self::Raw(values) => std::mem::size_of_val(values.as_ref()),
        }
    }
}

fn pack(words: &mut [u64], index: usize, bit_width: u8, value: u64) {
    debug_assert!(bit_width < 64);
    let width = usize::from(bit_width);
    let bit_index = index * width;
    let word_index = bit_index / u64::BITS as usize;
    let offset = bit_index % u64::BITS as usize;
    words[word_index] |= value << offset;
    if offset + width > u64::BITS as usize {
        words[word_index + 1] |= value >> (u64::BITS as usize - offset);
    }
}

fn unpack(words: &[u64], index: usize, bit_width: u8) -> u64 {
    debug_assert!(bit_width < 64);
    let width = usize::from(bit_width);
    let bit_index = index * width;
    let word_index = bit_index / u64::BITS as usize;
    let offset = bit_index % u64::BITS as usize;
    let mut value = words[word_index] >> offset;
    if offset + width > u64::BITS as usize {
        value |= words[word_index + 1] << (u64::BITS as usize - offset);
    }
    value & ((1_u64 << bit_width) - 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chooses_each_sealed_encoding_by_measured_size() {
        let values = std::iter::repeat_n(7, INT64_CHUNK_SIZE)
            .chain((0..INT64_CHUNK_SIZE).map(|value| value as i64))
            .chain((0..INT64_CHUNK_SIZE).map(|index| {
                if index.is_multiple_of(2) {
                    i64::MIN
                } else {
                    i64::MAX
                }
            }));
        let column = values.collect::<Int64Column>();
        let stats = column.storage_stats();

        assert_eq!(stats.sealed_chunks, 3);
        assert_eq!(stats.constant_chunks, 1);
        assert_eq!(stats.delta_chunks, 1);
        assert_eq!(stats.raw_chunks, 1);
        assert_eq!(stats.tail_values, 0);
    }

    #[test]
    fn generated_columns_round_trip_for_random_access_and_scans() {
        let mut random = SplitMix64::new(0x5eed_cafe_f00d_beef);
        for case in 0..96 {
            let len = (random.next() as usize) % (INT64_CHUNK_SIZE * 3 + 31);
            let mut expected = Vec::with_capacity(len);
            for index in 0..len {
                let value = match case % 6 {
                    0 => case as i64 - 48,
                    1 => index as i64 - len as i64 / 2,
                    2 => (random.next() % 2_000_001) as i64 - 1_000_000,
                    3 => random.next() as i64,
                    4 => [i64::MIN, i64::MIN + 1, -1, 0, 1, i64::MAX - 1, i64::MAX][index % 7],
                    _ => (random.next() % 17) as i64 - 8,
                };
                expected.push(value);
            }

            let column = expected.iter().copied().collect::<Int64Column>();
            assert_eq!(column.len(), expected.len());
            assert_eq!(column.iter().collect::<Vec<_>>(), expected);
            assert_eq!(
                column.iter().fold(Vec::new(), |mut values, value| {
                    values.push(value);
                    values
                }),
                expected
            );
            assert_eq!(column.iter().len(), expected.len());
            for (index, expected) in expected.into_iter().enumerate() {
                assert_eq!(column.get(index), Some(expected));
            }
            assert_eq!(column.get(len), None);
        }
    }

    #[test]
    fn every_delta_bit_width_round_trips_across_word_boundaries() {
        let mut random = SplitMix64::new(0x0123_4567_89ab_cdef);
        for bit_width in 1..64 {
            let mask = (1_u64 << bit_width) - 1;
            let mut expected = (0..INT64_CHUNK_SIZE)
                .map(|_| i64::MIN.wrapping_add((random.next() & mask) as i64))
                .collect::<Vec<_>>();
            expected[0] = i64::MIN;
            expected[1] = i64::MIN.wrapping_add(mask as i64);

            let column = expected.iter().copied().collect::<Int64Column>();
            assert_eq!(column.storage_stats().delta_chunks, 1, "width {bit_width}");
            assert_eq!(
                column.iter().collect::<Vec<_>>(),
                expected,
                "width {bit_width}"
            );
            assert_eq!(
                column.iter().fold(Vec::new(), |mut values, value| {
                    values.push(value);
                    values
                }),
                expected,
                "optimized width {bit_width}"
            );
        }
    }

    #[test]
    fn integer_boundaries_survive_delta_and_raw_chunks() {
        let near_min = (0..INT64_CHUNK_SIZE).map(|index| i64::MIN + (index % 257) as i64);
        let near_max = (0..INT64_CHUNK_SIZE).map(|index| i64::MAX - (index % 257) as i64);
        let full_range = (0..INT64_CHUNK_SIZE).map(|index| {
            if index.is_multiple_of(2) {
                i64::MIN
            } else {
                i64::MAX
            }
        });
        let expected = near_min
            .chain(near_max)
            .chain(full_range)
            .chain([i64::MIN, -1, 0, 1, i64::MAX])
            .collect::<Vec<_>>();
        let column = expected.iter().copied().collect::<Int64Column>();

        assert_eq!(column.iter().collect::<Vec<_>>(), expected);
        let stats = column.storage_stats();
        assert_eq!(stats.delta_chunks, 2);
        assert_eq!(stats.raw_chunks, 1);
        assert_eq!(stats.tail_values, 5);
    }

    #[test]
    fn transitions_between_sealed_chunks_and_tail_are_exact() {
        for len in [
            INT64_CHUNK_SIZE - 1,
            INT64_CHUNK_SIZE,
            INT64_CHUNK_SIZE + 1,
            INT64_CHUNK_SIZE * 2,
            INT64_CHUNK_SIZE * 2 + 1,
        ] {
            let column = (0..len).map(|value| value as i64).collect::<Int64Column>();
            let stats = column.storage_stats();
            assert_eq!(column.len(), len);
            assert_eq!(stats.sealed_chunks, len / INT64_CHUNK_SIZE);
            assert_eq!(stats.tail_values, len % INT64_CHUNK_SIZE);
            assert_eq!(column.get(0), Some(0));
            assert_eq!(column.get(len - 1), Some((len - 1) as i64));
            assert_eq!(column.get(len), None);
        }
    }

    #[test]
    fn optimized_scan_resumes_after_random_iterator_access() {
        let expected = (0..INT64_CHUNK_SIZE * 2 + 7)
            .map(|index| (index % 113) as i64 - 56)
            .collect::<Vec<_>>();
        let column = expected.iter().copied().collect::<Int64Column>();
        let mut iter = column.iter();

        assert_eq!(iter.next(), Some(expected[0]));
        assert_eq!(
            iter.nth(INT64_CHUNK_SIZE),
            Some(expected[INT64_CHUNK_SIZE + 1])
        );
        let suffix = iter.fold(Vec::new(), |mut values, value| {
            values.push(value);
            values
        });
        assert_eq!(suffix, expected[INT64_CHUNK_SIZE + 2..]);
    }

    #[test]
    fn memory_accounting_includes_encodings_and_heap_allocations() {
        let constant = std::iter::repeat_n(11, INT64_CHUNK_SIZE).collect::<Int64Column>();
        let delta = (0..INT64_CHUNK_SIZE)
            .map(|value| value as i64)
            .collect::<Int64Column>();
        let raw = (0..INT64_CHUNK_SIZE)
            .map(|index| {
                if index.is_multiple_of(2) {
                    i64::MIN
                } else {
                    i64::MAX
                }
            })
            .collect::<Int64Column>();

        let constant = constant.storage_stats();
        let delta = delta.storage_stats();
        let raw = raw.storage_stats();
        assert_eq!(constant.encoded_bytes, size_of::<i64>());
        assert_eq!(delta.encoded_bytes, 1_289);
        assert_eq!(raw.encoded_bytes, INT64_CHUNK_SIZE * size_of::<i64>());
        assert!(constant.allocated_bytes < delta.allocated_bytes);
        assert!(delta.allocated_bytes < raw.allocated_bytes);
        assert_eq!(constant.logical_bytes, raw.logical_bytes);
        assert!(raw.allocated_bytes > raw.logical_bytes);
    }

    #[derive(Debug, Clone, Copy)]
    struct SplitMix64 {
        state: u64,
    }

    impl SplitMix64 {
        fn new(seed: u64) -> Self {
            Self { state: seed }
        }

        fn next(&mut self) -> u64 {
            self.state = self.state.wrapping_add(0x9e37_79b9_7f4a_7c15);
            let mut value = self.state;
            value = (value ^ (value >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
            value = (value ^ (value >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
            value ^ (value >> 31)
        }
    }
}
