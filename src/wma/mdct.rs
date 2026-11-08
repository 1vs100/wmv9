//! Forward MDCT of `2n` samples into `n` coefficients,
//! `X[k] = sum x[i] cos(pi / n * (i + 1/2 + n/2) * (k + 1/2))`, through a
//! DCT-IV computed with an `n/2` point complex FFT.

use std::f64::consts::PI;

#[derive(Debug, Clone, Copy, Default)]
struct Complex {
    re: f64,
    im: f64,
}

impl Complex {
    fn mul(self, o: Complex) -> Complex {
        Complex {
            re: self.re * o.re - self.im * o.im,
            im: self.re * o.im + self.im * o.re,
        }
    }

    fn cis(angle: f64) -> Complex {
        Complex {
            re: angle.cos(),
            im: angle.sin(),
        }
    }
}

pub(crate) struct Mdct {
    n: usize,
    /// e^(-i pi (j + 1/4) / n), the rotation before the FFT.
    pre: Vec<Complex>,
    /// e^(-i pi j / n), the rotation after it.
    post: Vec<Complex>,
    /// FFT roots e^(-2 pi i j / (n/2)).
    roots: Vec<Complex>,
}

impl Mdct {
    /// An MDCT with `n` coefficients; `n` is a power of two of at least 4.
    pub(crate) fn new(n: usize) -> Self {
        assert!(n.is_power_of_two() && n >= 4, "MDCT size {n}");
        let m = n / 2;
        Mdct {
            n,
            pre: (0..m)
                .map(|j| Complex::cis(-PI * (4 * j + 1) as f64 / (4 * n) as f64))
                .collect(),
            post: (0..m)
                .map(|j| Complex::cis(-PI * j as f64 / n as f64))
                .collect(),
            roots: (0..m / 2)
                .map(|j| Complex::cis(-2.0 * PI * j as f64 / m as f64))
                .collect(),
        }
    }

    pub(crate) fn forward(&self, x: &[f64]) -> Vec<f64> {
        let n = self.n;
        assert_eq!(x.len(), 2 * n, "MDCT input length");
        let h = n / 2;
        let v: Vec<f64> = (0..n)
            .map(|i| {
                if i < h {
                    -x[3 * h - 1 - i] - x[3 * h + i]
                } else {
                    x[i - h] - x[3 * h - 1 - i]
                }
            })
            .collect();
        self.dct4(&v)
    }

    /// `X[k] = sum v[i] cos(pi / n * (i + 1/2) * (k + 1/2))`.
    fn dct4(&self, v: &[f64]) -> Vec<f64> {
        let n = self.n;
        let m = n / 2;
        let mut z: Vec<Complex> = (0..m)
            .map(|j| {
                Complex {
                    re: v[2 * j],
                    im: v[n - 1 - 2 * j],
                }
                .mul(self.pre[j])
            })
            .collect();
        self.fft(&mut z);
        let mut out = vec![0.0; n];
        for k in 0..m {
            let y = z[k].mul(self.post[k]);
            out[2 * k] = y.re;
            out[n - 1 - 2 * k] = -y.im;
        }
        out
    }

    /// In-place radix-2 FFT of size n/2.
    fn fft(&self, a: &mut [Complex]) {
        let m = a.len();
        let mut j = 0;
        for i in 1..m {
            let mut bit = m >> 1;
            while j & bit != 0 {
                j ^= bit;
                bit >>= 1;
            }
            j |= bit;
            if i < j {
                a.swap(i, j);
            }
        }
        let mut len = 2;
        while len <= m {
            let step = m / len;
            for start in (0..m).step_by(len) {
                for k in 0..len / 2 {
                    let w = self.roots[k * step];
                    let u = a[start + k];
                    let t = a[start + k + len / 2].mul(w);
                    a[start + k] = Complex {
                        re: u.re + t.re,
                        im: u.im + t.im,
                    };
                    a[start + k + len / 2] = Complex {
                        re: u.re - t.re,
                        im: u.im - t.im,
                    };
                }
            }
            len <<= 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn direct(x: &[f64]) -> Vec<f64> {
        let n = x.len() / 2;
        (0..n)
            .map(|k| {
                x.iter()
                    .enumerate()
                    .map(|(i, &v)| {
                        v * (PI / n as f64 * (i as f64 + 0.5 + n as f64 / 2.0) * (k as f64 + 0.5))
                            .cos()
                    })
                    .sum()
            })
            .collect()
    }

    #[test]
    fn fast_transform_equals_the_definition() {
        for n in [4, 8, 64, 512] {
            let mut seed = 12345u32;
            let x: Vec<f64> = (0..2 * n)
                .map(|_| {
                    seed = seed.wrapping_mul(1103515245).wrapping_add(12345);
                    f64::from(seed >> 16) / 65536.0 - 0.5
                })
                .collect();
            let fast = Mdct::new(n).forward(&x);
            for (k, (a, b)) in fast.iter().zip(direct(&x)).enumerate() {
                assert!((a - b).abs() < 1e-9, "n {n} k {k}: {a} against {b}");
            }
        }
    }
}
