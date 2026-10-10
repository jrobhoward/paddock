//! Arbitrary Modbus master requests against served tables of arbitrary
//! sizes. A request answered stays inside its table: a read returns
//! exactly the count asked for, and a write changes only addresses the
//! table has. Needs `--cfg fuzzing` (cargo-fuzz sets it).

#![no_main]

use arbitrary::Arbitrary;
use libfuzzer_sys::fuzz_target;
use paddock_modbus::fuzz_request;

#[derive(Debug, Arbitrary)]
struct Input {
    sizes: [u16; 4],
    function: u8,
    address: u16,
    count: u16,
    values: Vec<u16>,
}

fuzz_target!(|input: Input| {
    let Some((items, written)) = fuzz_request(
        input.sizes,
        input.function,
        input.address,
        input.count,
        &input.values,
    ) else {
        return;
    };
    let function = input.function % 8;
    let size = usize::from(match function {
        0 | 4 | 5 => input.sizes[0],
        1 => input.sizes[1],
        2 | 6 | 7 => input.sizes[2],
        _ => input.sizes[3],
    });
    let start = usize::from(input.address);
    if function < 4 {
        assert_eq!(items, usize::from(input.count));
        assert!(start + items <= size);
        assert!(written.is_empty());
    } else {
        let expected = if matches!(function, 4 | 6) { 1 } else { input.values.len() };
        assert_eq!(items, expected);
        assert_eq!(written.len(), expected);
        assert!(written.iter().all(|&a| usize::from(a) < size));
    }
});
