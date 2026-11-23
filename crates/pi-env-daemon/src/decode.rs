// Share the host decoder without linking the Durable runtime into the remote executable.
#[path = "../../pi-durable/src/env/decode.rs"]
mod durable;
#[path = "../../pi-durable/src/env/utf8.rs"]
mod utf8;

pub(crate) use durable::{StreamDecoder, count_newlines, starts_with_bom};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_newlines_like_a_byte_filter() {
        let mut inputs: Vec<Vec<u8>> = vec![Vec::new(), b"\n".repeat(255 * 8 * 3 + 7), (0..=255).collect()];
        let mut state = 0x9e37_79b9_7f4a_7c15u64;
        for length in 0..600 {
            inputs.push(
                (0..length)
                    .map(|_| {
                        state ^= state << 13;
                        state ^= state >> 7;
                        state ^= state << 17;
                        [b'\n', b'\n' ^ 0x80, b'\n' + 1, b'a', 0xff][(state % 5) as usize]
                    })
                    .collect(),
            );
        }
        for bytes in inputs {
            for start in 0..bytes.len().min(9) {
                let part = &bytes[start..];
                assert_eq!(count_newlines(part), part.iter().filter(|byte| **byte == b'\n').count(), "{part:02x?}");
            }
        }
    }

    #[test]
    fn keeps_a_feff_that_does_not_start_the_stream() {
        let mut decoder = StreamDecoder::new();
        let bytes = [0xe2, 0x82, 0xef, 0xbb, 0xbf, 0x61];
        let text: String = bytes.iter().map(|byte| decoder.decode(&[*byte])).collect::<String>() + &decoder.finish();
        assert_eq!(text, "\u{fffd}\u{feff}a");
    }

    #[test]
    fn drops_only_a_leading_mark_and_never_sniffs_utf16() {
        let mut decoder = StreamDecoder::new();
        assert_eq!(decoder.decode(&[0xef, 0xbb]) + &decoder.decode(&[0xbf, 0x61]) + &decoder.finish(), "a");
        let mut decoder = StreamDecoder::new();
        assert_eq!(decoder.decode(&[0xff, 0xfe, 0x41, 0x00]) + &decoder.finish(), "\u{fffd}\u{fffd}A\u{0}");
    }

    #[test]
    fn replaces_an_incomplete_sequence_once() {
        let mut decoder = StreamDecoder::new();
        assert_eq!(decoder.decode(&[0xe2, 0x82, 0x41]) + &decoder.finish(), "\u{fffd}A");
        let mut decoder = StreamDecoder::new();
        assert_eq!(decoder.decode(&[0xe2, 0x82]) + &decoder.finish(), "\u{fffd}");
    }

    #[test]
    fn every_split_matches_whole_input_decoding() {
        let bytes =
            b"\xef\xbb\xbfa\xc2\xa2\xe2\x82\xac\xf0\x9f\x98\x80\xe0\x80\xed\xa0\x80\xf4\x90\x80\xef\xbb\xbf\xc2";
        for (fresh, skip) in [(StreamDecoder::new(), 3), (StreamDecoder::range(), 0)] {
            let expected = String::from_utf8_lossy(&bytes[skip..]);
            for first in 0..=bytes.len() {
                for second in first..=bytes.len() {
                    let parts = [&bytes[..first], &bytes[first..second], &[], &bytes[second..]];
                    let (mut text, mut owned) = (fresh.clone(), fresh.clone());
                    let decoded: String =
                        parts.iter().map(|part| text.decode(part)).collect::<String>() + &text.finish();
                    let from_vecs: String =
                        parts.iter().map(|part| owned.decode_vec(part.to_vec())).collect::<String>() + &owned.finish();
                    assert_eq!(decoded, expected, "decode split at {first} and {second}");
                    assert_eq!(from_vecs, expected, "decode_vec split at {first} and {second}");
                }
            }
        }
    }

    #[test]
    fn decode_vec_reuses_a_complete_chunk() {
        let mut decoder = StreamDecoder::new();
        for chunk in ["ls -la\n", "日本語 ✓\n"] {
            let bytes = chunk.as_bytes().to_vec();
            let address = bytes.as_ptr();
            let text = decoder.decode_vec(bytes);
            assert_eq!((text.as_str(), text.as_ptr()), (chunk, address));
        }
        let mut decoder = StreamDecoder::new();
        assert_eq!(decoder.decode_vec(b"\xef\xbb\xbfok \xe2\x82".to_vec()), "ok ");
        assert_eq!(decoder.decode_vec(b"\xac".to_vec()) + &decoder.finish(), "\u{20ac}");
    }

    #[test]
    fn range_lengths_match_lossy_text_across_every_split() {
        let bytes = b"\xef\xbb\xbf\xc2\xa2\xe2\x82\xac\xf0\x9f\x98\x80\xed\xa0\x80\xf4\x90\x80\xe2\x82";
        let expected = String::from_utf8_lossy(bytes);
        for split in 0..=bytes.len() {
            let mut text = StreamDecoder::range();
            let mut count = StreamDecoder::range();
            let mut output = String::new();
            for part in [&bytes[..split], &[], &bytes[split..]] {
                let chunk = text.decode(part);
                assert_eq!(count.decoded_len(part), chunk.len());
                output.push_str(&chunk);
            }
            let eof = text.finish();
            assert_eq!(count.finish().len(), eof.len());
            output.push_str(&eof);
            assert_eq!(output, expected, "split at byte {split}");
        }
    }
}
