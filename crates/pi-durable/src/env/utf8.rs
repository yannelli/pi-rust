//! UTF-8 validation with the answer of `std::str::from_utf8(bytes).is_ok()`, faster on long non-ASCII text where the
//! compiler turns a comparison kernel into vector code. The first long input times the kernel against the standard
//! library, and the faster one validates from then on, so a build or a machine where the kernel is slow keeps `std`.

// pi-env-daemon compiles this file through `#[path]` without the rest of pi-durable: use only std paths here.
// `cargo xtask utf8-asm` fails when a release target compiles the kernel's loops to scalar code.

/// Whether `bytes` are valid UTF-8: `std::str::from_utf8(bytes).is_ok()`.
pub fn is_utf8(bytes: &[u8]) -> bool {
    // The standard library is faster on shorter inputs.
    #[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
    if bytes.len() >= 64 {
        return kernel::chosen()(bytes);
    }
    std_is_utf8(bytes)
}

fn std_is_utf8(bytes: &[u8]) -> bool {
    std::str::from_utf8(bytes).is_ok()
}

/// Only these architectures have been checked for vector code; other targets always use `std`.
#[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
mod kernel {
    use std::hint::black_box;
    use std::sync::OnceLock;
    use std::time::{Duration, Instant};

    type Validator = fn(&[u8]) -> bool;

    /// The faster of the kernel and the standard library in this build on this machine, timed at the first call.
    pub(super) fn chosen() -> Validator {
        static CHOSEN: OnceLock<Validator> = OnceLock::new();
        *CHOSEN.get_or_init(|| if is_faster_than_std() { is_utf8 } else { super::std_is_utf8 })
    }

    /// Whether the kernel beats the standard library on 64 KiB of three- and four-byte text, where the kernel runs its
    /// longest rules. Each side keeps its best of three runs, since a preempted run is only ever slower.
    fn is_faster_than_std() -> bool {
        let unit = "日本語のテキスト😀";
        let sample = unit.repeat(64 * 1024 / unit.len());
        let mut fastest = [Duration::MAX; 2];
        for _ in 0..3 {
            for (fastest, validator) in fastest.iter_mut().zip([is_utf8 as Validator, super::std_is_utf8]) {
                let start = Instant::now();
                black_box(validator(black_box(sample.as_bytes())));
                *fastest = (*fastest).min(start.elapsed());
            }
        }
        fastest[0] < fastest[1]
    }

    /// One exit test and one choice of rules per this many bytes.
    const BLOCK: usize = 1024;

    /// `std::str::from_utf8(bytes).is_ok()` from byte comparisons that the compiler turns into vector code.
    pub(super) fn is_utf8(bytes: &[u8]) -> bool {
        let n = bytes.len();
        if n < 4 {
            return super::std_is_utf8(bytes);
        }
        let starts_badly = breaks::<4>(bytes[0], 0, 0, 0)
            | breaks::<4>(bytes[1], bytes[0], 0, 0)
            | breaks::<4>(bytes[2], bytes[1], bytes[0], 0);
        let ends_inside = (bytes[n - 1] >= 0xc0) | (bytes[n - 2] >= 0xe0) | (bytes[n - 3] >= 0xf0);
        if starts_badly | ends_inside {
            return false;
        }
        // Byte `c[i]` follows `p1[i]`, `p2[i]`, and `p3[i]`.
        let (c, p1, p2, p3) = (&bytes[3..], &bytes[2..n - 1], &bytes[1..n - 2], &bytes[..n - 3]);
        let blocks = c.chunks(BLOCK).zip(p1.chunks(BLOCK)).zip(p2.chunks(BLOCK)).zip(p3.chunks(BLOCK));
        for (((c, p1), p2), p3) in blocks {
            // The largest byte of the block and of the three bytes before it says which rules can fire.
            let largest = c.iter().fold(p1[0].max(p2[0]).max(p3[0]), |largest, byte| largest.max(*byte));
            let broken = match largest {
                0..0x80 => false,
                0x80..0xe0 => block_breaks::<2>(c, p1, p2, p3),
                0xe0..0xf0 => block_breaks::<3>(c, p1, p2, p3),
                _ => block_breaks::<4>(c, p1, p2, p3),
            };
            if broken {
                return false;
            }
        }
        true
    }

    /// Whether a byte of the block breaks a rule, when no byte leads a character longer than `WIDTH` bytes. Not
    /// inlined, so each rule set is a function of its own that `cargo xtask utf8-asm` can find; the call per block
    /// costs nothing measurable.
    #[inline(never)]
    fn block_breaks<const WIDTH: usize>(c: &[u8], p1: &[u8], p2: &[u8], p3: &[u8]) -> bool {
        let mut broken = false;
        for (((c, p1), p2), p3) in c.iter().zip(p1).zip(p2).zip(p3) {
            broken |= breaks::<WIDTH>(*c, *p1, *p2, *p3);
        }
        broken
    }

    /// Whether byte `c` cannot follow `p3 p2 p1` in valid UTF-8, when none of them leads a character longer than
    /// `WIDTH` bytes. A bad lead byte (c0, c1, f5..ff) is reported on the byte after it.
    #[inline(always)]
    fn breaks<const WIDTH: usize>(c: u8, p1: u8, p2: u8, p3: u8) -> bool {
        let continuation = (c as i8) < -64;
        let mut demand = p1;
        if WIDTH >= 3 {
            demand = demand.max(p2.saturating_sub(0x20));
        }
        if WIDTH >= 4 {
            demand = demand.max(p3.saturating_sub(0x30));
        }
        let mut broken = (continuation ^ (demand >= 0xc0)) | ((p1 & 0xfe) == 0xc0);
        if WIDTH >= 3 {
            // e0 takes a0..bf next and ed takes 80..9f.
            broken |= p1 == if (c as i8) < -96 { 0xe0 } else { 0xed };
        }
        if WIDTH >= 4 {
            // f0 takes 90..bf next and f4 takes 80..8f; f5..ff start nothing.
            broken |= (p1 == if (c as i8) < -112 { 0xf0 } else { 0xf4 }) | (p1 > 0xf4);
        }
        broken
    }
}

#[cfg(all(test, any(target_arch = "aarch64", target_arch = "x86_64")))]
#[path = "utf8_tests.rs"]
mod tests;
