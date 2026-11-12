//! Forward 8x8 transform: the inverse of the decoder's integer inverse
//! transform taken as a linear map, so coefficient layout and scaling match
//! the decoder exactly and only the decoder's rounding is left out.

use std::sync::OnceLock;

use crate::wmv3::decode::dsp::{Block, inv_trans_8x8};

/// 64x64 matrix in row-major order.
type Matrix = Vec<f64>;

/// Amplitude of the impulses that measure the inverse transform; large
/// enough that rounding is negligible, small enough for i16 intermediates.
const PROBE: i16 = 1024;

/// Pixel response of each coefficient position: column k is the output of
/// the inverse transform for a unit coefficient at position k.
fn inverse_matrix() -> Matrix {
    let mut m = vec![0.0; 64 * 64];
    for k in 0..64 {
        let mut b: Block = [0; 64];
        b[k] = PROBE;
        inv_trans_8x8(&mut b);
        for (p, v) in b.iter().enumerate() {
            m[p * 64 + k] = f64::from(*v) / f64::from(PROBE);
        }
    }
    m
}

/// Gauss-Jordan inversion with partial pivoting.
fn invert(mut a: Matrix) -> Matrix {
    let n = 64;
    let mut inv = vec![0.0; n * n];
    for i in 0..n {
        inv[i * n + i] = 1.0;
    }
    for col in 0..n {
        let pivot = (col..n)
            .max_by(|&x, &y| a[x * n + col].abs().total_cmp(&a[y * n + col].abs()))
            .expect("non-empty range");
        for c in 0..n {
            a.swap(col * n + c, pivot * n + c);
            inv.swap(col * n + c, pivot * n + c);
        }
        let d = a[col * n + col];
        assert!(d.abs() > 1e-9, "inverse transform is singular");
        for c in 0..n {
            a[col * n + c] /= d;
            inv[col * n + c] /= d;
        }
        for r in 0..n {
            if r != col {
                let f = a[r * n + col];
                if f != 0.0 {
                    for c in 0..n {
                        a[r * n + c] -= f * a[col * n + c];
                        inv[r * n + c] -= f * inv[col * n + c];
                    }
                }
            }
        }
    }
    inv
}

fn transpose(m: &Matrix) -> Matrix {
    (0..64 * 64).map(|i| m[i % 64 * 64 + i / 64]).collect()
}

/// The forward transform transposed: row p holds pixel p's weight in
/// every coefficient.
fn forward_matrix() -> &'static Matrix {
    static M: OnceLock<Matrix> = OnceLock::new();
    M.get_or_init(|| transpose(&invert(inverse_matrix())))
}

/// Coefficients, in the decoder's layout, whose inverse transform gives
/// `pixels` (row-major 8x8).
pub(crate) fn forward(pixels: &[f64; 64]) -> [f64; 64] {
    // All coefficients accumulate together, each over the pixels in order
    // from -0.0 as a sum of each would.
    let mut out = [-0.0; 64];
    for (weights, &x) in forward_matrix().chunks_exact(64).zip(pixels) {
        for (o, &w) in out.iter_mut().zip(weights) {
            *o += w * x;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Forward then the decoder's inverse returns the block to within the
    /// decoder's rounding.
    #[test]
    fn decoder_inverse_restores_forward_input() {
        let pixels: [f64; 64] =
            std::array::from_fn(|i| f64::from(((i * 37 + (i / 8) * 11) % 200) as u16) - 100.0);
        let c = forward(&pixels);
        let mut b: Block = std::array::from_fn(|k| c[k].round() as i16);
        inv_trans_8x8(&mut b);
        for (p, v) in b.iter().enumerate() {
            assert!(
                (f64::from(*v) - pixels[p]).abs() <= 2.0,
                "sample {p}: {v} vs {}",
                pixels[p]
            );
        }
    }
}
