#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use std::borrow::Cow;

use super::*;

fn tables() -> Tables {
    Tables {
        holding: vec![0; 4],
        input: vec![7; 2],
        coils: vec![false; 8],
        discrete: vec![true; 2],
    }
}

#[test]
fn handle____reads____served_from_the_tables() {
    let mut t = tables();

    let input = handle(&mut t, Request::ReadInputRegisters(0, 2)).unwrap();
    let discrete = handle(&mut t, Request::ReadDiscreteInputs(1, 1)).unwrap();

    assert_eq!(input.0, Response::ReadInputRegisters(vec![7, 7]));
    assert_eq!(discrete.0, Response::ReadDiscreteInputs(vec![true]));
    assert!(input.1.is_empty());
}

#[test]
fn handle____writes____change_the_tables_and_report_points() {
    let mut t = tables();

    let (_, written) = handle(
        &mut t,
        Request::WriteMultipleRegisters(1, Cow::Owned(vec![5, 6])),
    )
    .unwrap();
    handle(&mut t, Request::WriteSingleCoil(3, true)).unwrap();

    assert_eq!(t.holding, vec![0, 5, 6, 0]);
    assert!(t.coils[3]);
    assert_eq!(
        written,
        vec![
            Point {
                table: Table::Holding,
                address: 1,
                value: 5
            },
            Point {
                table: Table::Holding,
                address: 2,
                value: 6
            },
        ]
    );
}

#[test]
fn handle____past_the_table_or_unsupported____exceptions() {
    let mut t = tables();

    let past = handle(&mut t, Request::ReadHoldingRegisters(3, 2));
    let write_past = handle(&mut t, Request::WriteSingleRegister(4, 1));
    let unsupported = handle(&mut t, Request::ReportServerId);

    assert_eq!(past.unwrap_err(), ExceptionCode::IllegalDataAddress);
    assert_eq!(write_past.unwrap_err(), ExceptionCode::IllegalDataAddress);
    assert_eq!(unsupported.unwrap_err(), ExceptionCode::IllegalFunction);
    assert_eq!(t.holding, vec![0; 4]);
}
