//! Port of upstream `test/framing.test.ts`.

use pi_protocol::{FrameDecoder, FrameDecoderOptions, encode_frame};

fn frame(payload: &[u8]) -> Vec<u8> {
    encode_frame(payload).unwrap()
}

#[test]
fn prefixes_payloads_with_a_four_byte_big_endian_length() {
    assert_eq!(frame(&[0xaa, 0xbb, 0xcc]), vec![0x00, 0x00, 0x00, 0x03, 0xaa, 0xbb, 0xcc]);
    assert_eq!(frame(&[]), vec![0, 0, 0, 0]);
}

#[test]
fn decodes_fragmented_coalesced_and_empty_frames_in_order() {
    let wire = [frame(&[1, 2, 3]), frame(&[]), frame(&[4])].concat();
    let mut decoder = FrameDecoder::default();
    let mut frames = Vec::new();
    for byte in &wire {
        frames.extend(decoder.push(std::slice::from_ref(byte)).unwrap());
    }
    decoder.end().unwrap();
    assert_eq!(frames, vec![vec![1, 2, 3], vec![], vec![4]]);

    let mut coalesced = FrameDecoder::default();
    assert_eq!(coalesced.push(&wire).unwrap(), frames);
    coalesced.end().unwrap();
}

#[test]
fn assembles_payloads_spanning_multiple_internal_blocks() {
    let payload: Vec<u8> = (0..70_000).map(|index| (index % 251) as u8).collect();
    let wire = frame(&payload);
    let mut decoder = FrameDecoder::default();
    let mut frames = decoder.push(&wire[..101]).unwrap();
    frames.extend(decoder.push(&wire[101..65_541]).unwrap());
    frames.extend(decoder.push(&wire[65_541..]).unwrap());
    decoder.end().unwrap();
    assert_eq!(frames, vec![payload]);
}

#[test]
fn handles_every_split_point_across_a_frame() {
    let wire = frame(&[10, 20, 30, 40]);
    for split in 0..=wire.len() {
        let mut decoder = FrameDecoder::default();
        let mut frames = decoder.push(&wire[..split]).unwrap();
        frames.extend(decoder.push(&wire[split..]).unwrap());
        decoder.end().unwrap();
        assert_eq!(frames, vec![vec![10, 20, 30, 40]]);
    }
}

#[test]
fn copies_payload_bytes_instead_of_aliasing_input() {
    let mut chunk = frame(&[1, 2, 3]);
    let mut decoder = FrameDecoder::default();
    let frames = decoder.push(&chunk).unwrap();
    chunk.fill(9);
    assert_eq!(frames, vec![vec![1, 2, 3]]);
}

#[test]
fn accepts_empty_chunks_and_a_clean_empty_stream() {
    let mut decoder = FrameDecoder::default();
    assert!(decoder.push(&[]).unwrap().is_empty());
    decoder.end().unwrap();
}

#[test]
fn rejects_a_truncated_stream_at_end() {
    for wire in [&[0u8, 0, 0][..], &[0, 0, 0, 2, 1][..]] {
        let mut decoder = FrameDecoder::default();
        assert!(decoder.push(wire).unwrap().is_empty());
        assert!(decoder.end().is_err());
    }
}

#[test]
fn rejects_an_oversized_declared_length_once_its_header_is_complete() {
    let mut decoder = FrameDecoder::new(FrameDecoderOptions::with_max_frame_length(3));
    assert!(decoder.push(&[0, 0, 0, 4]).unwrap_err().0.contains("limit"));
    assert!(decoder.push(&[1]).unwrap_err().0.contains("failed"));
}

#[test]
fn accepts_a_frame_exactly_at_the_configured_maximum() {
    let mut decoder = FrameDecoder::new(FrameDecoderOptions::with_max_frame_length(3));
    assert_eq!(decoder.push(&frame(&[1, 2, 3])).unwrap(), vec![vec![1, 2, 3]]);
    decoder.end().unwrap();
}

#[test]
fn cannot_be_pushed_after_end() {
    let mut decoder = FrameDecoder::default();
    decoder.end().unwrap();
    assert!(decoder.push(&[]).unwrap_err().0.contains("ended"));
    assert!(decoder.end().unwrap_err().0.contains("ended"));
}
