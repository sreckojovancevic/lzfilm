//! LZFILM v0.1 — compression of a sequence of fixed-length frames L as a "film".
//!
//! Idea: frames are aligned (same structure, no insertions), so instead of LZ
//! string search a positional difference to the previous frame is used
//! (P-frame). Every `gop` frames a full frame (I-frame) is stored for random
//! access. Each GOP is a separate LZMA2 stream, so any frame is decoded by
//! reading a single GOP.
//!
//! File format (little-endian):
//!   magic   "LZFM"            4
//!   version u8                1
//!   mode    u8                1   (0 raw, 1 xor, 2 sub, 3 sub+transpose, 4 B-frames, 5 B-frames+transpose)
//!   L       u32               4   frame length
//!   gop     u32               4   frames per GOP
//!   n       u64               8   total frames
//!   tail    u32               4   length of the last frame (<= L)
//!   index   n_gop × u64           offset of each GOP from the start of the payload
//!   payload GOP streams (xz/LZMA2)

use std::io::{self, Read, Write};
use xz2::stream::{Check, Filters, LzmaOptions, Stream};

const MAGIC: &[u8; 4] = b"LZFM";
const VERSION: u8 = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// No transformation — control, LZMA per GOP only.
    Raw = 0,
    /// P-frame = frame[i] XOR frame[i-1].
    Xor = 1,
    /// P-frame = frame[i] - frame[i-1] (mod 256), better for counters.
    Sub = 2,
    /// Sub, then GOP transposed by columns (byte j of all frames together).
    SubTranspose = 3,
    /// Point 9: B-frames. Anchors (every 3rd frame) are Sub against the previous anchor;
    /// frames in between are the difference to the average of the neighbouring anchors.
    BFrames = 4,
    /// B-frames, then transposition as in SubTranspose.
    BFramesTranspose = 5,
}

const B_STEP: usize = 3;

fn transposed(mode: Mode) -> bool {
    matches!(mode, Mode::SubTranspose | Mode::BFramesTranspose)
}

/// Coding order and references for B mode: (frame index, previous anchor, next anchor).
/// All anchors go first in order, then the B-frames. The decoder follows the same order.
fn b_plan(n: usize) -> Vec<(usize, Option<usize>, Option<usize>)> {
    let mut plan = Vec::with_capacity(n);
    let anchors: Vec<usize> = (0..n).step_by(B_STEP).collect();
    for (k, &a) in anchors.iter().enumerate() {
        plan.push((a, if k > 0 { Some(anchors[k - 1]) } else { None }, None));
    }
    for i in 0..n {
        if i % B_STEP != 0 {
            let pa = i - i % B_STEP;
            let na = pa + B_STEP;
            plan.push((i, Some(pa), if na < n { Some(na) } else { None }));
        }
    }
    plan
}

fn predict(prev: Option<&[u8]>, next: Option<&[u8]>, j: usize) -> u8 {
    let g = |f: Option<&[u8]>| f.and_then(|f| f.get(j).copied());
    match (g(prev), g(next)) {
        (Some(a), Some(b)) => ((a as u16 + b as u16) / 2) as u8,
        (Some(a), None) => a,
        (None, Some(b)) => b,
        (None, None) => 0,
    }
}

impl Mode {
    fn from_u8(v: u8) -> io::Result<Self> {
        Ok(match v {
            0 => Mode::Raw,
            1 => Mode::Xor,
            2 => Mode::Sub,
            3 => Mode::SubTranspose,
            4 => Mode::BFrames,
            5 => Mode::BFramesTranspose,
            _ => return Err(bad("unknown mode")),
        })
    }
}

pub struct Params {
    pub frame_len: usize,
    pub gop: usize,
    pub mode: Mode,
    pub preset: u32,
}

fn bad(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg)
}

fn xz_stream(preset: u32, frame_len: usize) -> io::Result<Stream> {
    let mut opts = LzmaOptions::new_preset(preset | 0x8000_0000)
        .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
    // Literal context: after a delta the previous byte says little, the position more.
    // lp/pb are limited to 0..=4; if L is a power of two <= 16, align to it.
    if frame_len.is_power_of_two() && frame_len <= 16 {
        let k = frame_len.trailing_zeros();
        opts.literal_position_bits(k).position_bits(k).literal_context_bits(0);
    }
    let mut f = Filters::new();
    f.lzma2(&opts);
    Stream::new_stream_encoder(&f, Check::Crc32).map_err(|e| io::Error::new(io::ErrorKind::Other, e))
}

fn xz_compress(data: &[u8], preset: u32, frame_len: usize) -> io::Result<Vec<u8>> {
    let mut enc = xz2::write::XzEncoder::new_stream(Vec::new(), xz_stream(preset, frame_len)?);
    enc.write_all(data)?;
    enc.finish()
}

fn xz_decompress(data: &[u8]) -> io::Result<Vec<u8>> {
    let mut out = Vec::new();
    xz2::read::XzDecoder::new(data).read_to_end(&mut out)?;
    Ok(out)
}

/// Turns one GOP (sequence of frames) into bytes ready for LZMA.
fn encode_gop(frames: &[&[u8]], l: usize, mode: Mode) -> Vec<u8> {
    let total: usize = frames.iter().map(|f| f.len()).sum();
    let mut out = Vec::with_capacity(total);
    if matches!(mode, Mode::BFrames | Mode::BFramesTranspose) {
        for (i, pa, na) in b_plan(frames.len()) {
            let cur = frames[i];
            // anchor: previous anchor only; B: average of previous and next
            let (p, n) = if i % B_STEP == 0 { (pa.map(|x| frames[x]), None) } else { (pa.map(|x| frames[x]), na.map(|x| frames[x])) };
            for j in 0..cur.len() {
                out.push(cur[j].wrapping_sub(predict(p, n, j)));
            }
        }
    } else {
    out.extend_from_slice(frames[0]); // I-frame
    for w in frames.windows(2) {
        let (prev, cur) = (w[0], w[1]);
        for j in 0..cur.len() {
            let p = if j < prev.len() { prev[j] } else { 0 };
            out.push(match mode {
                Mode::Raw => cur[j],
                Mode::Xor => cur[j] ^ p,
                Mode::Sub | Mode::SubTranspose => cur[j].wrapping_sub(p),
                _ => unreachable!(),
            });
        }
    }
    }
    if transposed(mode) && frames.len() > 1 && total == frames.len() * l {
        // Transpose only full GOPs (all frames of length L).
        let rows = frames.len();
        let mut t = Vec::with_capacity(total);
        for col in 0..l {
            for row in 0..rows {
                t.push(out[row * l + col]);
            }
        }
        return t;
    }
    out
}

fn decode_gop(buf: &[u8], lens: &[usize], l: usize, mode: Mode) -> io::Result<Vec<u8>> {
    let total: usize = lens.iter().sum();
    if buf.len() != total {
        return Err(bad("GOP length does not match the header"));
    }
    let mut d = buf.to_vec();
    if transposed(mode) && lens.len() > 1 && total == lens.len() * l {
        let rows = lens.len();
        let mut src = 0;
        for col in 0..l {
            for row in 0..rows {
                d[row * l + col] = buf[src];
                src += 1;
            }
        }
    }
    if matches!(mode, Mode::BFrames | Mode::BFramesTranspose) {
        // d is in b_plan order; restore the natural frame order
        let mut off = vec![0usize; lens.len() + 1];
        for i in 0..lens.len() {
            off[i + 1] = off[i] + lens[i];
        }
        let mut outb = vec![0u8; total];
        let mut src = 0;
        for (i, pa, na) in b_plan(lens.len()) {
            let n = if i % B_STEP == 0 { None } else { na };
            for j in 0..lens[i] {
                let pv = pa.and_then(|x| if j < lens[x] { Some(outb[off[x] + j]) } else { None });
                let nv = n.and_then(|x| if j < lens[x] { Some(outb[off[x] + j]) } else { None });
                let pred = match (pv, nv) {
                    (Some(a), Some(b)) => ((a as u16 + b as u16) / 2) as u8,
                    (Some(a), None) => a,
                    (None, Some(b)) => b,
                    (None, None) => 0,
                };
                outb[off[i] + j] = d[src].wrapping_add(pred);
                src += 1;
            }
        }
        return Ok(outb);
    }
    // Chain reconstruction: frame[i] = residual[i] ⊕ frame[i-1]
    let mut start = lens[0];
    let mut prev_start = 0;
    for w in lens.windows(2) {
        let (pl, cl) = (w[0], w[1]);
        for j in 0..cl {
            let p = if j < pl { d[prev_start + j] } else { 0 };
            let r = d[start + j];
            d[start + j] = match mode {
                Mode::Raw => r,
                Mode::Xor => r ^ p,
                Mode::Sub | Mode::SubTranspose => r.wrapping_add(p),
                _ => unreachable!(),
            };
        }
        prev_start = start;
        start += cl;
    }
    Ok(d)
}

fn frame_lens(n: u64, l: usize, tail: usize) -> impl Fn(u64) -> usize {
    move |i| if i + 1 == n { tail } else { l }
}

pub fn compress(input: &[u8], p: &Params) -> io::Result<Vec<u8>> {
    let l = p.frame_len;
    if l == 0 || l > u32::MAX as usize {
        return Err(bad("frame_len must be 1..=u32::MAX"));
    }
    let gop = p.gop.max(1);
    let n = if input.is_empty() { 0 } else { (input.len() + l - 1) / l } as u64;
    let tail = if n == 0 { 0 } else { input.len() - (n as usize - 1) * l };

    let frames: Vec<&[u8]> = input.chunks(l).collect();
    let mut index = Vec::new();
    let mut payload = Vec::new();
    for g in frames.chunks(gop) {
        index.push(payload.len() as u64);
        let raw = encode_gop(g, l, p.mode);
        payload.extend_from_slice(&xz_compress(&raw, p.preset, l)?);
    }

    let mut out = Vec::with_capacity(26 + index.len() * 8 + payload.len());
    out.extend_from_slice(MAGIC);
    out.push(VERSION);
    out.push(p.mode as u8);
    out.extend_from_slice(&(l as u32).to_le_bytes());
    out.extend_from_slice(&(gop as u32).to_le_bytes());
    out.extend_from_slice(&n.to_le_bytes());
    out.extend_from_slice(&(tail as u32).to_le_bytes());
    for o in &index {
        out.extend_from_slice(&o.to_le_bytes());
    }
    out.extend_from_slice(&payload);
    Ok(out)
}

pub struct Archive<'a> {
    pub mode: Mode,
    pub frame_len: usize,
    pub gop: usize,
    pub n: u64,
    tail: usize,
    index: Vec<u64>,
    payload: &'a [u8],
}

impl<'a> Archive<'a> {
    pub fn open(buf: &'a [u8]) -> io::Result<Self> {
        if buf.len() < 26 || &buf[0..4] != MAGIC {
            return Err(bad("not an LZFM file"));
        }
        if buf[4] != VERSION {
            return Err(bad("unsupported version"));
        }
        let mode = Mode::from_u8(buf[5])?;
        let rd32 = |o: usize| u32::from_le_bytes(buf[o..o + 4].try_into().unwrap()) as usize;
        let l = rd32(6);
        let gop = rd32(10);
        let n = u64::from_le_bytes(buf[14..22].try_into().unwrap());
        let tail = rd32(22);
        if l == 0 || gop == 0 || tail > l {
            return Err(bad("invalid header"));
        }
        let n_gop = ((n + gop as u64 - 1) / gop as u64) as usize;
        let idx_end = 26usize
            .checked_add(n_gop.checked_mul(8).ok_or_else(|| bad("index too large"))?)
            .ok_or_else(|| bad("index too large"))?;
        if buf.len() < idx_end {
            return Err(bad("truncated index"));
        }
        let index = (0..n_gop)
            .map(|i| u64::from_le_bytes(buf[26 + i * 8..34 + i * 8].try_into().unwrap()))
            .collect();
        Ok(Archive { mode, frame_len: l, gop, n, tail, index, payload: &buf[idx_end..] })
    }

    fn gop_bytes(&self, g: usize) -> io::Result<&'a [u8]> {
        let s = self.index[g] as usize;
        let e = self.index.get(g + 1).map(|&x| x as usize).unwrap_or(self.payload.len());
        self.payload.get(s..e).ok_or_else(|| bad("index out of range"))
    }

    /// Decodes the whole GOP g.
    pub fn read_gop(&self, g: usize) -> io::Result<Vec<u8>> {
        let first = (g * self.gop) as u64;
        let last = (first + self.gop as u64).min(self.n);
        let lf = frame_lens(self.n, self.frame_len, self.tail);
        let lens: Vec<usize> = (first..last).map(lf).collect();
        let raw = xz_decompress(self.gop_bytes(g)?)?;
        decode_gop(&raw, &lens, self.frame_len, self.mode)
    }

    /// Random access: returns frame i, decoding only its GOP.
    pub fn frame(&self, i: u64) -> io::Result<Vec<u8>> {
        if i >= self.n {
            return Err(bad("frame out of range"));
        }
        let g = (i / self.gop as u64) as usize;
        let gop = self.read_gop(g)?;
        let off = (i % self.gop as u64) as usize * self.frame_len;
        let len = frame_lens(self.n, self.frame_len, self.tail)(i);
        Ok(gop[off..off + len].to_vec())
    }

    pub fn read_all(&self) -> io::Result<Vec<u8>> {
        let mut out = Vec::new();
        for g in 0..self.index.len() {
            out.extend_from_slice(&self.read_gop(g)?);
        }
        Ok(out)
    }
}

pub fn decompress(buf: &[u8]) -> io::Result<Vec<u8>> {
    Archive::open(buf)?.read_all()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_all_film_modes() {
        let mut d = Vec::new();
        for i in 0..1000u32 {
            d.extend_from_slice(&(i * 3).to_le_bytes());
            d.extend_from_slice(&[(i % 7) as u8, 0xAA, (i * i) as u8]);
        }
        d.extend_from_slice(&[9, 8, 7]); // partial last frame
        for mode in [Mode::Raw, Mode::Xor, Mode::Sub, Mode::SubTranspose, Mode::BFrames, Mode::BFramesTranspose] {
            for gop in [1usize, 2, 3, 4, 7, 100, 10_000] {
                let p = Params { frame_len: 7, gop, mode, preset: 1 };
                let c = compress(&d, &p).unwrap();
                assert_eq!(decompress(&c).unwrap(), d, "{mode:?} gop={gop}");
                let a = Archive::open(&c).unwrap();
                assert_eq!(a.frame(500).unwrap(), d[3500..3507].to_vec());
            }
        }
    }
}
