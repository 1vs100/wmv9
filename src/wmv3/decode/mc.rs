//! Motion compensation kernels, transcribed from FFmpeg's vc1dsp.c (bicubic
//! "mspel" and VC-1 chroma), h264chroma_template.c and hpeldsp.c.
//!
//! Reference samples are fetched with coordinates clamped to the plane,
//! which equals FFmpeg's emulated_edge_mc for Simple and Main profile, where
//! the edge is the macroblock-aligned picture size.

use super::dsp::Plane;

/// FFmpeg's v->rnd. `One` selects the kernels that round down.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rnd {
    Zero,
    One,
}

impl Rnd {
    fn get(self) -> i32 {
        match self {
            Rnd::Zero => 0,
            Rnd::One => 1,
        }
    }

    pub fn toggled(self) -> Self {
        match self {
            Rnd::Zero => Rnd::One,
            Rnd::One => Rnd::Zero,
        }
    }
}

/// Reference sample at (x, y), replicating the plane edge outside it.
fn at(p: &Plane, x: isize, y: isize) -> i32 {
    let x = x.clamp(0, p.width as isize - 1) as usize;
    let y = y.clamp(0, p.height as isize - 1) as usize;
    i32::from(p.data[y * p.width + x])
}

/// A square of reference samples with a margin, after optional intensity
/// compensation, addressed relative to its top left sample.
pub struct Source {
    size: usize,
    margin: usize,
    data: Vec<i32>,
}

impl Source {
    /// Fetches `size + 3` samples per side starting `margin` before (x, y);
    /// the bicubic filter reads one before and two after the block.
    pub fn fetch(p: &Plane, x: isize, y: isize, size: usize, lut: Option<&[u8; 256]>) -> Self {
        let margin = 1;
        let span = size + 3;
        let mut data = Vec::with_capacity(span * span);
        for r in 0..span {
            for c in 0..span {
                let v = at(
                    p,
                    x + c as isize - margin as isize,
                    y + r as isize - margin as isize,
                );
                data.push(match lut {
                    Some(l) => i32::from(l[v as usize]),
                    None => v,
                });
            }
        }
        Source { size, margin, data }
    }

    fn s(&self, x: isize, y: isize) -> i32 {
        let span = (self.size + 3) as isize;
        let m = self.margin as isize;
        self.data[((y + m) * span + x + m) as usize]
    }
}

/// Bicubic taps by quarter-sample phase (vc1_mspel_*_filter_16bits).
fn taps(mode: usize) -> [i32; 4] {
    match mode {
        1 => [-4, 53, 18, -3],
        2 => [-1, 9, 9, -1],
        3 => [-3, 18, 53, -4],
        _ => [0, 0, 0, 0],
    }
}

/// vc1_mspel_filter: single direction filter with rounding.
fn mspel_1d(v: [i32; 4], mode: usize, r: i32) -> i32 {
    let t = taps(mode);
    let sum = t[0] * v[0] + t[1] * v[1] + t[2] * v[2] + t[3] * v[3];
    match mode {
        0 => v[1],
        2 => (sum + 8 - r) >> 4,
        _ => (sum + 32 - r) >> 6,
    }
}

fn clip(v: i32) -> u8 {
    v.clamp(0, 255) as u8
}

/// put_vc1_mspel_mc for an n x n block (n = 8 or 16): `hmode` and `vmode`
/// are the quarter-sample phases.
pub fn mspel(src: &Source, hmode: usize, vmode: usize, rnd: Rnd) -> Vec<u8> {
    let rnd = rnd.get();
    let n = src.size as isize;
    let mut out = Vec::with_capacity((n * n) as usize);
    if vmode != 0 && hmode != 0 {
        const SHIFT: [i32; 4] = [0, 5, 1, 5];
        let shift = (SHIFT[hmode] + SHIFT[vmode]) >> 1;
        let r = (1 << (shift - 1)) + rnd - 1;
        let tv = taps(vmode);
        // Vertical pass over columns -1..n+2, stored as i16 as in FFmpeg.
        let w = n + 3;
        let mut tmp = vec![0i16; (n * w) as usize];
        for j in 0..n {
            for i in 0..w {
                let x = i - 1;
                let v = tv[0] * src.s(x, j - 1)
                    + tv[1] * src.s(x, j)
                    + tv[2] * src.s(x, j + 1)
                    + tv[3] * src.s(x, j + 2);
                tmp[(j * w + i) as usize] = ((v + r) >> shift) as i16;
            }
        }
        let th = taps(hmode);
        let r = 64 - rnd;
        for j in 0..n {
            for i in 0..n {
                let t = |k: isize| i32::from(tmp[(j * w + i + 1 + k) as usize]);
                let v = th[0] * t(-1) + th[1] * t(0) + th[2] * t(1) + th[3] * t(2);
                out.push(clip((v + r) >> 7));
            }
        }
    } else if vmode != 0 {
        let r = 1 - rnd;
        for j in 0..n {
            for i in 0..n {
                let v = [
                    src.s(i, j - 1),
                    src.s(i, j),
                    src.s(i, j + 1),
                    src.s(i, j + 2),
                ];
                out.push(clip(mspel_1d(v, vmode, r)));
            }
        }
    } else {
        for j in 0..n {
            for i in 0..n {
                let v = [
                    src.s(i - 1, j),
                    src.s(i, j),
                    src.s(i + 1, j),
                    src.s(i + 2, j),
                ];
                out.push(clip(mspel_1d(v, hmode, rnd)));
            }
        }
    }
    out
}

/// hpeldsp put_pixels (rnd 0) or put_no_rnd_pixels (rnd 1) at half-sample
/// phase `dxy` (bit 0 horizontal, bit 1 vertical).
pub fn hpel(src: &Source, dxy: usize, rnd: Rnd) -> Vec<u8> {
    let n = src.size as isize;
    let r2 = 1 - rnd.get();
    let r4 = 2 - rnd.get();
    let mut out = Vec::with_capacity((n * n) as usize);
    for j in 0..n {
        for i in 0..n {
            let v = match dxy {
                0 => src.s(i, j),
                1 => (src.s(i, j) + src.s(i + 1, j) + r2) >> 1,
                2 => (src.s(i, j) + src.s(i, j + 1) + r2) >> 1,
                _ => {
                    (src.s(i, j) + src.s(i + 1, j) + src.s(i, j + 1) + src.s(i + 1, j + 1) + r4)
                        >> 2
                }
            };
            out.push(v as u8);
        }
    }
    out
}

/// put_h264_chroma_mc8 (rnd 0, rounding 32) or put_no_rnd_vc1_chroma_mc8
/// (rnd 1, rounding 28) at eighth-sample phase (x, y).
pub fn chroma(src: &Source, x: i32, y: i32, rnd: Rnd) -> Vec<u8> {
    let n = src.size as isize;
    let a = (8 - x) * (8 - y);
    let b = x * (8 - y);
    let c = (8 - x) * y;
    let d = x * y;
    let r = 32 - 4 * rnd.get();
    let mut out = Vec::with_capacity((n * n) as usize);
    for j in 0..n {
        for i in 0..n {
            let v = a * src.s(i, j)
                + b * src.s(i + 1, j)
                + c * src.s(i, j + 1)
                + d * src.s(i + 1, j + 1);
            out.push(((v + r) >> 6) as u8);
        }
    }
    out
}

/// Writes an n x n prediction into a plane.
pub fn store(plane: &mut Plane, x: usize, y: usize, n: usize, pred: &[u8]) {
    for r in 0..n {
        let row = plane.index(x, y + r);
        plane.data[row..row + n].copy_from_slice(&pred[r * n..r * n + n]);
    }
}

/// op_avg of the bicubic, half-sample and chroma kernels: rounding average
/// with what is already in the plane.
pub fn average(plane: &mut Plane, x: usize, y: usize, n: usize, pred: &[u8]) {
    for r in 0..n {
        let row = plane.index(x, y + r);
        for c in 0..n {
            let d = &mut plane.data[row + c];
            *d = ((u16::from(*d) + u16::from(pred[r * n + c]) + 1) >> 1) as u8;
        }
    }
}
