#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::time::Duration;

use super::*;

#[cfg(target_os = "linux")]
#[rstest::rstest]
#[case("2345 678 90 1 0 400 0\n", Some(678))]
#[case("2345", None)]
#[case("", None)]
#[case("2345 many 90", None)]
fn parse_resident____statm_line____its_second_field(
    #[case] statm: &str,
    #[case] expected: Option<u64>,
) {
    assert_eq!(linux::parse_resident(statm), expected);
}

#[test]
fn process_bytes____a_process_group_leader____its_resident_set() {
    // A test binary may not lead its process group (FreeBSD reads the
    // group), so a child that does: `sleep`, in a group of its own.
    use std::os::unix::process::CommandExt;

    let mut child = std::process::Command::new("sleep")
        .arg("5")
        .process_group(0)
        .spawn()
        .unwrap();
    let mut bytes = 0;
    for _ in 0..50 {
        bytes = process_bytes(child.id());
        if bytes > 0 {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    child.kill().unwrap();
    child.wait().unwrap();

    assert!(bytes > 0);
}

#[test]
fn process_bytes____gone____zero() {
    assert_eq!(process_bytes(u32::MAX), 0);
}

fn pair() -> (TcpStream, TcpStream) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    let (server, _) = listener.accept().unwrap();
    (client, server)
}

#[test]
fn tcp_quiet____no_data_for_a_while____at_least_that_long() {
    let (_client, server) = pair();
    let mut quiet = TcpQuiet::new();
    let _ = quiet.quiet(&server).unwrap();

    std::thread::sleep(Duration::from_millis(300));
    let found = quiet.quiet(&server).unwrap();

    assert!(found >= Duration::from_millis(200), "{found:?}");
}

#[test]
fn tcp_quiet____data_sent____quiet_again() {
    let (mut client, mut server) = pair();
    let mut quiet = TcpQuiet::new();
    let _ = quiet.quiet(&server).unwrap();
    std::thread::sleep(Duration::from_millis(300));

    server.write_all(b"x").unwrap();
    let mut b = [0u8; 1];
    client.read_exact(&mut b).unwrap();
    let found = quiet.quiet(&server).unwrap();

    assert!(found < Duration::from_millis(200), "{found:?}");
}

#[test]
fn tcp_quiet____data_received____quiet_again() {
    let (mut client, mut server) = pair();
    let mut quiet = TcpQuiet::new();
    let _ = quiet.quiet(&server).unwrap();
    std::thread::sleep(Duration::from_millis(300));

    client.write_all(b"x").unwrap();
    let mut b = [0u8; 1];
    server.read_exact(&mut b).unwrap();
    let found = quiet.quiet(&server).unwrap();

    assert!(found < Duration::from_millis(200), "{found:?}");
}
