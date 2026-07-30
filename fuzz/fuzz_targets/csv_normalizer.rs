#![no_main]

use libfuzzer_sys::fuzz_target;

#[path = "../../benchmark/normalize.rs"]
mod normalize;

use normalize::{ColumnType, compare_outputs};

const MAX_INPUT_BYTES: usize = 64 * 1024;

fuzz_target!(|data: &[u8]| {
    if data.len() > MAX_INPUT_BYTES {
        return;
    }
    if let Ok(input) = std::str::from_utf8(data) {
        let columns = [
            ("integer", ColumnType::Integer),
            ("float", ColumnType::Float),
            ("boolean", ColumnType::Boolean),
            ("string", ColumnType::String),
        ];
        let _ = compare_outputs(input, input, &columns);
    }
});
