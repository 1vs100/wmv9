//! Arithmetic kernels, transcribed from FFmpeg's vc1dsp.c and idctdsp.c so
//! that results are bit-identical, including the 16-bit truncation of
//! intermediate values.

/// An 8x8 block of coefficients or samples, row-major.
pub type Block = [i16; 64];

/// VC-1 8x8 inverse transform (vc1_inv_trans_8x8_c).
pub fn inv_trans_8x8(block: &mut Block) {
    let mut temp = [0i16; 64];
    for i in 0..8 {
        let s = |k: usize| i32::from(block[i + k]);
        let t1 = 12 * (s(0) + s(32)) + 4;
        let t2 = 12 * (s(0) - s(32)) + 4;
        let t3 = 16 * s(16) + 6 * s(48);
        let t4 = 6 * s(16) - 16 * s(48);
        let (t5, t6, t7, t8) = (t1 + t3, t2 + t4, t2 - t4, t1 - t3);
        let u1 = 16 * s(8) + 15 * s(24) + 9 * s(40) + 4 * s(56);
        let u2 = 15 * s(8) - 4 * s(24) - 16 * s(40) - 9 * s(56);
        let u3 = 9 * s(8) - 16 * s(24) + 4 * s(40) + 15 * s(56);
        let u4 = 4 * s(8) - 9 * s(24) + 15 * s(40) - 16 * s(56);
        let d = &mut temp[i * 8..i * 8 + 8];
        d[0] = ((t5 + u1) >> 3) as i16;
        d[1] = ((t6 + u2) >> 3) as i16;
        d[2] = ((t7 + u3) >> 3) as i16;
        d[3] = ((t8 + u4) >> 3) as i16;
        d[4] = ((t8 - u4) >> 3) as i16;
        d[5] = ((t7 - u3) >> 3) as i16;
        d[6] = ((t6 - u2) >> 3) as i16;
        d[7] = ((t5 - u1) >> 3) as i16;
    }
    for i in 0..8 {
        let s = |k: usize| i32::from(temp[i + k]);
        let t1 = 12 * (s(0) + s(32)) + 64;
        let t2 = 12 * (s(0) - s(32)) + 64;
        let t3 = 16 * s(16) + 6 * s(48);
        let t4 = 6 * s(16) - 16 * s(48);
        let (t5, t6, t7, t8) = (t1 + t3, t2 + t4, t2 - t4, t1 - t3);
        let u1 = 16 * s(8) + 15 * s(24) + 9 * s(40) + 4 * s(56);
        let u2 = 15 * s(8) - 4 * s(24) - 16 * s(40) - 9 * s(56);
        let u3 = 9 * s(8) - 16 * s(24) + 4 * s(40) + 15 * s(56);
        let u4 = 4 * s(8) - 9 * s(24) + 15 * s(40) - 16 * s(56);
        block[i] = ((t5 + u1) >> 7) as i16;
        block[i + 8] = ((t6 + u2) >> 7) as i16;
        block[i + 16] = ((t7 + u3) >> 7) as i16;
        block[i + 24] = ((t8 + u4) >> 7) as i16;
        block[i + 32] = ((t8 - u4 + 1) >> 7) as i16;
        block[i + 40] = ((t7 - u3 + 1) >> 7) as i16;
        block[i + 48] = ((t6 - u2 + 1) >> 7) as i16;
        block[i + 56] = ((t5 - u1 + 1) >> 7) as i16;
    }
}

/// Overlap smoothing across a horizontal edge: rows 6 and 7 of `top` and
/// rows 0 and 1 of `bottom` (vc1_v_s_overlap_c).
pub fn v_s_overlap(top: &mut Block, bottom: &mut Block) {
    let (mut rnd1, mut rnd2) = (4, 3);
    for i in 0..8 {
        let a = i32::from(top[48 + i]);
        let b = i32::from(top[56 + i]);
        let c = i32::from(bottom[i]);
        let d = i32::from(bottom[8 + i]);
        let d1 = a - d;
        let d2 = a - d + b - c;
        top[48 + i] = ((a * 8 - d1 + rnd1) >> 3) as i16;
        top[56 + i] = ((b * 8 - d2 + rnd2) >> 3) as i16;
        bottom[i] = ((c * 8 + d2 + rnd1) >> 3) as i16;
        bottom[8 + i] = ((d * 8 + d1 + rnd2) >> 3) as i16;
        rnd2 = 7 - rnd2;
        rnd1 = 7 - rnd1;
    }
}

/// Overlap smoothing across a vertical edge: columns 6 and 7 of `left` and
/// columns 0 and 1 of `right`, for progressive pictures
/// (vc1_h_s_overlap_c with strides 8 and flags 1).
pub fn h_s_overlap(left: &mut Block, right: &mut Block) {
    let (mut rnd1, mut rnd2) = (4, 3);
    for i in 0..8 {
        let r = i * 8;
        let a = i32::from(left[r + 6]);
        let b = i32::from(left[r + 7]);
        let c = i32::from(right[r]);
        let d = i32::from(right[r + 1]);
        let d1 = a - d;
        let d2 = a - d + b - c;
        left[r + 6] = ((a * 8 - d1 + rnd1) >> 3) as i16;
        left[r + 7] = ((b * 8 - d2 + rnd2) >> 3) as i16;
        right[r] = ((c * 8 + d2 + rnd1) >> 3) as i16;
        right[r + 1] = ((d * 8 + d1 + rnd2) >> 3) as i16;
        rnd2 = 7 - rnd2;
        rnd1 = 7 - rnd1;
    }
}

/// Overlap smoothing between two blocks of the same buffer.
pub fn h_s_overlap_within(blocks: &mut [Block], left: usize, right: usize) {
    let (l, r) = two_mut(blocks, left, right);
    h_s_overlap(l, r);
}

pub fn v_s_overlap_within(blocks: &mut [Block], top: usize, bottom: usize) {
    let (t, b) = two_mut(blocks, top, bottom);
    v_s_overlap(t, b);
}

fn two_mut<T>(v: &mut [T], a: usize, b: usize) -> (&mut T, &mut T) {
    assert_ne!(a, b);
    if a < b {
        let (x, y) = v.split_at_mut(b);
        (&mut x[a], &mut y[0])
    } else {
        let (x, y) = v.split_at_mut(a);
        (&mut y[0], &mut x[b])
    }
}

/// A plane of 8-bit samples.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plane {
    pub width: usize,
    pub height: usize,
    pub data: Vec<u8>,
}

impl Plane {
    pub fn new(width: usize, height: usize) -> Self {
        Plane {
            width,
            height,
            data: vec![0; width * height],
        }
    }

    pub fn index(&self, x: usize, y: usize) -> usize {
        y * self.width + x
    }
}

/// put_pixels_clamped / put_signed_pixels_clamped for one block.
pub fn put_block(plane: &mut Plane, x: usize, y: usize, block: &Block, signed: bool) {
    for r in 0..8 {
        let row = plane.index(x, y + r);
        for c in 0..8 {
            let v = i32::from(block[r * 8 + c]) + if signed { 128 } else { 0 };
            plane.data[row + c] = v.clamp(0, 255) as u8;
        }
    }
}

/// One loop filter line across the edge before sample `at`; samples are
/// `stride` apart along the filter direction (vc1_filter_line).
fn filter_line(p: &mut [u8], at: usize, stride: usize, pq: i32) -> bool {
    let s = |k: isize| i32::from(p[(at as isize + k * stride as isize) as usize]);
    let a0 = (2 * (s(-2) - s(1)) - 5 * (s(-1) - s(0)) + 4) >> 3;
    let a0_sign = a0 >> 31;
    let a0 = (a0 ^ a0_sign) - a0_sign;
    if a0 >= pq {
        return false;
    }
    let a1 = ((2 * (s(-4) - s(-1)) - 5 * (s(-3) - s(-2)) + 4) >> 3).abs();
    let a2 = ((2 * (s(0) - s(3)) - 5 * (s(1) - s(2)) + 4) >> 3).abs();
    if !(a1 < a0 || a2 < a0) {
        return false;
    }
    let clip = s(-1) - s(0);
    let clip_sign = clip >> 31;
    let clip = ((clip ^ clip_sign) - clip_sign) >> 1;
    if clip == 0 {
        return false;
    }
    let a3 = a1.min(a2);
    let d = 5 * (a3 - a0);
    let mut d_sign = d >> 31;
    let mut d = ((d ^ d_sign) - d_sign) >> 3;
    d_sign ^= a0_sign;
    if (d_sign ^ clip_sign) == 0 {
        d = d.min(clip);
        d = (d ^ d_sign) - d_sign;
        let i0 = at - stride;
        p[i0] = (i32::from(p[i0]) - d).clamp(0, 255) as u8;
        p[at] = (i32::from(p[at]) + d).clamp(0, 255) as u8;
    }
    true
}

/// vc1_loop_filter: `len` lines starting at `start`, lines `step` apart,
/// each filtered across the edge with samples `stride` apart.
fn loop_filter(p: &mut [u8], start: usize, step: usize, stride: usize, len: usize, pq: i32) {
    let mut src = start;
    for _ in (0..len).step_by(4) {
        if filter_line(p, src + 2 * step, stride, pq) {
            filter_line(p, src, stride, pq);
            filter_line(p, src + step, stride, pq);
            filter_line(p, src + 3 * step, stride, pq);
        }
        src += step * 4;
    }
}

/// Filters the horizontal edge above row `y`, `len` samples from column `x`.
pub fn v_loop_filter(plane: &mut Plane, x: usize, y: usize, len: usize, pq: i32) {
    let start = plane.index(x, y);
    let w = plane.width;
    loop_filter(&mut plane.data, start, 1, w, len, pq);
}

/// Filters the vertical edge left of column `x`, `len` samples from row `y`.
pub fn h_loop_filter(plane: &mut Plane, x: usize, y: usize, len: usize, pq: i32) {
    let start = plane.index(x, y);
    let w = plane.width;
    loop_filter(&mut plane.data, start, w, 1, len, pq);
}
