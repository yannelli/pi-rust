//! Big-integer helpers: random numbers, Miller-Rabin primes, and the RFC 2409/3526 MODP groups.

use num_bigint::BigUint;
use num_traits::{One, Zero};

use super::random_bytes;
use crate::error::JsError;

pub(super) fn from_hex(hex: &str) -> BigUint {
    BigUint::parse_bytes(hex.as_bytes(), 16).unwrap_or_default()
}

/// Big-endian bytes left-padded with zeros to `size`.
pub(super) fn to_fixed(value: &BigUint, size: usize) -> Vec<u8> {
    let bytes = value.to_bytes_be();
    let mut out = vec![0; size.saturating_sub(bytes.len())];
    out.extend(bytes);
    out
}

/// A uniform random integer with at most `bits` bits.
pub(super) fn random_bits(bits: u64) -> Result<BigUint, JsError> {
    let mut bytes = random_bytes(bits.div_ceil(8) as usize)?;
    let excess = bytes.len() as u64 * 8 - bits;
    if let Some(first) = bytes.first_mut() {
        *first &= 0xffu8.checked_shr(excess as u32).unwrap_or(0);
    }
    Ok(BigUint::from_bytes_be(&bytes))
}

/// A uniform random integer in `[0, limit)`.
pub(super) fn random_below(limit: &BigUint) -> Result<BigUint, JsError> {
    loop {
        let value = random_bits(limit.bits())?;
        if &value < limit {
            return Ok(value);
        }
    }
}

fn small_primes() -> Vec<u32> {
    let mut sieve = vec![true; 2048];
    (2..2048)
        .filter(|&n| {
            if sieve[n] {
                (n * n..2048).step_by(n).for_each(|m| sieve[m] = false);
            }
            sieve[n]
        })
        .map(|n| n as u32)
        .collect()
}

/// Trial division and `rounds` Miller-Rabin tests with random bases.
pub(super) fn is_probable_prime(n: &BigUint, rounds: u32) -> Result<bool, JsError> {
    let two = BigUint::from(2u8);
    if n < &two {
        return Ok(false);
    }
    for prime in small_primes() {
        let prime = BigUint::from(prime);
        if n == &prime {
            return Ok(true);
        }
        if (n % &prime).is_zero() {
            return Ok(false);
        }
    }
    let minus_one = n - 1u8;
    let shift = minus_one.trailing_zeros().unwrap_or(0);
    let odd = &minus_one >> shift;
    'rounds: for _ in 0..rounds.max(1) {
        let base = random_below(&(n - 3u8))? + &two;
        let mut x = base.modpow(&odd, n);
        if x.is_one() || x == minus_one {
            continue;
        }
        for _ in 1..shift {
            x = &x * &x % n;
            if x == minus_one {
                continue 'rounds;
            }
        }
        return Ok(false);
    }
    Ok(true)
}

/// Default Miller-Rabin rounds, following OpenSSL's choice by size.
pub(super) fn default_rounds(bits: u64) -> u32 {
    if bits > 2048 { 128 } else { 64 }
}

/// A random prime of exactly `bits` bits, optionally safe or with `prime % add == rem`.
pub(super) fn generate_prime(
    bits: u64,
    safe: bool,
    add: Option<&BigUint>,
    rem: Option<&BigUint>,
) -> Result<BigUint, JsError> {
    if bits < 2 || (safe && bits < 3) {
        return Err(JsError::range_error("invalid number of bits").with_code("ERR_OUT_OF_RANGE"));
    }
    let default_rem = BigUint::from(if safe { 3u8 } else { 1u8 });
    let rounds = default_rounds(bits).min(40);
    loop {
        let mut candidate = random_bits(bits)? | (BigUint::one() << (bits - 1)) | BigUint::one();
        if bits > 2 {
            candidate |= BigUint::one() << (bits - 2);
        }
        if let Some(add) = add {
            let rem = rem.unwrap_or(&default_rem);
            let offset = &candidate % add;
            candidate = candidate - offset + rem;
            if candidate.bits() != bits {
                continue;
            }
        }
        if safe {
            if !candidate.bit(1) {
                continue;
            }
            let half = &candidate >> 1u32;
            if is_probable_prime(&half, rounds)? && is_probable_prime(&candidate, rounds)? {
                return Ok(candidate);
            }
        } else if is_probable_prime(&candidate, rounds)? {
            return Ok(candidate);
        }
    }
}

/// `floor(2^bits * pi)` from Machin's formula.
fn pi_fixed(bits: u64) -> BigUint {
    let guard = 64;
    let atan_inverse = |x: u32| {
        let mut power = (BigUint::one() << (bits + guard)) / x;
        let square = BigUint::from(x * x);
        let (mut positive, mut negative) = (BigUint::zero(), BigUint::zero());
        let mut k = 0u32;
        while !power.is_zero() {
            let term = &power / (2 * k + 1);
            if k % 2 == 0 {
                positive += term;
            } else {
                negative += term;
            }
            power /= &square;
            k += 1;
        }
        positive - negative
    };
    (atan_inverse(5) * 16u8 - atan_inverse(239) * 4u8) >> guard
}

/// The prime of a named MODP group; every group uses generator 2.
pub(super) fn modp_group(name: &str) -> Option<BigUint> {
    let (bits, offset): (u64, u32) = match name {
        "modp1" => (768, 149_686),
        "modp2" => (1024, 129_093),
        "modp5" => (1536, 741_804),
        "modp14" => (2048, 124_476),
        "modp15" => (3072, 1_690_314),
        "modp16" => (4096, 240_904),
        "modp17" => (6144, 929_484),
        "modp18" => (8192, 4_743_158),
        _ => return None,
    };
    let one = BigUint::one();
    let top = (&one << bits) - (&one << (bits - 64)) - 1u8;
    Some(top + ((pi_fixed(bits - 130) + offset) << 64u32))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_primes_and_groups() {
        assert!(is_probable_prime(&BigUint::from(2_147_483_647u32), 10).unwrap());
        assert!(!is_probable_prime(&BigUint::from(2_147_483_649u32), 10).unwrap());
        let prime = generate_prime(64, true, None, None).unwrap();
        assert_eq!(prime.bits(), 64);
        assert!(is_probable_prime(&(prime >> 1u32), 10).unwrap());
        let modp2 = format!("{:x}", modp_group("modp2").unwrap());
        assert!(modp2.starts_with("ffffffffffffffffc90fdaa22168c234c4c6628b80dc1cd1"));
        assert!(modp2.ends_with("49286651ece65381ffffffffffffffff"));
        assert!(is_probable_prime(&modp_group("modp1").unwrap(), 4).unwrap());
    }
}
