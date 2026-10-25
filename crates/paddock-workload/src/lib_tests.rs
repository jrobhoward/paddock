#![allow(non_snake_case)]

use super::*;

#[test]
fn run____every_workload____same_nonzero_checksum_each_time() {
    let inputs = Inputs::new();
    for name in WORKLOADS {
        let first = run(&inputs, name, 1).unwrap();
        assert_ne!(first, 0, "{name}");
        assert_eq!(run(&Inputs::new(), name, 1), Some(first), "{name}");
        assert_eq!(run(&inputs, name, 2), Some(first.wrapping_mul(2)), "{name}");
    }
}

#[test]
fn run____unknown_name____none() {
    assert_eq!(run(&Inputs::new(), "nope", 1), None);
}

#[test]
fn regex____every_log_line____matches() {
    let inputs = Inputs::new();
    assert_eq!(inputs.log_line.find_iter(&inputs.log).count(), LOG_LINES);
}
