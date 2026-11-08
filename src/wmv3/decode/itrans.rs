//! Inverse transforms that add a residual to prediction samples,
//! transcribed from FFmpeg's vc1dsp.c (vc1_inv_trans_*_c, the *_dc_c
//! shortcuts FFmpeg takes when only the DC coefficient is coded, and
//! add_pixels_clamped after the 8x8 transform).

use super::dsp::{Block, Plane, inv_trans_8x8};

fn add(plane: &mut Plane, x: usize, y: usize, dx: usize, dy: usize, v: i32) {
    let i = plane.index(x + dx, y + dy);
    plane.data[i] = (i32::from(plane.data[i]) + v).clamp(0, 255) as u8;
}

/// Full 8x8 transform followed by add_pixels_clamped.
pub fn add_8x8(plane: &mut Plane, x: usize, y: usize, block: &mut Block) {
    inv_trans_8x8(block);
    for r in 0..8 {
        for c in 0..8 {
            add(plane, x, y, c, r, i32::from(block[r * 8 + c]));
        }
    }
}

fn add_dc(plane: &mut Plane, x: usize, y: usize, w: usize, h: usize, dc: i32) {
    for r in 0..h {
        for c in 0..w {
            add(plane, x, y, c, r, dc);
        }
    }
}

pub fn add_8x8_dc(plane: &mut Plane, x: usize, y: usize, block: &Block) {
    let dc = i32::from(block[0]);
    let dc = (3 * dc + 1) >> 1;
    let dc = (3 * dc + 16) >> 5;
    add_dc(plane, x, y, 8, 8, dc);
}

/// One 8x4 half: `off` is 0 (top) or 32 (bottom) within the block.
pub fn add_8x4_dc(plane: &mut Plane, x: usize, y: usize, block: &Block, off: usize) {
    let dc = i32::from(block[off]);
    let dc = (3 * dc + 1) >> 1;
    let dc = (17 * dc + 64) >> 7;
    add_dc(plane, x, y, 8, 4, dc);
}

/// One 4x8 half: `off` is 0 (left) or 4 (right) within the block.
pub fn add_4x8_dc(plane: &mut Plane, x: usize, y: usize, block: &Block, off: usize) {
    let dc = i32::from(block[off]);
    let dc = (17 * dc + 4) >> 3;
    let dc = (12 * dc + 64) >> 7;
    add_dc(plane, x, y, 4, 8, dc);
}

pub fn add_4x4_dc(plane: &mut Plane, x: usize, y: usize, block: &Block, off: usize) {
    let dc = i32::from(block[off]);
    let dc = (17 * dc + 4) >> 3;
    let dc = (17 * dc + 64) >> 7;
    add_dc(plane, x, y, 4, 4, dc);
}

/// vc1_inv_trans_8x4_c on the 8x4 half starting at `off`.
pub fn add_8x4(plane: &mut Plane, x: usize, y: usize, block: &mut Block, off: usize) {
    for i in 0..4 {
        let s = |k: usize| i32::from(block[off + i * 8 + k]);
        let t1 = 12 * (s(0) + s(4)) + 4;
        let t2 = 12 * (s(0) - s(4)) + 4;
        let t3 = 16 * s(2) + 6 * s(6);
        let t4 = 6 * s(2) - 16 * s(6);
        let (t5, t6, t7, t8) = (t1 + t3, t2 + t4, t2 - t4, t1 - t3);
        let u1 = 16 * s(1) + 15 * s(3) + 9 * s(5) + 4 * s(7);
        let u2 = 15 * s(1) - 4 * s(3) - 16 * s(5) - 9 * s(7);
        let u3 = 9 * s(1) - 16 * s(3) + 4 * s(5) + 15 * s(7);
        let u4 = 4 * s(1) - 9 * s(3) + 15 * s(5) - 16 * s(7);
        let out = [
            t5 + u1,
            t6 + u2,
            t7 + u3,
            t8 + u4,
            t8 - u4,
            t7 - u3,
            t6 - u2,
            t5 - u1,
        ];
        for (k, v) in out.into_iter().enumerate() {
            block[off + i * 8 + k] = (v >> 3) as i16;
        }
    }
    for i in 0..8 {
        let s = |k: usize| i32::from(block[off + i + k]);
        let t1 = 17 * (s(0) + s(16)) + 64;
        let t2 = 17 * (s(0) - s(16)) + 64;
        let t3 = 22 * s(8) + 10 * s(24);
        let t4 = 22 * s(24) - 10 * s(8);
        add(plane, x, y, i, 0, (t1 + t3) >> 7);
        add(plane, x, y, i, 1, (t2 - t4) >> 7);
        add(plane, x, y, i, 2, (t2 + t4) >> 7);
        add(plane, x, y, i, 3, (t1 - t3) >> 7);
    }
}

/// vc1_inv_trans_4x8_c on the 4x8 half starting at `off`.
pub fn add_4x8(plane: &mut Plane, x: usize, y: usize, block: &mut Block, off: usize) {
    for i in 0..8 {
        let s = |k: usize| i32::from(block[off + i * 8 + k]);
        let t1 = 17 * (s(0) + s(2)) + 4;
        let t2 = 17 * (s(0) - s(2)) + 4;
        let t3 = 22 * s(1) + 10 * s(3);
        let t4 = 22 * s(3) - 10 * s(1);
        let out = [t1 + t3, t2 - t4, t2 + t4, t1 - t3];
        for (k, v) in out.into_iter().enumerate() {
            block[off + i * 8 + k] = (v >> 3) as i16;
        }
    }
    for i in 0..4 {
        let s = |k: usize| i32::from(block[off + i + k]);
        let t1 = 12 * (s(0) + s(32)) + 64;
        let t2 = 12 * (s(0) - s(32)) + 64;
        let t3 = 16 * s(16) + 6 * s(48);
        let t4 = 6 * s(16) - 16 * s(48);
        let (t5, t6, t7, t8) = (t1 + t3, t2 + t4, t2 - t4, t1 - t3);
        let u1 = 16 * s(8) + 15 * s(24) + 9 * s(40) + 4 * s(56);
        let u2 = 15 * s(8) - 4 * s(24) - 16 * s(40) - 9 * s(56);
        let u3 = 9 * s(8) - 16 * s(24) + 4 * s(40) + 15 * s(56);
        let u4 = 4 * s(8) - 9 * s(24) + 15 * s(40) - 16 * s(56);
        let rows = [
            (t5 + u1) >> 7,
            (t6 + u2) >> 7,
            (t7 + u3) >> 7,
            (t8 + u4) >> 7,
            (t8 - u4 + 1) >> 7,
            (t7 - u3 + 1) >> 7,
            (t6 - u2 + 1) >> 7,
            (t5 - u1 + 1) >> 7,
        ];
        for (r, v) in rows.into_iter().enumerate() {
            add(plane, x, y, i, r, v);
        }
    }
}

/// vc1_inv_trans_4x4_c on the 4x4 quarter starting at `off`.
pub fn add_4x4(plane: &mut Plane, x: usize, y: usize, block: &mut Block, off: usize) {
    for i in 0..4 {
        let s = |k: usize| i32::from(block[off + i * 8 + k]);
        let t1 = 17 * (s(0) + s(2)) + 4;
        let t2 = 17 * (s(0) - s(2)) + 4;
        let t3 = 22 * s(1) + 10 * s(3);
        let t4 = 22 * s(3) - 10 * s(1);
        let out = [t1 + t3, t2 - t4, t2 + t4, t1 - t3];
        for (k, v) in out.into_iter().enumerate() {
            block[off + i * 8 + k] = (v >> 3) as i16;
        }
    }
    for i in 0..4 {
        let s = |k: usize| i32::from(block[off + i + k]);
        let t1 = 17 * (s(0) + s(16)) + 64;
        let t2 = 17 * (s(0) - s(16)) + 64;
        let t3 = 22 * s(8) + 10 * s(24);
        let t4 = 22 * s(24) - 10 * s(8);
        add(plane, x, y, i, 0, (t1 + t3) >> 7);
        add(plane, x, y, i, 1, (t2 - t4) >> 7);
        add(plane, x, y, i, 2, (t2 + t4) >> 7);
        add(plane, x, y, i, 3, (t1 - t3) >> 7);
    }
}
