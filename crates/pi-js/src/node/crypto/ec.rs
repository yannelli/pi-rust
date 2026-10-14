//! Short Weierstrass curves (P-256, P-384, P-521, secp256k1) for ECDSA and ECDH, and X25519.

use num_bigint::BigUint;
use num_traits::{One, Zero};

use super::bigint::{from_hex, random_below, to_fixed};
use crate::error::JsError;

pub(super) struct Curve {
    /// Node's name (`getCurves`), the JWK name, and the OID.
    pub name: &'static str,
    pub jwk: &'static str,
    pub oid: &'static str,
    p: &'static str,
    a: &'static str,
    b: &'static str,
    gx: &'static str,
    gy: &'static str,
    n: &'static str,
}

pub(super) const CURVES: &[Curve] = &[
    Curve {
        name: "prime256v1",
        jwk: "P-256",
        oid: "1.2.840.10045.3.1.7",
        p: "ffffffff00000001000000000000000000000000ffffffffffffffffffffffff",
        a: "ffffffff00000001000000000000000000000000fffffffffffffffffffffffc",
        b: "5ac635d8aa3a93e7b3ebbd55769886bc651d06b0cc53b0f63bce3c3e27d2604b",
        gx: "6b17d1f2e12c4247f8bce6e563a440f277037d812deb33a0f4a13945d898c296",
        gy: "4fe342e2fe1a7f9b8ee7eb4a7c0f9e162bce33576b315ececbb6406837bf51f5",
        n: "ffffffff00000000ffffffffffffffffbce6faada7179e84f3b9cac2fc632551",
    },
    Curve {
        name: "secp384r1",
        jwk: "P-384",
        oid: "1.3.132.0.34",
        p: "fffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffeffffffff0000000000000000ffffffff",
        a: "fffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffeffffffff0000000000000000fffffffc",
        b: "b3312fa7e23ee7e4988e056be3f82d19181d9c6efe8141120314088f5013875ac656398d8a2ed19d2a85c8edd3ec2aef",
        gx: "aa87ca22be8b05378eb1c71ef320ad746e1d3b628ba79b9859f741e082542a385502f25dbf55296c3a545e3872760ab7",
        gy: "3617de4a96262c6f5d9e98bf9292dc29f8f41dbd289a147ce9da3113b5f0b8c00a60b1ce1d7e819d7a431d7c90ea0e5f",
        n: "ffffffffffffffffffffffffffffffffffffffffffffffffc7634d81f4372ddf581a0db248b0a77aecec196accc52973",
    },
    Curve {
        name: "secp521r1",
        jwk: "P-521",
        oid: "1.3.132.0.35",
        p: "01ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff",
        a: "01fffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffc",
        b: "0051953eb9618e1c9a1f929a21a0b68540eea2da725b99b315f3b8b489918ef109e156193951ec7e937b1652c0bd3bb1bf073573df883d2c34f1ef451fd46b503f00",
        gx: "00c6858e06b70404e9cd9e3ecb662395b4429c648139053fb521f828af606b4d3dbaa14b5e77efe75928fe1dc127a2ffa8de3348b3c1856a429bf97e7e31c2e5bd66",
        gy: "011839296a789a3bc0045c8a5fb42c7d1bd998f54449579b446817afbd17273e662c97ee72995ef42640c550b9013fad0761353c7086a272c24088be94769fd16650",
        n: "01fffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffa51868783bf2f966b7fcc0148f709a5d03bb5c9b8899c47aebb6fb71e91386409",
    },
    Curve {
        name: "secp256k1",
        jwk: "secp256k1",
        oid: "1.3.132.0.10",
        p: "fffffffffffffffffffffffffffffffffffffffffffffffffffffffefffffc2f",
        a: "00",
        b: "07",
        gx: "79be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798",
        gy: "483ada7726a3c4655da4fbfc0e1108a8fd17b448a68554199c47d08ffb10d4b8",
        n: "fffffffffffffffffffffffffffffffebaaedce6af48a03bbfd25e8cd0364141",
    },
];

/// Find a curve by Node name, alias, JWK name, or OID.
pub(super) fn curve(name: &str) -> Option<&'static Curve> {
    let name = match name {
        "P-256" | "secp256r1" => "prime256v1",
        "P-384" => "secp384r1",
        "P-521" => "secp521r1",
        other => other,
    };
    CURVES.iter().find(|curve| curve.name == name || curve.jwk == name || curve.oid == name)
}

pub(super) fn invalid_curve() -> JsError {
    JsError::type_error("Invalid EC curve name").with_code("ERR_CRYPTO_INVALID_CURVE")
}

fn invalid_point() -> JsError {
    JsError::new("Public key is not valid for specified curve").with_code("ERR_CRYPTO_ECDH_INVALID_PUBLIC_KEY")
}

/// Jacobian point; `z == 0` is the point at infinity.
#[derive(Clone)]
struct Jacobian {
    x: BigUint,
    y: BigUint,
    z: BigUint,
}

pub(super) struct Params {
    pub p: BigUint,
    a: BigUint,
    b: BigUint,
    pub n: BigUint,
    g: (BigUint, BigUint),
    /// Byte length of a field element.
    pub size: usize,
}

impl Curve {
    pub(super) fn params(&self) -> Params {
        let p = from_hex(self.p);
        let size = p.bits().div_ceil(8) as usize;
        Params {
            a: from_hex(self.a),
            b: from_hex(self.b),
            n: from_hex(self.n),
            g: (from_hex(self.gx), from_hex(self.gy)),
            p,
            size,
        }
    }
}

impl Params {
    fn sub(&self, a: &BigUint, b: &BigUint) -> BigUint {
        (a + &self.p - b % &self.p) % &self.p
    }

    fn double(&self, point: &Jacobian) -> Jacobian {
        let p = &self.p;
        if point.z.is_zero() || point.y.is_zero() {
            return Jacobian { x: BigUint::one(), y: BigUint::one(), z: BigUint::zero() };
        }
        let y2 = &point.y * &point.y % p;
        let s = BigUint::from(4u8) * &point.x * &y2 % p;
        let z2 = &point.z * &point.z % p;
        let m = (BigUint::from(3u8) * &point.x * &point.x + &self.a * &z2 * &z2) % p;
        let x = self.sub(&(&m * &m), &(BigUint::from(2u8) * &s));
        let y = self.sub(&(&m * self.sub(&s, &x)), &(BigUint::from(8u8) * &y2 * &y2));
        let z = BigUint::from(2u8) * &point.y * &point.z % p;
        Jacobian { x, y, z }
    }

    fn add(&self, one: &Jacobian, two: &Jacobian) -> Jacobian {
        let p = &self.p;
        if one.z.is_zero() {
            return two.clone();
        }
        if two.z.is_zero() {
            return one.clone();
        }
        let z1s = &one.z * &one.z % p;
        let z2s = &two.z * &two.z % p;
        let u1 = &one.x * &z2s % p;
        let u2 = &two.x * &z1s % p;
        let s1 = &one.y * &z2s * &two.z % p;
        let s2 = &two.y * &z1s * &one.z % p;
        if u1 == u2 {
            return if s1 == s2 {
                self.double(one)
            } else {
                Jacobian { x: BigUint::one(), y: BigUint::one(), z: BigUint::zero() }
            };
        }
        let h = self.sub(&u2, &u1);
        let r = self.sub(&s2, &s1);
        let h2 = &h * &h % p;
        let h3 = &h2 * &h % p;
        let u1h2 = &u1 * &h2 % p;
        let x = self.sub(&self.sub(&(&r * &r), &h3), &(BigUint::from(2u8) * &u1h2));
        let y = self.sub(&(&r * self.sub(&u1h2, &x)), &(&s1 * &h3));
        let z = h * &one.z * &two.z % p;
        Jacobian { x, y, z }
    }

    fn affine(&self, point: &Jacobian) -> Option<(BigUint, BigUint)> {
        let zinv = point.z.modinv(&self.p)?;
        let zinv2 = &zinv * &zinv % &self.p;
        Some((&point.x * &zinv2 % &self.p, &point.y * &zinv2 * &zinv % &self.p))
    }

    /// `k * point`, or `None` at infinity.
    pub(super) fn mul(&self, k: &BigUint, point: &(BigUint, BigUint)) -> Option<(BigUint, BigUint)> {
        let base = Jacobian { x: point.0.clone(), y: point.1.clone(), z: BigUint::one() };
        let mut acc = Jacobian { x: BigUint::one(), y: BigUint::one(), z: BigUint::zero() };
        for bit in (0..k.bits()).rev() {
            acc = self.double(&acc);
            if k.bit(bit) {
                acc = self.add(&acc, &base);
            }
        }
        self.affine(&acc)
    }

    pub(super) fn mul_base(&self, k: &BigUint) -> Option<(BigUint, BigUint)> {
        self.mul(k, &self.g)
    }

    pub(super) fn on_curve(&self, (x, y): &(BigUint, BigUint)) -> bool {
        let p = &self.p;
        x < p && y < p && y * y % p == (x * x * x + &self.a * x + &self.b) % p
    }

    /// Encode a point as compressed (2), uncompressed (4), or hybrid (6) bytes.
    pub(super) fn encode(&self, (x, y): &(BigUint, BigUint), form: u8) -> Vec<u8> {
        let odd = u8::from(y.bit(0));
        match form {
            2 => [vec![2 | odd], to_fixed(x, self.size)].concat(),
            6 => [vec![6 | odd], to_fixed(x, self.size), to_fixed(y, self.size)].concat(),
            _ => [vec![4], to_fixed(x, self.size), to_fixed(y, self.size)].concat(),
        }
    }

    pub(super) fn decode(&self, bytes: &[u8]) -> Result<(BigUint, BigUint), JsError> {
        let size = self.size;
        let point = match bytes.first() {
            Some(4 | 6 | 7) if bytes.len() == 1 + 2 * size => {
                (BigUint::from_bytes_be(&bytes[1..=size]), BigUint::from_bytes_be(&bytes[1 + size..]))
            }
            Some(&tag @ (2 | 3)) if bytes.len() == 1 + size => {
                let x = BigUint::from_bytes_be(&bytes[1..]);
                let rhs = (&x * &x * &x + &self.a * &x + &self.b) % &self.p;
                let mut y = rhs.modpow(&((&self.p + 1u8) >> 2), &self.p);
                if y.bit(0) != (tag == 3) {
                    y = self.sub(&BigUint::zero(), &y);
                }
                (x, y)
            }
            _ => return Err(invalid_point()),
        };
        if self.on_curve(&point) { Ok(point) } else { Err(invalid_point()) }
    }

    pub(super) fn random_scalar(&self) -> Result<BigUint, JsError> {
        Ok(random_below(&(&self.n - 1u8))? + 1u8)
    }

    pub(super) fn ecdh(&self, d: &BigUint, public: &[u8]) -> Result<Vec<u8>, JsError> {
        let point = self.decode(public)?;
        let shared = self.mul(d, &point).ok_or_else(invalid_point)?;
        Ok(to_fixed(&shared.0, self.size))
    }

    /// Leftmost bits of a digest as an integer below `n`'s bit length.
    fn truncate(&self, digest: &[u8]) -> BigUint {
        let e = BigUint::from_bytes_be(digest);
        let excess = (digest.len() as u64 * 8).saturating_sub(self.n.bits());
        e >> excess
    }

    pub(super) fn sign(&self, d: &BigUint, digest: &[u8]) -> Result<(BigUint, BigUint), JsError> {
        let n = &self.n;
        let e = self.truncate(digest);
        loop {
            let k = self.random_scalar()?;
            let Some((x, _)) = self.mul_base(&k) else { continue };
            let r = x % n;
            let Some(kinv) = k.modinv(n) else { continue };
            let s = kinv * (&e + &r * d) % n;
            if !r.is_zero() && !s.is_zero() {
                return Ok((r, s));
            }
        }
    }

    pub(super) fn verify(&self, public: &(BigUint, BigUint), digest: &[u8], r: &BigUint, s: &BigUint) -> bool {
        let n = &self.n;
        if r.is_zero() || s.is_zero() || r >= n || s >= n {
            return false;
        }
        let Some(w) = s.modinv(n) else { return false };
        let e = self.truncate(digest);
        let left = self.mul_base(&(e * &w % n));
        let right = self.mul(&(r * &w % n), public);
        let lift = |point: Option<(BigUint, BigUint)>| match point {
            Some((x, y)) => Jacobian { x, y, z: BigUint::one() },
            None => Jacobian { x: BigUint::one(), y: BigUint::one(), z: BigUint::zero() },
        };
        match self.affine(&self.add(&lift(left), &lift(right))) {
            Some((x, _)) => &(x % n) == r,
            None => false,
        }
    }
}

/// RFC 7748 X25519 scalar multiplication on little-endian bytes.
pub(super) fn x25519(scalar: &[u8], u: &[u8]) -> Result<Vec<u8>, JsError> {
    if scalar.len() != 32 || u.len() != 32 {
        return Err(JsError::new("Invalid X25519 key length").with_code("ERR_CRYPTO_INVALID_KEYLEN"));
    }
    let p = (BigUint::one() << 255u32) - 19u8;
    let mut k = scalar.to_vec();
    k[0] &= 248;
    k[31] = (k[31] & 127) | 64;
    let k = BigUint::from_bytes_le(&k);
    let mut u_bytes = u.to_vec();
    u_bytes[31] &= 127;
    let x1 = BigUint::from_bytes_le(&u_bytes) % &p;
    let sub = |a: &BigUint, b: &BigUint| (a + &p - b) % &p;
    let (mut x2, mut z2, mut x3, mut z3) = (BigUint::one(), BigUint::zero(), x1.clone(), BigUint::one());
    let mut swap = false;
    for t in (0..255u64).rev() {
        let bit = k.bit(t);
        if swap != bit {
            std::mem::swap(&mut x2, &mut x3);
            std::mem::swap(&mut z2, &mut z3);
        }
        swap = bit;
        let a = (&x2 + &z2) % &p;
        let aa = &a * &a % &p;
        let b = sub(&x2, &z2);
        let bb = &b * &b % &p;
        let e = sub(&aa, &bb);
        let c = (&x3 + &z3) % &p;
        let d = sub(&x3, &z3);
        let da = d * &a % &p;
        let cb = c * &b % &p;
        let sum = (&da + &cb) % &p;
        x3 = &sum * &sum % &p;
        let diff = sub(&da, &cb);
        z3 = &x1 * (&diff * &diff % &p) % &p;
        x2 = &aa * &bb % &p;
        z2 = e.clone() * ((aa + BigUint::from(121_665u32) * &e) % &p) % &p;
    }
    if swap {
        std::mem::swap(&mut x2, &mut x3);
        std::mem::swap(&mut z2, &mut z3);
    }
    let result = x2 * z2.modpow(&(&p - 2u8), &p) % &p;
    let mut out = result.to_bytes_le();
    out.resize(32, 0);
    if out.iter().all(|byte| *byte == 0) {
        return Err(JsError::new("Failed to perform X25519 key agreement").with_code("ERR_CRYPTO_OPERATION_FAILED"));
    }
    Ok(out)
}

pub(super) fn x25519_public(scalar: &[u8]) -> Result<Vec<u8>, JsError> {
    let mut base = [0u8; 32];
    base[0] = 9;
    x25519(scalar, &base)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    #[test]
    fn curve_parameters_are_consistent() {
        for curve in CURVES {
            let params = curve.params();
            assert!(params.on_curve(&params.g), "{}", curve.name);
            assert!(params.mul_base(&params.n).is_none(), "{}", curve.name);
            let point = params.mul_base(&BigUint::from(12345u32)).unwrap();
            assert!(params.decode(&params.encode(&point, 2)).unwrap() == point);
        }
    }

    #[test]
    fn matches_rfc6979_and_rfc7748_vectors() {
        let params = curve("P-256").unwrap().params();
        let d = from_hex("c9afa9d845ba75166b5c215767b1d6934e50c3db36e89b127b8a622b120f6721");
        let (x, y) = params.mul_base(&d).unwrap();
        assert_eq!(hex(&to_fixed(&x, 32)), "60fed4ba255a9d31c961eb74c6356d68c049b8923b61fa6ce669622e60f29fb6");
        assert_eq!(hex(&to_fixed(&y, 32)), "7903fe1008b8bc99a41ae9e95628bc64f2f1b20c2d7e9f5177a3c294d4462299");
        let digest = ring::digest::digest(&ring::digest::SHA256, b"sample");
        let r = from_hex("efd48b2aacb6a8fd1140dd9cd45e81d69d2c877b56aaf991c34d0ea84eaf3716");
        let s = from_hex("f7cb1c942d657c41d436c7a1b6e29f65f3e900dbb9aff4064dc4ab2f843acda8");
        assert!(params.verify(&(x.clone(), y.clone()), digest.as_ref(), &r, &s));
        let (r2, s2) = params.sign(&d, digest.as_ref()).unwrap();
        assert!(params.verify(&(x, y), digest.as_ref(), &r2, &s2));
        let scalar = from_hex("a546e36bf0527c9d3b16154b82465edd62144c0ac1fc5a18506a2244ba449ac4").to_bytes_be();
        let u = from_hex("e6db6867583030db3594c1a424b15f7c726624ec26b3353b10a903a6d0ab1c4c").to_bytes_be();
        assert_eq!(
            hex(&x25519(&scalar, &u).unwrap()),
            "c3da55379de9c6908e94ea4df28d084f32eccf03491c71f754b4075577a28552"
        );
    }
}
