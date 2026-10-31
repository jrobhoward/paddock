#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use super::*;
use proptest::prelude::*;
use rstest::rstest;

#[test]
fn crc16____check_string____known_value() {
    let crc = crc16(b"123456789");

    assert_eq!(crc, 0x29B1);
}

#[rstest]
#[case::empty(&[], &[0x01])]
#[case::one_zero(&[0x00], &[0x01, 0x01])]
#[case::zeros_between(&[0x11, 0x00, 0x00, 0x22], &[0x02, 0x11, 0x01, 0x02, 0x22])]
#[case::no_zero(&[0x11, 0x22, 0x33], &[0x04, 0x11, 0x22, 0x33])]
fn cobs_encode____examples____match_the_published_ones(#[case] raw: &[u8], #[case] encoded: &[u8]) {
    let out = cobs_encode(raw);

    assert_eq!(out, encoded);
}

proptest! {
    #[test]
    fn cobs____any_bytes____round_trip_without_zeros(raw in proptest::collection::vec(any::<u8>(), 0..1200)) {
        let encoded = cobs_encode(&raw);

        prop_assert!(!encoded.contains(&0));
        prop_assert_eq!(cobs_decode(&encoded), Some(raw));
    }

    #[test]
    fn frame____any_payload____decodes_to_itself(
        from in any::<u32>(),
        to in any::<u32>(),
        payload in proptest::collection::vec(any::<u8>(), 0..=MAX_PAYLOAD),
    ) {
        let bytes = encode(from, to, &payload).unwrap();

        let frames = Decoder::default().push(&bytes);

        prop_assert_eq!(frames, vec![Frame { from, to, payload }]);
    }

    #[test]
    fn decoder____noise_then_a_frame____finds_the_frame(noise in proptest::collection::vec(any::<u8>(), 0..300)) {
        let mut stream = noise;
        stream.push(0);
        stream.extend(encode(1, 2, b"beat").unwrap());

        let frames = Decoder::default().push(&stream);

        prop_assert_eq!(frames.last(), Some(&Frame { from: 1, to: 2, payload: b"beat".to_vec() }));
    }
}

#[test]
fn decoder____frame_split_across_reads____decoded_once_complete() {
    let bytes = encode(7, 9, b"hello").unwrap();
    let mut d = Decoder::default();

    let first = d.push(&bytes[..4]);
    let second = d.push(&bytes[4..]);

    assert!(first.is_empty());
    assert_eq!(second.len(), 1);
    assert_eq!(second[0].payload, b"hello");
}

#[test]
fn decoder____one_bit_flipped____frame_dropped() {
    let mut bytes = encode(7, 9, b"hello").unwrap();
    bytes[6] ^= 0x04;

    let frames = Decoder::default().push(&bytes);

    assert!(frames.is_empty());
}

#[test]
fn decoder____overlong_run____skipped_until_the_next_delimiter() {
    let mut stream = vec![0x55; MAX_ENCODED + 10];
    stream.push(0);
    stream.extend(encode(1, 2, b"after").unwrap());

    let frames = Decoder::default().push(&stream);

    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0].payload, b"after");
}

#[test]
fn encode____payload_over_the_limit____refused() {
    let payload = vec![1; MAX_PAYLOAD + 1];

    let bytes = encode(1, 2, &payload);

    assert!(bytes.is_none());
}
