//! Reproduces rqs_lib's EC point decoding bug, and measures how often it fires.
use num_bigint::{BigUint, ToBigInt};
use p256::elliptic_curve::sec1::{FromEncodedPoint, ToEncodedPoint};
use p256::{EncodedPoint, PublicKey, SecretKey};

/// rqs_lib's utils.rs::encode_point, verbatim: BigUint -> BigInt ->
/// to_signed_bytes_be. This is also what Java BigInteger.toByteArray() does,
/// which is what the proto says the wire format is.
fn encode_point(unsigned: &[u8]) -> Vec<u8> {
    BigUint::from_bytes_be(unsigned)
        .to_bigint()
        .unwrap()
        .to_signed_bytes_be()
}

/// rqs_lib's rebuild logic from inbound.rs/outbound.rs, verbatim.
fn rebuild_rqs_lib(x: &[u8], y: &[u8]) -> Vec<u8> {
    let mut bytes = vec![0x04];
    if x.len() > 32 {
        bytes.extend_from_slice(&x[x.len() - 32..]);
    } else {
        bytes.extend_from_slice(x);
    }
    if y.len() > 32 {
        bytes.extend_from_slice(&y[y.len() - 32..]);
    } else {
        bytes.extend_from_slice(y);
    }
    bytes
}

/// The three-line fix: left-pad a short coordinate to 32 bytes.
fn rebuild_fixed(x: &[u8], y: &[u8]) -> Vec<u8> {
    fn coord(v: &[u8]) -> [u8; 32] {
        let mut out = [0u8; 32];
        let src = if v.len() > 32 { &v[v.len() - 32..] } else { v };
        out[32 - src.len()..].copy_from_slice(src);
        out
    }
    let mut bytes = vec![0x04];
    bytes.extend_from_slice(&coord(x));
    bytes.extend_from_slice(&coord(y));
    bytes
}

fn main() {
    // 1. Direct reproduction with a key whose x coordinate has a leading zero.
    println!("=== 1. searching for a real P-256 key with a short coordinate ===");
    let mut found = None;
    for attempt in 1..=200_000u32 {
        let sk = SecretKey::random(&mut rand::thread_rng());
        let pk = sk.public_key();
        let point = pk.to_encoded_point(false);
        let x = point.x().unwrap();
        let y = point.y().unwrap();
        let (ex, ey) = (encode_point(x), encode_point(y));
        if ex.len() < 32 || ey.len() < 32 {
            println!("  found after {attempt} keys: wire x={} bytes, y={} bytes", ex.len(), ey.len());
            found = Some((ex, ey));
            break;
        }
    }

    if let Some((ex, ey)) = found {
        let rqs = rebuild_rqs_lib(&ex, &ey);
        println!("  rqs_lib rebuild -> {} bytes (needs 65)", rqs.len());
        match EncodedPoint::from_bytes(&rqs) {
            Ok(p) => println!("  decode: OK (unexpected) valid={}", PublicKey::from_encoded_point(&p).is_some().unwrap_u8() == 1),
            Err(e) => println!("  decode: FAILED -> {e}"),
        }
        let fixed = rebuild_fixed(&ex, &ey);
        println!("  fixed rebuild  -> {} bytes", fixed.len());
        match EncodedPoint::from_bytes(&fixed) {
            Ok(p) => println!(
                "  decode: OK, valid point = {}",
                bool::from(PublicKey::from_encoded_point(&p).is_some())
            ),
            Err(e) => println!("  decode: FAILED -> {e}"),
        }
    } else {
        println!("  none found in 200k keys");
    }

    // 2. Measured failure rate over many real keys.
    println!("\n=== 2. measured rate over 500,000 real P-256 keys ===");
    let total = 500_000u32;
    let (mut short_x, mut short_y, mut either, mut long_coord) = (0u32, 0u32, 0u32, 0u32);
    let mut rqs_decode_failures = 0u32;
    for _ in 0..total {
        let sk = SecretKey::random(&mut rand::thread_rng());
        let point = sk.public_key().to_encoded_point(false);
        let ex = encode_point(point.x().unwrap());
        let ey = encode_point(point.y().unwrap());
        let sx = ex.len() < 32;
        let sy = ey.len() < 32;
        if sx { short_x += 1; }
        if sy { short_y += 1; }
        if sx || sy { either += 1; }
        if ex.len() > 32 || ey.len() > 32 { long_coord += 1; }
        if EncodedPoint::from_bytes(rebuild_rqs_lib(&ex, &ey)).is_err() {
            rqs_decode_failures += 1;
        }
    }
    let pct = |n: u32| n as f64 * 100.0 / total as f64;
    println!("  short x:              {short_x:>6}  ({:.3}%)", pct(short_x));
    println!("  short y:              {short_y:>6}  ({:.3}%)", pct(short_y));
    println!("  either short:         {either:>6}  ({:.3}%)  <- affected keys", pct(either));
    println!("  at least one 33-byte: {long_coord:>6}  ({:.3}%)  <- handled correctly", pct(long_coord));
    println!("  rqs_lib decode fails: {rqs_decode_failures:>6}  ({:.3}%)", pct(rqs_decode_failures));
    println!("  -> 1 in {:.0} keys", total as f64 / either.max(1) as f64);
}
