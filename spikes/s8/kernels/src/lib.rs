//! Workloads compiled both natively and into the guest, from the same source.

use sha2::{Digest, Sha256};

/// Hash a `len`-byte buffer `iters` times, chaining each digest into the next
/// so no iteration can be skipped. Returns the last digest as hex.
pub fn sha256_repeat(len: usize, iters: u32) -> String {
    let data = vec![0xAB_u8; len];
    let mut digest = [0u8; 32];
    for _ in 0..iters {
        let mut h = Sha256::new();
        h.update(&data);
        h.update(digest);
        digest = h.finalize().into();
    }
    hex(&digest)
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(DIGITS[usize::from(b >> 4)] as char);
        out.push(DIGITS[usize::from(b & 0xf)] as char);
    }
    out
}

/// The Computer Language Benchmarks Game n-body simulation (the plain scalar
/// version). Returns the energy after `steps`, to 9 decimals.
pub fn nbody(steps: u32) -> String {
    const PI: f64 = std::f64::consts::PI;
    const SOLAR_MASS: f64 = 4.0 * PI * PI;
    const DAYS_PER_YEAR: f64 = 365.24;
    #[derive(Clone, Copy)]
    struct Body {
        pos: [f64; 3],
        vel: [f64; 3],
        mass: f64,
    }
    let mut bodies = [
        Body { pos: [0.0; 3], vel: [0.0; 3], mass: SOLAR_MASS },
        Body {
            pos: [4.841_431_442_464_72e0, -1.160_320_044_027_428_4e0, -1.036_220_444_711_231_1e-1],
            vel: [1.660_076_642_744_037e-3 * DAYS_PER_YEAR, 7.699_011_184_197_404e-3 * DAYS_PER_YEAR, -6.904_600_169_720_63e-5 * DAYS_PER_YEAR],
            mass: 9.547_919_384_243_266e-4 * SOLAR_MASS,
        },
        Body {
            pos: [8.343_366_718_244_58e0, 4.124_798_564_124_305e0, -4.035_234_171_143_214e-1],
            vel: [-2.767_425_107_268_624e-3 * DAYS_PER_YEAR, 4.998_528_012_349_172e-3 * DAYS_PER_YEAR, 2.304_172_975_737_639_3e-5 * DAYS_PER_YEAR],
            mass: 2.858_859_806_661_308e-4 * SOLAR_MASS,
        },
        Body {
            pos: [1.289_436_956_213_913_1e1, -1.511_115_140_169_863_1e1, -2.233_075_788_926_557_3e-1],
            vel: [2.964_601_375_647_616e-3 * DAYS_PER_YEAR, 2.378_471_739_594_809_5e-3 * DAYS_PER_YEAR, -2.965_895_685_402_375_6e-5 * DAYS_PER_YEAR],
            mass: 4.366_244_043_351_563e-5 * SOLAR_MASS,
        },
        Body {
            pos: [1.537_969_711_485_091_1e1, -2.591_931_460_998_796_4e1, 1.792_587_729_503_711_8e-1],
            vel: [2.680_677_724_903_893_2e-3 * DAYS_PER_YEAR, 1.628_241_700_382_423e-3 * DAYS_PER_YEAR, -9.515_922_545_197_159e-5 * DAYS_PER_YEAR],
            mass: 5.151_389_020_466_114_5e-5 * SOLAR_MASS,
        },
    ];
    let n = bodies.len();
    let mut p = [0.0; 3];
    for b in &bodies {
        for k in 0..3 {
            p[k] += b.vel[k] * b.mass;
        }
    }
    for k in 0..3 {
        bodies[0].vel[k] = -p[k] / SOLAR_MASS;
    }
    let dt = 0.01;
    for _ in 0..steps {
        for i in 0..n {
            for j in (i + 1)..n {
                let d = [
                    bodies[i].pos[0] - bodies[j].pos[0],
                    bodies[i].pos[1] - bodies[j].pos[1],
                    bodies[i].pos[2] - bodies[j].pos[2],
                ];
                let d2 = d[0] * d[0] + d[1] * d[1] + d[2] * d[2];
                let mag = dt / (d2 * d2.sqrt());
                let (mi, mj) = (bodies[i].mass, bodies[j].mass);
                for k in 0..3 {
                    bodies[i].vel[k] -= d[k] * mj * mag;
                    bodies[j].vel[k] += d[k] * mi * mag;
                }
            }
        }
        for b in &mut bodies {
            for k in 0..3 {
                b.pos[k] += dt * b.vel[k];
            }
        }
    }
    let mut e = 0.0;
    for i in 0..n {
        let b = &bodies[i];
        e += 0.5 * b.mass * (b.vel[0] * b.vel[0] + b.vel[1] * b.vel[1] + b.vel[2] * b.vel[2]);
        for j in (i + 1)..n {
            let c = &bodies[j];
            let d = [b.pos[0] - c.pos[0], b.pos[1] - c.pos[1], b.pos[2] - c.pos[2]];
            e -= b.mass * c.mass / (d[0] * d[0] + d[1] * d[1] + d[2] * d[2]).sqrt();
        }
    }
    format!("{e:.9}")
}

/// The Computer Language Benchmarks Game fannkuch-redux (single-threaded).
/// Returns `checksum,max_flips`.
pub fn fannkuch(n: usize) -> String {
    let mut perm1: Vec<usize> = (0..n).collect();
    let mut perm = vec![0usize; n];
    let mut count = vec![0usize; n];
    let (mut max_flips, mut checksum, mut perm_count) = (0usize, 0i64, 0usize);
    let mut r = n;
    loop {
        while r != 1 {
            count[r - 1] = r;
            r -= 1;
        }
        perm.copy_from_slice(&perm1);
        let mut flips = 0;
        let mut k = perm[0];
        while k != 0 {
            perm[..=k].reverse();
            flips += 1;
            k = perm[0];
        }
        max_flips = max_flips.max(flips);
        checksum += if perm_count % 2 == 0 { flips as i64 } else { -(flips as i64) };
        loop {
            if r == n {
                return format!("{checksum},{max_flips}");
            }
            let first = perm1[0];
            for i in 0..r {
                perm1[i] = perm1[i + 1];
            }
            perm1[r] = first;
            count[r] -= 1;
            if count[r] > 0 {
                break;
            }
            r += 1;
        }
        perm_count += 1;
    }
}

/// Run one command: `noop`, `sha256:<len>:<iters>`, `nbody:<steps>`,
/// `fannkuch:<n>`.
pub fn run(command: &str) -> String {
    let mut parts = command.split(':');
    let verb = parts.next().unwrap_or("");
    let mut num = || parts.next().and_then(|p| p.parse::<u64>().ok()).unwrap_or(0);
    match verb {
        "noop" => String::new(),
        "sha256" => {
            let len = num() as usize;
            sha256_repeat(len, num() as u32)
        }
        "nbody" => nbody(num() as u32),
        "fannkuch" => fannkuch(num() as usize),
        _ => format!("unknown command {command}"),
    }
}
