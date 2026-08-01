use std::{
    hash::{DefaultHasher, Hasher},
    mem::size_of,
};

use crate::error::{Error, Result};

const BITS_PER_WORD: usize = u64::BITS as usize;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DataType {
    Int64,
    Float64,
    Boolean,
    String,
}

impl DataType {
    pub const fn name(self) -> &'static str {
        match self {
            Self::Int64 => "Int64",
            Self::Float64 => "Float64",
            Self::Boolean => "Boolean",
            Self::String => "String",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Field {
    name: Box<str>,
    data_type: DataType,
    nullable: bool,
}

impl Field {
    pub fn new(name: impl Into<Box<str>>, data_type: DataType, nullable: bool) -> Self {
        Self {
            name: name.into(),
            data_type,
            nullable,
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub const fn data_type(&self) -> DataType {
        self.data_type
    }

    pub const fn nullable(&self) -> bool {
        self.nullable
    }

    fn retained_bytes(&self) -> usize {
        self.name.len()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Schema {
    fields: Box<[Field]>,
}

impl Schema {
    pub fn new(fields: impl Into<Box<[Field]>>) -> Self {
        Self {
            fields: fields.into(),
        }
    }

    pub fn fields(&self) -> &[Field] {
        &self.fields
    }

    pub fn field(&self, index: usize) -> Option<&Field> {
        self.fields.get(index)
    }

    pub fn len(&self) -> usize {
        self.fields.len()
    }

    pub fn is_empty(&self) -> bool {
        self.fields.is_empty()
    }

    pub fn retained_bytes(&self) -> usize {
        self.fields.len() * size_of::<Field>()
            + self.fields.iter().map(Field::retained_bytes).sum::<usize>()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Bitmap {
    words: Box<[u64]>,
    len: usize,
    capacity: usize,
}

impl Bitmap {
    pub(crate) fn with_capacity(capacity: usize) -> Self {
        let word_count = capacity.div_ceil(BITS_PER_WORD);
        Self {
            words: vec![0; word_count].into_boxed_slice(),
            len: 0,
            capacity,
        }
    }

    pub(crate) fn all(len: usize, capacity: usize) -> Self {
        debug_assert!(len <= capacity);
        let mut bitmap = Self::with_capacity(capacity);
        bitmap.len = len;
        let full_words = len / BITS_PER_WORD;
        bitmap.words[..full_words].fill(u64::MAX);
        let remainder = len % BITS_PER_WORD;
        if remainder != 0 {
            bitmap.words[full_words] = (1_u64 << remainder) - 1;
        }
        bitmap
    }

    pub(crate) fn push(&mut self, value: bool) -> Result<()> {
        if self.len == self.capacity {
            return Err(Error::CapacityExceeded {
                capacity: self.capacity,
            });
        }
        let index = self.len;
        self.len += 1;
        self.set(index, value);
        Ok(())
    }

    pub(crate) fn get(&self, index: usize) -> bool {
        assert!(index < self.len, "bitmap index out of bounds");
        self.word(index / BITS_PER_WORD) & (1_u64 << (index % BITS_PER_WORD)) != 0
    }

    pub(crate) fn set(&mut self, index: usize, value: bool) {
        assert!(index < self.len, "bitmap index out of bounds");
        let word = &mut self.words[index / BITS_PER_WORD];
        let mask = 1_u64 << (index % BITS_PER_WORD);
        if value {
            *word |= mask;
        } else {
            *word &= !mask;
        }
    }

    pub(crate) fn word(&self, index: usize) -> u64 {
        self.words.get(index).copied().unwrap_or(0) & self.tail_mask(index)
    }

    pub(crate) fn word_count(&self) -> usize {
        self.words.len()
    }

    pub(crate) fn intersect_with(&mut self, other: &Self) {
        debug_assert_eq!(self.len, other.len);
        debug_assert_eq!(self.capacity, other.capacity);
        for index in 0..self.words.len() {
            self.words[index] &= other.word(index);
        }
    }

    pub(crate) fn len(&self) -> usize {
        self.len
    }

    pub(crate) fn capacity(&self) -> usize {
        self.capacity
    }

    pub(crate) fn count_ones(&self) -> usize {
        self.words
            .iter()
            .enumerate()
            .map(|(index, word)| (word & self.tail_mask(index)).count_ones() as usize)
            .sum()
    }

    pub(crate) fn retained_bytes(&self) -> usize {
        self.words.len() * size_of::<u64>()
    }

    fn tail_mask(&self, word_index: usize) -> u64 {
        let word_start = word_index * BITS_PER_WORD;
        if word_start >= self.len {
            return 0;
        }
        let remaining = self.len - word_start;
        if remaining >= BITS_PER_WORD {
            u64::MAX
        } else {
            (1_u64 << remaining) - 1
        }
    }
}

/// A fixed-capacity bitmap identifying rows visible to execution kernels.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectionMask {
    bitmap: Bitmap,
}

impl SelectionMask {
    pub fn all(len: usize, capacity: usize) -> Result<Self> {
        if len > capacity {
            return Err(Error::CapacityExceeded { capacity });
        }
        Ok(Self {
            bitmap: Bitmap::all(len, capacity),
        })
    }

    pub fn none(len: usize, capacity: usize) -> Result<Self> {
        if len > capacity {
            return Err(Error::CapacityExceeded { capacity });
        }
        let mut bitmap = Bitmap::with_capacity(capacity);
        bitmap.len = len;
        Ok(Self { bitmap })
    }

    pub fn is_selected(&self, row: usize) -> bool {
        self.bitmap.get(row)
    }

    pub fn set(&mut self, row: usize, selected: bool) {
        self.bitmap.set(row, selected);
    }

    pub fn selected_count(&self) -> usize {
        self.bitmap.count_ones()
    }

    pub fn len(&self) -> usize {
        self.bitmap.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn capacity(&self) -> usize {
        self.bitmap.capacity()
    }

    pub fn retained_bytes(&self) -> usize {
        self.bitmap.retained_bytes()
    }

    pub fn intersect(&self, other: &Self) -> Result<Self> {
        self.validate_shape(other.len(), other.capacity())?;
        let mut result = Self::none(self.len(), self.capacity())?;
        for word in 0..self.bitmap.word_count() {
            result.bitmap.words[word] = self.bitmap.word(word) & other.bitmap.word(word);
        }
        Ok(result)
    }

    pub(crate) fn word(&self, index: usize) -> u64 {
        self.bitmap.word(index)
    }

    pub(crate) fn set_word(&mut self, index: usize, value: u64) {
        self.bitmap.words[index] = value & self.bitmap.tail_mask(index);
    }

    pub(crate) fn word_count(&self) -> usize {
        self.bitmap.word_count()
    }

    pub(crate) fn validate_shape(&self, len: usize, capacity: usize) -> Result<()> {
        if self.len() != len || self.capacity() != capacity {
            return Err(Error::SelectionMismatch {
                expected_len: len,
                actual_len: self.len(),
                expected_capacity: capacity,
                actual_capacity: self.capacity(),
            });
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct PrimitiveArray<T> {
    values: Box<[T]>,
    validity: Bitmap,
    len: usize,
}

impl<T: Copy + Default> PrimitiveArray<T> {
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            values: vec![T::default(); capacity].into_boxed_slice(),
            validity: Bitmap::with_capacity(capacity),
            len: 0,
        }
    }

    pub fn from_options(
        capacity: usize,
        values: impl IntoIterator<Item = Option<T>>,
    ) -> Result<Self> {
        let mut array = Self::with_capacity(capacity);
        for value in values {
            array.push(value)?;
        }
        Ok(array)
    }

    pub fn push(&mut self, value: Option<T>) -> Result<()> {
        if self.len == self.capacity() {
            return Err(Error::CapacityExceeded {
                capacity: self.capacity(),
            });
        }
        let valid = value.is_some();
        self.values[self.len] = value.unwrap_or_default();
        self.validity.push(valid)?;
        self.len += 1;
        Ok(())
    }

    pub fn value(&self, row: usize) -> Option<T> {
        self.validity.get(row).then(|| self.values[row])
    }

    pub fn values(&self) -> &[T] {
        &self.values[..self.len]
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn capacity(&self) -> usize {
        self.values.len()
    }

    pub fn null_count(&self) -> usize {
        self.len - self.validity.count_ones()
    }

    pub fn retained_bytes(&self) -> usize {
        self.values.len() * size_of::<T>() + self.validity.retained_bytes()
    }

    pub(crate) fn validity(&self) -> &Bitmap {
        &self.validity
    }
}

pub type Int64Array = PrimitiveArray<i64>;
pub type Float64Array = PrimitiveArray<f64>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BooleanArray {
    values: Bitmap,
    validity: Bitmap,
    len: usize,
}

impl BooleanArray {
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            values: Bitmap::with_capacity(capacity),
            validity: Bitmap::with_capacity(capacity),
            len: 0,
        }
    }

    pub fn from_options(
        capacity: usize,
        values: impl IntoIterator<Item = Option<bool>>,
    ) -> Result<Self> {
        let mut array = Self::with_capacity(capacity);
        for value in values {
            array.push(value)?;
        }
        Ok(array)
    }

    pub fn push(&mut self, value: Option<bool>) -> Result<()> {
        if self.len == self.capacity() {
            return Err(Error::CapacityExceeded {
                capacity: self.capacity(),
            });
        }
        self.values.push(value.unwrap_or(false))?;
        self.validity.push(value.is_some())?;
        self.len += 1;
        Ok(())
    }

    pub fn value(&self, row: usize) -> Option<bool> {
        self.validity.get(row).then(|| self.values.get(row))
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn capacity(&self) -> usize {
        self.values.capacity()
    }

    pub fn null_count(&self) -> usize {
        self.len - self.validity.count_ones()
    }

    pub fn retained_bytes(&self) -> usize {
        self.values.retained_bytes() + self.validity.retained_bytes()
    }

    pub(crate) fn validity(&self) -> &Bitmap {
        &self.validity
    }
}

/// UTF-8 values stored as fixed-width dictionary keys plus one copy per distinct value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DictionaryArray {
    keys: Box<[u32]>,
    dictionary: Box<[Option<Box<str>>]>,
    dictionary_len: usize,
    validity: Bitmap,
    len: usize,
}

impl DictionaryArray {
    pub fn with_capacity(capacity: usize) -> Result<Self> {
        if capacity > u32::MAX as usize {
            return Err(Error::InvalidCapacity { capacity });
        }
        Ok(Self {
            keys: vec![0; capacity].into_boxed_slice(),
            dictionary: (0..capacity)
                .map(|_| None)
                .collect::<Vec<_>>()
                .into_boxed_slice(),
            dictionary_len: 0,
            validity: Bitmap::with_capacity(capacity),
            len: 0,
        })
    }

    pub fn from_options<S: AsRef<str>>(
        capacity: usize,
        values: impl IntoIterator<Item = Option<S>>,
    ) -> Result<Self> {
        let mut array = Self::with_capacity(capacity)?;
        for value in values {
            array.push(value.as_ref().map(AsRef::as_ref))?;
        }
        Ok(array)
    }

    pub(crate) fn from_options_controlled<'a>(
        capacity: usize,
        values: impl IntoIterator<Item = Option<&'a str>>,
        mut check_cancellation: impl FnMut() -> Result<()>,
        mut reserve_string_bytes: impl FnMut(usize) -> Result<()>,
    ) -> Result<Self> {
        let index_slots = Self::build_index_slots(capacity)?;
        let mut array = Self::with_capacity(capacity)?;
        let mut dictionary_index = vec![u32::MAX; index_slots].into_boxed_slice();
        for value in values {
            check_cancellation()?;
            if array.len == capacity {
                return Err(Error::CapacityExceeded { capacity });
            }
            let key = match value {
                None => 0,
                Some(value) => {
                    let mut slot = controlled_string_hash(value, &mut check_cancellation)? as usize
                        & (index_slots - 1);
                    loop {
                        check_cancellation()?;
                        let key = dictionary_index[slot];
                        if key == u32::MAX {
                            reserve_string_bytes(value.len())?;
                            let key = u32::try_from(array.dictionary_len)
                                .map_err(|_| Error::InvalidCapacity { capacity })?;
                            array.dictionary[array.dictionary_len] =
                                Some(clone_string_controlled(value, &mut check_cancellation)?);
                            array.dictionary_len += 1;
                            dictionary_index[slot] = key;
                            break key;
                        }
                        if strings_equal_controlled(
                            array.dictionary[key as usize]
                                .as_deref()
                                .expect("dictionary index points to a populated value"),
                            value,
                            &mut check_cancellation,
                        )? {
                            break key;
                        }
                        slot = (slot + 1) & (index_slots - 1);
                    }
                }
            };
            array.keys[array.len] = key;
            array.validity.push(value.is_some())?;
            array.len += 1;
        }
        Ok(array)
    }

    pub(crate) fn build_workspace_bytes(capacity: usize) -> Result<usize> {
        Self::build_index_slots(capacity)?
            .checked_mul(size_of::<u32>())
            .ok_or(Error::InvalidCapacity { capacity })
    }

    fn build_index_slots(capacity: usize) -> Result<usize> {
        capacity
            .max(1)
            .checked_mul(2)
            .and_then(usize::checked_next_power_of_two)
            .ok_or(Error::InvalidCapacity { capacity })
    }

    pub fn push(&mut self, value: Option<&str>) -> Result<()> {
        if self.len == self.capacity() {
            return Err(Error::CapacityExceeded {
                capacity: self.capacity(),
            });
        }
        let key = match value {
            None => 0,
            Some(value) => match self.dictionary[..self.dictionary_len]
                .iter()
                .position(|candidate| candidate.as_deref() == Some(value))
            {
                Some(key) => key as u32,
                None => {
                    let key = self.dictionary_len;
                    self.dictionary[key] = Some(value.into());
                    self.dictionary_len += 1;
                    key as u32
                }
            },
        };
        self.keys[self.len] = key;
        self.validity.push(value.is_some())?;
        self.len += 1;
        Ok(())
    }

    pub fn value(&self, row: usize) -> Option<&str> {
        if !self.validity.get(row) {
            return None;
        }
        self.dictionary[self.keys[row] as usize].as_deref()
    }

    pub fn key(&self, row: usize) -> Option<u32> {
        self.validity.get(row).then(|| self.keys[row])
    }

    pub fn dictionary(&self) -> impl ExactSizeIterator<Item = &str> {
        self.dictionary[..self.dictionary_len]
            .iter()
            .map(|value| value.as_deref().expect("populated dictionary slot"))
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn capacity(&self) -> usize {
        self.keys.len()
    }

    pub fn null_count(&self) -> usize {
        self.len - self.validity.count_ones()
    }

    pub fn retained_bytes(&self) -> usize {
        self.keys.len() * size_of::<u32>()
            + self.dictionary.len() * size_of::<Option<Box<str>>>()
            + self.dictionary().map(str::len).sum::<usize>()
            + self.validity.retained_bytes()
    }

    pub(crate) fn validity(&self) -> &Bitmap {
        &self.validity
    }
}

const CONTROLLED_STRING_CHUNK_BYTES: usize = 64 * 1024;

fn controlled_string_hash(
    value: &str,
    check_cancellation: &mut impl FnMut() -> Result<()>,
) -> Result<u64> {
    let mut hasher = DefaultHasher::new();
    hasher.write_usize(value.len());
    for chunk in value.as_bytes().chunks(CONTROLLED_STRING_CHUNK_BYTES) {
        check_cancellation()?;
        hasher.write(chunk);
    }
    Ok(hasher.finish())
}

fn strings_equal_controlled(
    left: &str,
    right: &str,
    check_cancellation: &mut impl FnMut() -> Result<()>,
) -> Result<bool> {
    if left.len() != right.len() {
        return Ok(false);
    }
    for (left, right) in left
        .as_bytes()
        .chunks(CONTROLLED_STRING_CHUNK_BYTES)
        .zip(right.as_bytes().chunks(CONTROLLED_STRING_CHUNK_BYTES))
    {
        check_cancellation()?;
        if left != right {
            return Ok(false);
        }
    }
    Ok(true)
}

fn clone_string_controlled(
    value: &str,
    check_cancellation: &mut impl FnMut() -> Result<()>,
) -> Result<Box<str>> {
    let mut clone = Vec::with_capacity(value.len());
    for chunk in value.as_bytes().chunks(CONTROLLED_STRING_CHUNK_BYTES) {
        check_cancellation()?;
        clone.extend_from_slice(chunk);
    }
    Ok(String::from_utf8(clone)
        .expect("copying an existing UTF-8 string preserves validity")
        .into_boxed_str())
}

#[derive(Debug, Clone, PartialEq)]
pub enum Column {
    Int64(Int64Array),
    Float64(Float64Array),
    Boolean(BooleanArray),
    String(DictionaryArray),
}

impl Column {
    pub const fn data_type(&self) -> DataType {
        match self {
            Self::Int64(_) => DataType::Int64,
            Self::Float64(_) => DataType::Float64,
            Self::Boolean(_) => DataType::Boolean,
            Self::String(_) => DataType::String,
        }
    }

    pub fn len(&self) -> usize {
        match self {
            Self::Int64(array) => array.len(),
            Self::Float64(array) => array.len(),
            Self::Boolean(array) => array.len(),
            Self::String(array) => array.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn capacity(&self) -> usize {
        match self {
            Self::Int64(array) => array.capacity(),
            Self::Float64(array) => array.capacity(),
            Self::Boolean(array) => array.capacity(),
            Self::String(array) => array.capacity(),
        }
    }

    pub fn null_count(&self) -> usize {
        match self {
            Self::Int64(array) => array.null_count(),
            Self::Float64(array) => array.null_count(),
            Self::Boolean(array) => array.null_count(),
            Self::String(array) => array.null_count(),
        }
    }

    pub fn retained_bytes(&self) -> usize {
        match self {
            Self::Int64(array) => array.retained_bytes(),
            Self::Float64(array) => array.retained_bytes(),
            Self::Boolean(array) => array.retained_bytes(),
            Self::String(array) => array.retained_bytes(),
        }
    }

    pub(crate) fn validity(&self) -> &Bitmap {
        match self {
            Self::Int64(array) => array.validity(),
            Self::Float64(array) => array.validity(),
            Self::Boolean(array) => array.validity(),
            Self::String(array) => array.validity(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BatchConfig {
    pub capacity: usize,
    pub memory_limit_bytes: usize,
}

impl BatchConfig {
    pub const fn new(capacity: usize, memory_limit_bytes: usize) -> Self {
        Self {
            capacity,
            memory_limit_bytes,
        }
    }

    pub const fn unlimited(capacity: usize) -> Self {
        Self::new(capacity, usize::MAX)
    }
}

/// A query-independent collection of equal-length, typed column arrays.
#[derive(Debug, Clone, PartialEq)]
pub struct RecordBatch {
    schema: Schema,
    columns: Box<[Column]>,
    selection: SelectionMask,
    len: usize,
    config: BatchConfig,
    retained_bytes: usize,
}

impl RecordBatch {
    pub fn try_new(schema: Schema, columns: Vec<Column>, config: BatchConfig) -> Result<Self> {
        if schema.len() != columns.len() {
            return Err(Error::SchemaMismatch {
                fields: schema.len(),
                columns: columns.len(),
            });
        }

        let len = columns.first().map_or(0, Column::len);
        for (index, (field, column)) in schema.fields().iter().zip(&columns).enumerate() {
            if column.capacity() != config.capacity {
                return Err(Error::CapacityMismatch {
                    column: index,
                    expected: config.capacity,
                    actual: column.capacity(),
                });
            }
            if column.len() != len {
                return Err(Error::LengthMismatch {
                    column: index,
                    expected: len,
                    actual: column.len(),
                });
            }
            if field.data_type() != column.data_type() {
                return Err(Error::BatchTypeMismatch {
                    column: index,
                    expected: field.data_type().name(),
                    actual: column.data_type().name(),
                });
            }
            if !field.nullable() && column.null_count() != 0 {
                return Err(Error::NullInNonNullableColumn { column: index });
            }
        }
        if len > config.capacity {
            return Err(Error::CapacityExceeded {
                capacity: config.capacity,
            });
        }

        let memory_overflow = || Error::MemoryLimitExceeded {
            operator: "record batch",
            required: usize::MAX,
            limit: config.memory_limit_bytes,
        };
        let column_container_bytes = columns
            .len()
            .checked_mul(size_of::<Column>())
            .ok_or_else(memory_overflow)?;
        let column_payload_bytes = columns.iter().try_fold(0_usize, |total, column| {
            total.checked_add(column.retained_bytes())
        });
        let column_payload_bytes = column_payload_bytes.ok_or_else(memory_overflow)?;
        let selection_bytes = config
            .capacity
            .div_ceil(BITS_PER_WORD)
            .checked_mul(size_of::<u64>())
            .ok_or_else(memory_overflow)?;
        let retained_bytes = schema
            .retained_bytes()
            .checked_add(column_container_bytes)
            .and_then(|bytes| bytes.checked_add(column_payload_bytes))
            .and_then(|bytes| bytes.checked_add(selection_bytes))
            .ok_or_else(memory_overflow)?;
        if retained_bytes > config.memory_limit_bytes {
            return Err(Error::MemoryLimitExceeded {
                operator: "record batch",
                required: retained_bytes,
                limit: config.memory_limit_bytes,
            });
        }

        let columns = columns.into_boxed_slice();
        let selection = SelectionMask::all(len, config.capacity)?;
        debug_assert_eq!(selection.retained_bytes(), selection_bytes);

        Ok(Self {
            schema,
            columns,
            selection,
            len,
            config,
            retained_bytes,
        })
    }

    pub fn schema(&self) -> &Schema {
        &self.schema
    }

    pub fn columns(&self) -> &[Column] {
        &self.columns
    }

    pub fn column(&self, index: usize) -> Result<&Column> {
        self.columns.get(index).ok_or(Error::InvalidColumn {
            column: index,
            columns: self.columns.len(),
        })
    }

    pub fn selection(&self) -> &SelectionMask {
        &self.selection
    }

    pub fn replace_selection(&mut self, selection: SelectionMask) -> Result<()> {
        selection.validate_shape(self.len, self.capacity())?;
        self.selection = selection;
        Ok(())
    }

    pub fn reset_selection(&mut self) {
        self.selection = SelectionMask::all(self.len, self.capacity())
            .expect("batch length was validated against capacity");
    }

    pub const fn len(&self) -> usize {
        self.len
    }

    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub const fn capacity(&self) -> usize {
        self.config.capacity
    }

    pub const fn memory_limit_bytes(&self) -> usize {
        self.config.memory_limit_bytes
    }

    /// Exact bytes in heap allocations retained by this batch, excluding allocator metadata.
    pub const fn retained_bytes(&self) -> usize {
        self.retained_bytes
    }
}
