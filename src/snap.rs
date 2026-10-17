//! Chain of versions (snapshots) as a film: frame = a whole version.
//!
//! Version i is coded against version i−1, block by block (4 KB), and everything goes
//! into one LZMA stream. Both versions are read sequentially, so RAM does not depend on
//! the snapshot size: two buffers + the LZMA dictionary (+ a block index when `motion` is on).
//!
//! Without `motion`: each block = XOR with the same position in the previous version.
//! With `motion` (point 8, "motion estimation" for disks): a file system does not shift
//! bytes, it relocates whole blocks. So the previous version is indexed by the content
//! of its 4 KB blocks; if a block does not match at the same position, the same content
//! is looked up anywhere in the previous version. Every block gets a tag:
//!   0 = XOR with the same position, 1 = moved (+ u64 source, XOR with the source),
//!   2 = no prediction (block goes as is) — when XOR does not produce at least 50% zeros,
//!       because new content XORed with unrelated old content becomes incompressible.
//!
//! Format (.lzs): "LZSN" ver:u8(=2) flags:u8 n:u32 n×size:u64, then the xz stream.

use std::collections::HashMap;
use std::fs::File;
use std::io::{self, BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

const MAGIC: &[u8; 4] = b"LZSN";
const VER: u8 = 2;
const CHUNK: usize = 4 << 20;
pub const BLK: usize = 4096;

const TAG_SAME: u8 = 0;
const TAG_MOVED: u8 = 1;
const TAG_RAW: u8 = 2;

#[derive(Default, Debug, Clone, Copy)]
pub struct Stats {
    pub same_zero: u64,
    pub same: u64,
    pub moved: u64,
    pub raw: u64,
}

fn read_full(r: &mut impl Read, buf: &mut [u8]) -> io::Result<usize> {
    let mut got = 0;
    while got < buf.len() {
        let k = r.read(&mut buf[got..])?;
        if k == 0 {
            break;
        }
        got += k;
    }
    Ok(got)
}

fn hash(b: &[u8]) -> u64 {
    // FNV-1a over u64 words; a collision only spoils the prediction, never correctness.
    let mut h: u64 = 0xcbf29ce484222325;
    for w in b.chunks(8) {
        let mut x = [0u8; 8];
        x[..w.len()].copy_from_slice(w);
        h ^= u64::from_le_bytes(x);
        h = h.wrapping_mul(0x100000001b3).rotate_left(23);
    }
    h
}

fn nonzero(b: &[u8]) -> usize {
    b.iter().filter(|&&x| x != 0).count()
}

/// Content index of a version's 4 KB blocks (first occurrence; zero blocks are skipped).
fn index(path: &Path) -> io::Result<HashMap<u64, u64>> {
    let mut r = BufReader::with_capacity(CHUNK, File::open(path)?);
    let mut buf = vec![0u8; CHUNK];
    let mut map = HashMap::new();
    let mut off = 0u64;
    loop {
        let n = read_full(&mut r, &mut buf)?;
        if n == 0 {
            break;
        }
        for (k, b) in buf[..n].chunks(BLK).enumerate() {
            if b.len() == BLK && nonzero(b) > 0 {
                map.entry(hash(b)).or_insert(off + (k * BLK) as u64);
            }
        }
        off += n as u64;
    }
    Ok(map)
}

pub fn compress(versions: &[PathBuf], dst: &Path, preset: u32, motion: bool) -> io::Result<Stats> {
    let mut out = BufWriter::with_capacity(CHUNK, File::create(dst)?);
    out.write_all(MAGIC)?;
    out.write_all(&[VER, motion as u8])?;
    out.write_all(&(versions.len() as u32).to_le_bytes())?;
    for v in versions {
        out.write_all(&std::fs::metadata(v)?.len().to_le_bytes())?;
    }
    let mut xz = xz2::write::XzEncoder::new(out, preset);
    let mut st = Stats::default();
    let mut cur = vec![0u8; CHUNK];
    let mut prev = vec![0u8; CHUNK];
    let mut moved = vec![0u8; BLK];
    let mut res = Vec::with_capacity(CHUNK + CHUNK / BLK * 9);
    for (i, v) in versions.iter().enumerate() {
        let mut rc = BufReader::with_capacity(CHUNK, File::open(v)?);
        let pv = if i > 0 { Some(&versions[i - 1]) } else { None };
        let mut rp = match pv { Some(p) => Some(BufReader::with_capacity(CHUNK, File::open(p)?)), None => None };
        let idx = match (motion, pv) { (true, Some(p)) => Some(index(p)?), _ => None };
        let mut rnd = match (motion, pv) { (true, Some(p)) => Some(File::open(p)?), _ => None };
        let mut pos = 0u64;
        loop {
            let n = read_full(&mut rc, &mut cur)?;
            if n == 0 {
                break;
            }
            let m = match rp.as_mut() { Some(r) => read_full(r, &mut prev[..n])?, None => 0 };
            prev[m..n].iter_mut().for_each(|x| *x = 0);
            res.clear();
            if !motion || i == 0 {
                for j in 0..n {
                    res.push(cur[j] ^ prev[j]);
                }
            } else {
                for k in (0..n).step_by(BLK) {
                    let e = (k + BLK).min(n);
                    let c = &cur[k..e];
                    let p = &prev[k..e];
                    let xs: Vec<u8> = c.iter().zip(p).map(|(a, b)| a ^ b).collect();
                    let nz_same = nonzero(&xs);
                    if nz_same == 0 {
                        st.same_zero += 1;
                        res.push(TAG_SAME);
                        res.extend_from_slice(&xs);
                        continue;
                    }
                    // look for the same content elsewhere in the previous version
                    let mut done = false;
                    if c.len() == BLK {
                        if let Some(&src) = idx.as_ref().unwrap().get(&hash(c)) {
                            if src != pos + k as u64 {
                                let f = rnd.as_mut().unwrap();
                                f.seek(SeekFrom::Start(src))?;
                                if read_full(f, &mut moved)? == BLK && moved[..] == c[..] {
                                    st.moved += 1;
                                    res.push(TAG_MOVED);
                                    res.extend_from_slice(&src.to_le_bytes());
                                    res.extend(std::iter::repeat(0u8).take(BLK));
                                    done = true;
                                }
                            }
                        }
                    }
                    if !done {
                        // XOR only makes sense if it really produces zeros; new content written over
                        // unrelated old bytes would turn into noise — then the block goes raw.
                        if nz_same * 2 > xs.len() || nonzero(c) < nz_same {
                            st.raw += 1;
                            res.push(TAG_RAW);
                            res.extend_from_slice(c);
                        } else {
                            st.same += 1;
                            res.push(TAG_SAME);
                            res.extend_from_slice(&xs);
                        }
                    }
                }
            }
            xz.write_all(&res)?;
            pos += n as u64;
        }
    }
    xz.finish()?.flush()?;
    Ok(st)
}

pub fn extract(src: &Path, dir: &Path) -> io::Result<Vec<PathBuf>> {
    let bad = |m: &str| io::Error::new(io::ErrorKind::InvalidData, m.to_string());
    let mut f = BufReader::with_capacity(CHUNK, File::open(src)?);
    let mut h = [0u8; 10];
    f.read_exact(&mut h)?;
    if &h[..4] != MAGIC || h[4] != VER {
        return Err(bad("not an LZSN file or unsupported version"));
    }
    let motion = h[5] & 1 == 1;
    let n = u32::from_le_bytes(h[6..10].try_into().unwrap()) as usize;
    let mut sizes = Vec::with_capacity(n);
    for _ in 0..n {
        let mut b = [0u8; 8];
        f.read_exact(&mut b)?;
        sizes.push(u64::from_le_bytes(b));
    }
    std::fs::create_dir_all(dir)?;
    let mut xz = xz2::read::XzDecoder::new(f);
    let mut res = vec![0u8; CHUNK];
    let mut prev = vec![0u8; CHUNK];
    let mut blk = vec![0u8; BLK];
    let mut srcb = vec![0u8; BLK];
    let mut outs: Vec<PathBuf> = Vec::new();
    for (i, &size) in sizes.iter().enumerate() {
        let p = dir.join(format!("v{i}.img"));
        let mut w = BufWriter::with_capacity(CHUNK, File::create(&p)?);
        let mut rp = if i > 0 { Some(BufReader::with_capacity(CHUNK, File::open(&outs[i - 1])?)) } else { None };
        let mut rnd = if i > 0 && motion { Some(File::open(&outs[i - 1])?) } else { None };
        let mut left = size;
        while left > 0 {
            let n = left.min(CHUNK as u64) as usize;
            let m = match rp.as_mut() { Some(r) => read_full(r, &mut prev[..n])?, None => 0 };
            prev[m..n].iter_mut().for_each(|x| *x = 0);
            if !motion || i == 0 {
                if read_full(&mut xz, &mut res[..n])? != n {
                    return Err(bad("truncated stream"));
                }
                for j in 0..n {
                    res[j] ^= prev[j];
                }
                w.write_all(&res[..n])?;
            } else {
                for k in (0..n).step_by(BLK) {
                    let e = (k + BLK).min(n);
                    let len = e - k;
                    let mut tag = [0u8; 1];
                    xz.read_exact(&mut tag)?;
                    let pred: &[u8] = match tag[0] {
                        TAG_SAME => &prev[k..e],
                        TAG_MOVED => {
                            let mut s8 = [0u8; 8];
                            xz.read_exact(&mut s8)?;
                            let fh = rnd.as_mut().ok_or_else(|| bad("moved block without a previous version"))?;
                            fh.seek(SeekFrom::Start(u64::from_le_bytes(s8)))?;
                            if read_full(fh, &mut srcb[..len])? != len {
                                return Err(bad("source of a moved block is outside the version"));
                            }
                            &srcb[..len]
                        }
                        TAG_RAW => {
                            srcb[..len].iter_mut().for_each(|x| *x = 0);
                            &srcb[..len]
                        }
                        _ => return Err(bad("unknown block tag")),
                    };
                    xz.read_exact(&mut blk[..len])?;
                    for j in 0..len {
                        blk[j] ^= pred[j];
                    }
                    w.write_all(&blk[..len])?;
                }
            }
            left -= n as u64;
        }
        w.flush()?;
        outs.push(p);
    }
    Ok(outs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snap_roundtrip_with_moves() {
        let dir = std::env::temp_dir().join(format!("lzsn_test_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut x: u64 = 7;
        let mut rnd = || { x ^= x << 13; x ^= x >> 7; x ^= x << 17; x as u8 };
        let mut v0: Vec<u8> = (0..BLK * 300 + 123).map(|_| rnd()).collect();
        for b in v0[BLK * 10..BLK * 20].iter_mut() { *b = 0; }
        let mut v1 = v0.clone();
        // move blocks 50..60 to 200..210, change a few bytes, add a zero block
        let moved: Vec<u8> = v0[BLK * 50..BLK * 60].to_vec();
        v1[BLK * 200..BLK * 210].copy_from_slice(&moved);
        v1[777] ^= 0x55;
        for b in v1[BLK * 30..BLK * 31].iter_mut() { *b = 0; }
        v1.truncate(BLK * 290 + 5);
        let mut v2 = v1.clone();
        v2.extend((0..BLK * 3).map(|_| rnd()));
        let paths: Vec<PathBuf> = [&v0, &v1, &v2].iter().enumerate().map(|(i, v)| {
            let p = dir.join(format!("in{i}"));
            std::fs::write(&p, v).unwrap();
            p
        }).collect();
        for motion in [false, true] {
            let a = dir.join(format!("a{motion}.lzs"));
            let st = compress(&paths, &a, 1, motion).unwrap();
            if motion {
                assert!(st.moved >= 10, "{st:?}");
            }
            let outs = extract(&a, &dir.join(format!("out{motion}"))).unwrap();
            for (o, v) in outs.iter().zip([&v0, &v1, &v2]) {
                assert_eq!(&std::fs::read(o).unwrap(), v);
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
