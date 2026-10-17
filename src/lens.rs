//! Lens: recognizes the structure of a file before handing it to LZMA.
//!
//! The file is viewed as a film: frame = row of length L. For each column (byte
//! position within the frame) the coding is chosen:
//!   RAW   — byte goes as is (noise, text, already compressed data)
//!   CHAIN — difference to the same column in the previous frame (motion, counters, drift)
//!   NEG   — difference to the "negative" (most frequent value of the column, static background)
//!   DELTA2— second derivative: x − 2·prev + prev2 (linear growth: counters, timestamps)
//! Residuals are written column by column (transposed) in segments, so LZMA
//! sees long runs of similar values.
//!
//! The second lens is BWT (see `bwt.rs`): groups similar contexts without comparisons,
//! for text/code/logs where the film lens finds no period. Variant BwtMtf adds move-to-front.
//!
//! Which lenses are tried is chosen by switches (`Switches`); base is plain LZMA.
//! Whether a lens applies to a file at all is decided by trial compression of a sample:
//! if it does not beat plain LZMA by at least MIN_GAIN, the file goes RAW.

use std::io::Write;

pub const MODE_RAW: u8 = 0;
pub const MODE_CHAIN: u8 = 1;
pub const MODE_NEG: u8 = 2;
pub const MODE_DELTA2: u8 = 3;

const MAX_L: usize = 2048;
const SCORE_SAMPLE: usize = 64 * 1024;
const TRIAL_SAMPLE: usize = 256 * 1024;
const MIN_FILE: usize = 4096;
const MIN_GAIN: f64 = 0.03;
/// Target segment size (bytes) within which transposition is done.
const SEGMENT_BYTES: usize = 1 << 20;

/// Default BWT block size (memory ~5×4 B per block byte while encoding).
pub const BWT_BLOCK: usize = 8 << 20;
/// How many trailing bytes of the solid stream are used as context during selection.
pub const CTX_BYTES: usize = 512 << 10;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Raw = 0,
    Film = 1,
    Bwt = 2,
    BwtMtf = 3,
}

impl Kind {
    pub fn from_u8(v: u8) -> Option<Kind> {
        Some(match v {
            0 => Kind::Raw,
            1 => Kind::Film,
            2 => Kind::Bwt,
            3 => Kind::BwtMtf,
            _ => return None,
        })
    }
    pub fn name(self) -> &'static str {
        match self {
            Kind::Raw => "raw",
            Kind::Film => "film",
            Kind::Bwt => "bwt",
            Kind::BwtMtf => "bwt+mtf",
        }
    }
}

/// Which lenses are enabled. All disabled = base (plain solid LZMA).
#[derive(Clone, Copy, Debug, Default)]
pub struct Switches {
    pub film: bool,
    pub bwt: bool,
}

/// How the lens is selected.
#[derive(Clone, Copy, Debug)]
pub struct Detect {
    pub sw: Switches,
    /// Point 1: periods only as powers of two + fine tuning ±10% around the best.
    pub fast_period: bool,
    /// Point 2: entropy estimate instead of trial LZMA compression.
    pub proxy: bool,
    /// Selection in solid context: a trial is measured as the increment after the last CTX_BYTES of the stream.
    pub ctx: bool,
    /// Point 4: allow the DELTA2 column mode.
    pub delta2: bool,
    /// Point 7: BWT block size.
    pub bwt_block: usize,
}

impl Detect {
    pub fn new(sw: Switches) -> Self {
        Detect { sw, fast_period: false, proxy: false, ctx: false, delta2: false, bwt_block: BWT_BLOCK }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Lens {
    pub kind: Kind,
    pub l: usize,
    pub modes: Vec<u8>,
    pub negative: Vec<u8>,
}

impl Lens {
    pub fn is_raw(&self) -> bool {
        self.kind == Kind::Raw
    }
    pub fn raw() -> Self {
        Lens { kind: Kind::Raw, l: 0, modes: Vec::new(), negative: Vec::new() }
    }
    /// BWT lens; `l` holds the block size.
    pub fn bwt(mtf: bool, block: usize) -> Self {
        Lens { kind: if mtf { Kind::BwtMtf } else { Kind::Bwt }, l: block, modes: Vec::new(), negative: Vec::new() }
    }
    /// How many payload bytes a chunk of `n` input bytes occupies.
    pub fn encoded_len(&self, n: usize) -> usize {
        match self.kind {
            Kind::Bwt | Kind::BwtMtf => n + 4,
            _ => n,
        }
    }
}

fn entropy(hist: &[u32; 256], n: usize) -> f64 {
    if n == 0 {
        return 0.0;
    }
    let n = n as f64;
    hist.iter()
        .filter(|&&c| c > 0)
        .map(|&c| {
            let p = c as f64 / n;
            -p * p.log2()
        })
        .sum()
}

fn xz_size(data: &[u8]) -> usize {
    let mut e = xz2::write::XzEncoder::new(Vec::new(), 6);
    e.write_all(data).unwrap();
    e.finish().unwrap().len()
}

/// Period candidates: number of positions where s[i] == s[i-L].
/// Returns up to 3 best L values, excluding multiples of already chosen ones.
fn period_candidates(s: &[u8]) -> Vec<usize> {
    let max_l = MAX_L.min(s.len() / 8);
    if max_l < 2 {
        return Vec::new();
    }
    let mut scores: Vec<(usize, f64)> = (2..=max_l)
        .map(|l| {
            let m = s[l..].iter().zip(s.iter()).filter(|(a, b)| a == b).count();
            (l, m as f64 / (s.len() - l) as f64)
        })
        .collect();
    let best = scores.iter().map(|x| x.1).fold(0.0, f64::max);
    if best < 0.05 {
        return Vec::new();
    }
    // The smallest L close to the maximum is the true period (2L, 3L... score the same).
    scores.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap().then(a.0.cmp(&b.0)));
    let mut out: Vec<usize> = Vec::new();
    // first: the smallest L with score >= 95% of the best
    if let Some(&(l, _)) = scores.iter().filter(|x| x.1 >= best * 0.95).min_by_key(|x| x.0) {
        out.push(l);
    }
    for &(l, _) in &scores {
        if out.len() >= 3 {
            break;
        }
        if out.iter().all(|&o| l % o != 0 && o % l != 0) {
            out.push(l);
        }
    }
    out
}

/// Point 1: powers of two only, then ±10% around the best.
fn period_candidates_fast(s: &[u8]) -> Vec<usize> {
    let score = |l: usize| s[l..].iter().zip(s.iter()).filter(|(a, b)| a == b).count() as f64 / (s.len() - l) as f64;
    let max_l = 1024.min(s.len() / 8);
    let mut best = (0usize, 0.0f64);
    let mut l = 2;
    while l <= max_l {
        let v = score(l);
        if v > best.1 {
            best = (l, v);
        }
        l *= 2;
    }
    if best.1 < 0.05 {
        return Vec::new();
    }
    let lo = (best.0 * 9 / 10).max(2);
    let hi = (best.0 * 11 / 10).min(max_l);
    let mut fine = (best.0, best.1);
    for l in lo..=hi {
        let v = score(l);
        if v > fine.1 * 1.0001 || (v >= fine.1 * 0.95 && l < fine.0) {
            fine = (l, v);
        }
    }
    vec![fine.0]
}

/// Builds the lens for a given L on a sample: negative and mode per column.
///
/// Point 3: one pass per column. The NEG histogram is not counted separately — it is the
/// RAW histogram shifted by the negative value, so NEG entropy is ALWAYS equal to
/// RAW entropy. Therefore NEG is chosen by another criterion: when a column has a
/// dominant value (≥ 60%) other than zero, NEG turns it into zeros and thus
/// merges static columns into long runs of zeros after transposition — LZMA sees that,
/// order-0 entropy does not.
fn build_lens(s: &[u8], l: usize, delta2: bool) -> Lens {
    let rows = s.len() / l;
    let mut negative = vec![0u8; l];
    let mut modes = vec![MODE_RAW; l];
    for c in 0..l {
        let mut h_raw = [0u32; 256];
        let mut h_chain = [0u32; 256];
        let mut h_d2 = [0u32; 256];
        let (mut p1, mut p2) = (0u8, 0u8);
        for r in 0..rows {
            let x = s[r * l + c];
            h_raw[x as usize] += 1;
            h_chain[x.wrapping_sub(p1) as usize] += 1;
            h_d2[x.wrapping_sub(p1.wrapping_mul(2)).wrapping_add(p2) as usize] += 1;
            p2 = p1;
            p1 = x;
        }
        let neg = (0..256).max_by_key(|&v| h_raw[v]).unwrap() as u8;
        negative[c] = neg;
        let e_raw = entropy(&h_raw, rows);
        let e_chain = entropy(&h_chain, rows);
        let e_d2 = if delta2 { entropy(&h_d2, rows) } else { f64::INFINITY };
        // Baseline: RAW or NEG (same entropy); NEG when the column is mostly static.
        let dominant = h_raw[neg as usize] as f64 >= rows as f64 * 0.6 && neg != 0;
        let (mut m, mut best) = (if dominant { MODE_NEG } else { MODE_RAW }, e_raw - 1e-9);
        // Chain and delta2 must be strictly better (a lens has to earn its place).
        if e_chain < best {
            m = MODE_CHAIN;
            best = e_chain;
        }
        if e_d2 < best - 0.05 {
            m = MODE_DELTA2;
        }
        modes[c] = m;
    }
    Lens { kind: Kind::Film, l, modes, negative }
}

/// Size estimate of an encoded block without compression (point 2): sum of order-0
/// entropies per column for film, or the entropy of the whole block otherwise.
fn proxy_size(encoded: &[u8], lens: &Lens) -> usize {
    let bits = if lens.kind == Kind::Film {
        let l = lens.l;
        let rows = encoded.len() / l;
        // after transposition the columns are contiguous (sample < segment)
        (0..l)
            .map(|c| {
                let mut h = [0u32; 256];
                for &b in &encoded[c * rows..(c + 1) * rows] {
                    h[b as usize] += 1;
                }
                entropy(&h, rows) * rows as f64
            })
            .sum::<f64>()
    } else {
        let mut h = [0u32; 256];
        for &b in encoded {
            h[b as usize] += 1;
        }
        entropy(&h, encoded.len()) * encoded.len() as f64
    };
    (bits / 8.0) as usize
}

/// Recognition: returns the best enabled lens, or RAW if no lens pays off.
///
/// `ctx` is the tail of the solid stream before this file. When `opt.ctx` is on, every
/// trial is measured as an increment: xz(ctx + candidate) − xz(ctx). This shows whether
/// LZMA already packs this file well next to its neighbours — an isolated sample cannot see that.
pub fn detect(data: &[u8], opt: &Detect, ctx: &[u8]) -> Lens {
    let sw = opt.sw;
    if (!sw.film && !sw.bwt) || data.len() < MIN_FILE {
        return Lens::raw();
    }
    let score = &data[..data.len().min(SCORE_SAMPLE)];
    // Quick filter: already compressed/encrypted data (jpg, zip, docx, pdf streams) has
    // entropy ~8 bits/byte and a lens has nothing to see there — skip the trials.
    let mut h = [0u32; 256];
    for &b in score {
        h[b as usize] += 1;
    }
    if entropy(&h, score.len()) > 7.5 {
        return Lens::raw();
    }
    let trial = &data[..data.len().min(TRIAL_SAMPLE)];
    let use_ctx = opt.ctx && !opt.proxy && !ctx.is_empty();
    let ctx_size = if use_ctx { xz_size(ctx) } else { 0 };
    let measure = |enc: &[u8], lens: &Lens| -> usize {
        if opt.proxy {
            proxy_size(enc, lens)
        } else if use_ctx {
            let mut buf = Vec::with_capacity(ctx.len() + enc.len());
            buf.extend_from_slice(ctx);
            buf.extend_from_slice(enc);
            xz_size(&buf).saturating_sub(ctx_size)
        } else {
            xz_size(enc)
        }
    };
    let raw_size = measure(trial, &Lens::raw());
    let mut best = (Lens::raw(), raw_size);
    let consider = |lens: Lens, best: &mut (Lens, usize)| {
        let mut t = Vec::with_capacity(trial.len() + 4);
        Coder::new(&lens).encode(trial, &mut t);
        let side = if lens.kind == Kind::Film { lens.l * 2 } else { 0 }; // negative + modes in the table
        let sz = measure(&t, &lens) + side;
        if sz < best.1 {
            *best = (lens, sz);
        }
    };
    if sw.film {
        // Typical sample widths (8/16/24/32/64-bit, stereo, RGB, RGBA) are always tried:
        // signal periodicity (e.g. a sine in a WAV) can outscore the true sample width.
        let mut cands = if opt.fast_period { period_candidates_fast(score) } else { period_candidates(score) };
        for l in [2usize, 3, 4, 8] {
            if !cands.contains(&l) {
                cands.push(l);
            }
        }
        for l in cands {
            if trial.len() / l < 8 {
                continue;
            }
            let lens = build_lens(trial, l, opt.delta2);
            if lens.modes.iter().all(|&m| m == MODE_RAW) {
                continue;
            }
            consider(lens, &mut best);
        }
    }
    if sw.bwt {
        consider(Lens::bwt(false, opt.bwt_block), &mut best);
        consider(Lens::bwt(true, opt.bwt_block), &mut best);
    }
    if best.1 as f64 <= raw_size as f64 * (1.0 - MIN_GAIN) {
        best.0
    } else {
        Lens::raw()
    }
}

fn seg_rows(l: usize) -> usize {
    (SEGMENT_BYTES / l).max(1)
}

/// Chunk size for streaming: whole segments, ~SEGMENT_BYTES.
pub fn chunk_bytes(lens: &Lens) -> usize {
    match lens.kind {
        Kind::Raw => SEGMENT_BYTES * 4,
        Kind::Film => seg_rows(lens.l) * lens.l,
        Kind::Bwt | Kind::BwtMtf => if lens.l > 0 { lens.l } else { BWT_BLOCK },
    }
}

/// Streaming coder: takes the file in chunks (each except the last must be a
/// multiple of L) and remembers the last frame between chunks.
pub struct Coder<'a> {
    lens: &'a Lens,
    prev: Vec<u8>,
    prev2: Vec<u8>,
}

impl<'a> Coder<'a> {
    pub fn new(lens: &'a Lens) -> Self {
        let n = if lens.kind == Kind::Film { lens.l } else { 0 };
        Coder { lens, prev: vec![0u8; n], prev2: vec![0u8; n] }
    }

    pub fn encode(&mut self, data: &[u8], out: &mut Vec<u8>) {
        let lens = self.lens;
        match lens.kind {
            Kind::Raw => {
                out.extend_from_slice(data);
                return;
            }
            Kind::Bwt | Kind::BwtMtf => {
                // Each call is one BWT block: [primary index u32][L column]
                let (mut l, p) = crate::bwt::forward(data);
                if lens.kind == Kind::BwtMtf {
                    crate::bwt::mtf(&mut l);
                }
                out.extend_from_slice(&p.to_le_bytes());
                out.extend_from_slice(&l);
                return;
            }
            Kind::Film => {}
        }
        let l = lens.l;
        let rows = data.len() / l;
        let sr = seg_rows(l);
        let mut r0 = 0;
        while r0 < rows {
            let r1 = (r0 + sr).min(rows);
            for c in 0..l {
                let mode = lens.modes[c];
                let neg = lens.negative[c];
                let row = |r: usize| -> u8 { data[r * l + c] };
                let mut prev = if r0 == 0 { self.prev[c] } else { row(r0 - 1) };
                let mut prev2 = match r0 { 0 => self.prev2[c], 1 => self.prev[c], _ => row(r0 - 2) };
                for r in r0..r1 {
                    let x = row(r);
                    out.push(match mode {
                        MODE_CHAIN => x.wrapping_sub(prev),
                        MODE_NEG => x.wrapping_sub(neg),
                        MODE_DELTA2 => x.wrapping_sub(prev.wrapping_mul(2)).wrapping_add(prev2),
                        _ => x,
                    });
                    prev2 = prev;
                    prev = x;
                }
            }
            r0 = r1;
        }
        if rows >= 2 {
            self.prev2.copy_from_slice(&data[(rows - 2) * l..(rows - 1) * l]);
        } else if rows == 1 {
            self.prev2.copy_from_slice(&self.prev.clone());
        }
        if rows > 0 {
            self.prev.copy_from_slice(&data[(rows - 1) * l..rows * l]);
        }
        out.extend_from_slice(&data[rows * l..]); // tail shorter than a frame goes raw
    }

    /// `res` must be exactly `lens.encoded_len(n)` bytes of one chunk.
    pub fn decode(&mut self, res: &[u8], out: &mut Vec<u8>) {
        let lens = self.lens;
        match lens.kind {
            Kind::Raw => {
                out.extend_from_slice(res);
                return;
            }
            Kind::Bwt | Kind::BwtMtf => {
                let p = u32::from_le_bytes(res[..4].try_into().unwrap());
                let mut l = res[4..].to_vec();
                if lens.kind == Kind::BwtMtf {
                    crate::bwt::imtf(&mut l);
                }
                out.extend_from_slice(&crate::bwt::inverse(&l, p));
                return;
            }
            Kind::Film => {}
        }
        let l = lens.l;
        let rows = res.len() / l;
        let sr = seg_rows(l);
        let base = out.len();
        out.resize(base + res.len(), 0);
        let o = &mut out[base..];
        let mut src = 0;
        let mut r0 = 0;
        while r0 < rows {
            let r1 = (r0 + sr).min(rows);
            for c in 0..l {
                let mode = lens.modes[c];
                let neg = lens.negative[c];
                let mut prev = if r0 == 0 { self.prev[c] } else { o[(r0 - 1) * l + c] };
                let mut prev2 = match r0 { 0 => self.prev2[c], 1 => self.prev[c], _ => o[(r0 - 2) * l + c] };
                for r in r0..r1 {
                    let v = res[src];
                    src += 1;
                    let x = match mode {
                        MODE_CHAIN => v.wrapping_add(prev),
                        MODE_NEG => v.wrapping_add(neg),
                        MODE_DELTA2 => v.wrapping_add(prev.wrapping_mul(2)).wrapping_sub(prev2),
                        _ => v,
                    };
                    o[r * l + c] = x;
                    prev2 = prev;
                    prev = x;
                }
            }
            r0 = r1;
        }
        if rows >= 2 {
            self.prev2.copy_from_slice(&o[(rows - 2) * l..(rows - 1) * l]);
        } else if rows == 1 {
            self.prev2.copy_from_slice(&self.prev.clone());
        }
        if rows > 0 {
            self.prev.copy_from_slice(&o[(rows - 1) * l..rows * l]);
        }
        o[rows * l..].copy_from_slice(&res[src..]);
    }
}

/// Whole file at once (for samples and tests).
pub fn encode(data: &[u8], lens: &Lens, out: &mut Vec<u8>) {
    Coder::new(lens).encode(data, out)
}

pub fn decode(res: &[u8], lens: &Lens) -> Vec<u8> {
    let mut o = Vec::with_capacity(res.len());
    Coder::new(lens).decode(res, &mut o);
    o
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_all_modes() {
        let mut d = Vec::new();
        for i in 0..5000u32 {
            d.extend_from_slice(&i.to_le_bytes());
            d.extend_from_slice(&[0xAA, (i * 7) as u8, 3, 9, 1, 2, 3]);
        }
        d.extend_from_slice(&[1, 2, 3]); // partial tail
        for l in [1usize, 3, 11, 64, 1000] {
            let mut lens = build_lens(&d, l, true);
            for (c, m) in lens.modes.iter_mut().enumerate() { *m = (c % 4) as u8; } // all modes
            let mut e = Vec::new();
            encode(&d, &lens, &mut e);
            assert_eq!(e.len(), d.len());
            assert_eq!(decode(&e, &lens), d, "L={l}");
        }
        // streamed in chunks must give the same result
        let mut lens = build_lens(&d, 11, true);
        for (c, m) in lens.modes.iter_mut().enumerate() { *m = (c % 4) as u8; }
        let mut whole = Vec::new();
        encode(&d, &lens, &mut whole);
        let mut parts = Vec::new();
        let mut c = Coder::new(&lens);
        for ch in d.chunks(11 * 2) { c.encode(ch, &mut parts); }
        // transposition per segment changes the layout, but decoding must match
        let mut back = Vec::new();
        let mut c = Coder::new(&lens);
        let mut off = 0;
        for ch in d.chunks(11 * 2) { c.decode(&parts[off..off + ch.len()], &mut back); off += ch.len(); }
        assert_eq!(back, d);
        // chunks of a single row each (prev2 edge case)
        let mut parts = Vec::new();
        let mut c = Coder::new(&lens);
        for ch in d.chunks(11) { c.encode(ch, &mut parts); }
        let mut back = Vec::new();
        let mut c = Coder::new(&lens);
        let mut off = 0;
        for ch in d.chunks(11) { c.decode(&parts[off..off + ch.len()], &mut back); off += ch.len(); }
        assert_eq!(back, d);
        assert_eq!(detect(&d, &Detect::new(Switches { film: true, bwt: false }), &[]).l, 11);
        let mut f = Detect::new(Switches { film: true, bwt: false });
        f.fast_period = true;
        f.delta2 = true;
        f.ctx = true;
        let _ = detect(&d, &f, &d[..4096]);
        // BWT roundtrip through Coder
        for m in [false, true] {
            let lens = Lens::bwt(m, 1000);
            let mut e = Vec::new();
            Coder::new(&lens).encode(&d, &mut e);
            assert_eq!(e.len(), lens.encoded_len(d.len()));
            let mut back = Vec::new();
            Coder::new(&lens).decode(&e, &mut back);
            assert_eq!(back, d);
        }
    }
}
