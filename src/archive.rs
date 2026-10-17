//! Directory archiver: a lens per file + one solid LZMA2 stream.
//!
//! File layout (.lzd), all little-endian:
//!   "LZFD" ver:u8
//!   payload        — one xz stream: encoded contents of all files in order
//!   table          — xz stream: for each file the path, size, ranges (length + lens)
//!   footer         — table_off:u64 table_len:u64 "LZFE"
//!
//! Writing and reading are strictly sequential (HDD friendly): each file is
//! read once, start to end, in large blocks; the archive is written forward
//! only, the table goes at the end.

use crate::lens::{self, Coder, Detect, Kind, Lens};
use std::fs::{self, File};
use std::io::{self, BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};

const MAGIC: &[u8; 4] = b"LZFD";
const END: &[u8; 4] = b"LZFE";
const VERSION: u8 = 3;
/// Range length when per-range lens selection is on (point 5 / mixing lenses).
pub const RANGE: u64 = 16 << 20;

/// Extensions of known already-compressed formats: no lens trial (saves time).
const PACKED_EXT: &[&str] = &[
    "jpg", "jpeg", "png", "gif", "webp", "heic", "zip", "7z", "rar", "gz", "tgz", "xz", "bz2", "zst", "lz4",
    "docx", "xlsx", "pptx", "odt", "ods", "odp", "epub", "jar", "apk", "mp3", "mp4", "m4a", "mkv", "avi",
    "mov", "webm", "ogg", "flac", "aac", "cab", "msi", "lrz", "lzd",
];
const IO_BUF: usize = 8 << 20;

pub struct Entry {
    pub path: String, // relative, separator '/'
    pub size: u64,
    /// File ranges in order: (length, lens). Without `--ranges` there is one range.
    pub ranges: Vec<(u64, Lens)>,
}

impl Entry {
    pub fn is_raw(&self) -> bool {
        self.ranges.iter().all(|r| r.1.is_raw())
    }
}

pub struct Options {
    pub preset: u32,
    pub group: bool,
    pub verbose: bool,
    /// Enabled lenses and selection mode; all disabled = base (plain solid LZMA).
    pub detect: Detect,
    /// The lens is chosen separately for every range of RANGE bytes.
    pub ranges: bool,
    /// Point 6: number of LZMA2 threads (1 = solid in a single block). Reading stays single-threaded.
    pub threads: u32,
    /// Skip the lens trial for known compressed extensions.
    pub skip_packed: bool,
}

impl Options {
    pub fn new(detect: Detect) -> Self {
        Options { preset: 9, group: true, verbose: false, detect, ranges: false, threads: 1, skip_packed: true }
    }
}

fn bad(m: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, m.into())
}

/// Walks the directory; returns relative paths of regular files.
fn walk(root: &Path) -> io::Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    let mut stack = vec![PathBuf::new()];
    while let Some(rel) = stack.pop() {
        let mut ents: Vec<_> = fs::read_dir(root.join(&rel))?.collect::<Result<_, _>>()?;
        ents.sort_by_key(|e| e.file_name());
        for e in ents.into_iter().rev() {
            let ft = e.file_type()?;
            let r = rel.join(e.file_name());
            if ft.is_dir() {
                stack.push(r);
            } else if ft.is_file() {
                out.push(r);
            }
        }
    }
    out.reverse();
    out.sort(); // stable, deterministic order
    Ok(out)
}

fn rel_string(p: &Path) -> io::Result<String> {
    let parts: Vec<String> = p
        .components()
        .map(|c| match c {
            Component::Normal(s) => s.to_str().map(String::from).ok_or_else(|| bad("path is not UTF-8")),
            _ => Err(bad("unexpected path component")),
        })
        .collect::<Result<_, _>>()?;
    Ok(parts.join("/"))
}

fn ext_of(p: &Path) -> String {
    p.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase()
}

/// Reads up to `n` bytes (or until EOF) into the buffer.
fn read_up_to(r: &mut impl Read, buf: &mut Vec<u8>, n: usize) -> io::Result<usize> {
    let start = buf.len();
    buf.resize(start + n, 0);
    let mut got = 0;
    while got < n {
        let k = r.read(&mut buf[start + got..])?;
        if k == 0 {
            break;
        }
        got += k;
    }
    buf.truncate(start + got);
    Ok(got)
}

pub fn create(src: &Path, dst: &Path, opt: &Options) -> io::Result<Vec<Entry>> {
    let mut files = walk(src)?;
    if opt.group {
        // Similar files next to each other: by extension, then by path.
        files.sort_by(|a, b| {
            ext_of(a).cmp(&ext_of(b)).then_with(|| a.cmp(b))
        });
    }

    let out = BufWriter::with_capacity(IO_BUF, File::create(dst)?);
    let mut out = CountingWriter { w: out, n: 0 };
    out.write_all(MAGIC)?;
    out.write_all(&[VERSION])?;

    let mut xz = if opt.threads > 1 {
        let st = xz2::stream::MtStreamBuilder::new()
            .threads(opt.threads)
            .preset(opt.preset)
            .block_size(64 << 20)
            .encoder()
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
        xz2::write::XzEncoder::new_stream(out, st)
    } else {
        xz2::write::XzEncoder::new(out, opt.preset)
    };
    let mut entries = Vec::with_capacity(files.len());
    let mut enc = Vec::with_capacity(IO_BUF);
    // Tail of the solid stream (encoded bytes), for lens selection in context.
    let mut tail: Vec<u8> = Vec::with_capacity(lens::CTX_BYTES * 2);
    let want_lens = opt.detect.sw.film || opt.detect.sw.bwt;

    for rel in &files {
        let mut f = BufReader::with_capacity(IO_BUF, File::open(src.join(rel))?);
        let probe = want_lens && !(opt.skip_packed && PACKED_EXT.contains(&ext_of(rel).as_str()));
        let range_len = if opt.ranges { RANGE } else { u64::MAX };
        let mut pending = Vec::with_capacity(IO_BUF);
        let mut eof = read_up_to(&mut f, &mut pending, IO_BUF)? == 0;
        let mut ranges: Vec<(u64, Lens)> = Vec::new();
        let mut size = 0u64;
        while !pending.is_empty() {
            // New range: the lens is chosen at its start.
            let head = pending.len().min(range_len as usize);
            let lens = if probe { lens::detect(&pending[..head], &opt.detect, &tail) } else { Lens::raw() };
            let cb = lens::chunk_bytes(&lens) as u64;
            let mut coder = Coder::new(&lens);
            let mut r_left = range_len;
            let mut r_len = 0u64;
            while r_left > 0 {
                let want = cb.min(r_left) as usize;
                while pending.len() < want && !eof {
                    eof = read_up_to(&mut f, &mut pending, IO_BUF.max(want))? == 0;
                }
                if pending.is_empty() {
                    break;
                }
                let n = want.min(pending.len());
                enc.clear();
                coder.encode(&pending[..n], &mut enc);
                xz.write_all(&enc)?;
                tail.extend_from_slice(&enc[enc.len().saturating_sub(lens::CTX_BYTES)..]);
                if tail.len() > lens::CTX_BYTES {
                    tail.drain(..tail.len() - lens::CTX_BYTES);
                }
                pending.drain(..n);
                r_left -= n as u64;
                r_len += n as u64;
                if n < want {
                    break; // EOF in the middle of a range
                }
            }
            size += r_len;
            ranges.push((r_len, lens));
            if pending.is_empty() && !eof {
                eof = read_up_to(&mut f, &mut pending, IO_BUF)? == 0;
            }
        }
        if ranges.is_empty() {
            ranges.push((0, Lens::raw()));
        }
        let path = rel_string(rel)?;
        let e = Entry { path, size, ranges };
        if opt.verbose {
            eprintln!("{:>12}  {:<40}  {}", size, describe_entry(&e), e.path);
        }
        entries.push(e);
    }

    let mut out = xz.finish()?;
    let table_off = out.n;
    let table = xz_bytes(&encode_table(&entries), 9)?;
    out.write_all(&table)?;
    out.write_all(&table_off.to_le_bytes())?;
    out.write_all(&(table.len() as u64).to_le_bytes())?;
    out.write_all(END)?;
    out.w.flush()?;
    Ok(entries)
}

struct CountingWriter<W: Write> {
    w: W,
    n: u64,
}
impl<W: Write> Write for CountingWriter<W> {
    fn write(&mut self, b: &[u8]) -> io::Result<usize> {
        let k = self.w.write(b)?;
        self.n += k as u64;
        Ok(k)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.w.flush()
    }
}

fn xz_bytes(d: &[u8], preset: u32) -> io::Result<Vec<u8>> {
    let mut e = xz2::write::XzEncoder::new(Vec::new(), preset);
    e.write_all(d)?;
    e.finish()
}

fn encode_table(es: &[Entry]) -> Vec<u8> {
    let mut t = Vec::new();
    t.extend_from_slice(&(es.len() as u64).to_le_bytes());
    for e in es {
        let p = e.path.as_bytes();
        t.extend_from_slice(&(p.len() as u32).to_le_bytes());
        t.extend_from_slice(p);
        t.extend_from_slice(&e.size.to_le_bytes());
        t.extend_from_slice(&(e.ranges.len() as u32).to_le_bytes());
        for (len, l) in &e.ranges {
            t.extend_from_slice(&len.to_le_bytes());
            t.push(l.kind as u8);
            t.extend_from_slice(&(l.l as u32).to_le_bytes());
            if l.kind == Kind::Film {
                t.extend_from_slice(&l.modes);
                t.extend_from_slice(&l.negative);
            }
        }
    }
    t
}

fn decode_table(t: &[u8]) -> io::Result<Vec<Entry>> {
    let mut p = 0usize;
    let mut take = |n: usize| -> io::Result<&[u8]> {
        let s = t.get(p..p + n).ok_or_else(|| bad("truncated table"))?;
        p += n;
        Ok(s)
    };
    let n = u64::from_le_bytes(take(8)?.try_into().unwrap());
    let mut es = Vec::new();
    for _ in 0..n {
        let pl = u32::from_le_bytes(take(4)?.try_into().unwrap()) as usize;
        let path = String::from_utf8(take(pl)?.to_vec()).map_err(|_| bad("path is not UTF-8"))?;
        let size = u64::from_le_bytes(take(8)?.try_into().unwrap());
        let nr = u32::from_le_bytes(take(4)?.try_into().unwrap()) as usize;
        let mut ranges = Vec::with_capacity(nr.min(1 << 16));
        let mut sum = 0u64;
        for _ in 0..nr {
            let len = u64::from_le_bytes(take(8)?.try_into().unwrap());
            let kind = Kind::from_u8(take(1)?[0]).ok_or_else(|| bad("unknown lens"))?;
            let l = u32::from_le_bytes(take(4)?.try_into().unwrap()) as usize;
            let (modes, negative) = if kind == Kind::Film {
                if l == 0 {
                    return Err(bad("film lens with L=0"));
                }
                let m = take(l)?.to_vec();
                if m.iter().any(|&x| x > lens::MODE_DELTA2) {
                    return Err(bad("unknown column mode"));
                }
                (m, take(l)?.to_vec())
            } else {
                (Vec::new(), Vec::new())
            };
            if matches!(kind, Kind::Bwt | Kind::BwtMtf) && l == 0 {
                return Err(bad("BWT block 0"));
            }
            sum = sum.checked_add(len).ok_or_else(|| bad("length overflow"))?;
            ranges.push((len, Lens { kind, l, modes, negative }));
        }
        if sum != size {
            return Err(bad("sum of ranges does not match the file size"));
        }
        es.push(Entry { path, size, ranges });
    }
    Ok(es)
}

/// Opens an archive: returns the table and the payload range.
pub fn read_table(path: &Path) -> io::Result<(Vec<Entry>, u64, u64)> {
    let mut f = File::open(path)?;
    let len = f.seek(SeekFrom::End(0))?;
    if len < 5 + 20 {
        return Err(bad("not an LZFD archive"));
    }
    let mut head = [0u8; 5];
    f.seek(SeekFrom::Start(0))?;
    f.read_exact(&mut head)?;
    if &head[..4] != MAGIC || head[4] != VERSION {
        return Err(bad("not an LZFD archive or unsupported version"));
    }
    let mut foot = [0u8; 20];
    f.seek(SeekFrom::Start(len - 20))?;
    f.read_exact(&mut foot)?;
    if &foot[16..] != END {
        return Err(bad("damaged archive end"));
    }
    let toff = u64::from_le_bytes(foot[0..8].try_into().unwrap());
    let tlen = u64::from_le_bytes(foot[8..16].try_into().unwrap());
    if toff < 5 || toff.checked_add(tlen).map_or(true, |e| e > len - 20) {
        return Err(bad("invalid footer"));
    }
    let mut tz = vec![0u8; tlen as usize];
    f.seek(SeekFrom::Start(toff))?;
    f.read_exact(&mut tz)?;
    let mut t = Vec::new();
    xz2::read::XzDecoder::new(&tz[..]).read_to_end(&mut t)?;
    Ok((decode_table(&t)?, 5, toff))
}

/// Safe destination path (no "..", no absolute paths).
fn safe_join(root: &Path, rel: &str) -> io::Result<PathBuf> {
    let mut p = root.to_path_buf();
    for part in rel.split('/') {
        if part.is_empty() || part == "." || part == ".." || part.contains(['\\', ':']) {
            return Err(bad(format!("unsafe path in archive: {rel}")));
        }
        p.push(part);
    }
    Ok(p)
}

/// Extraction. If `dst` is None, only verifies (decodes and discards).
/// `check` receives every file and its content in chunks, for verification.
pub fn extract(
    arch: &Path,
    dst: Option<&Path>,
    mut check: impl FnMut(&Entry, &[u8]) -> io::Result<()>,
) -> io::Result<Vec<Entry>> {
    let (entries, poff, pend) = read_table(arch)?;
    let mut f = File::open(arch)?;
    f.seek(SeekFrom::Start(poff))?;
    let r = BufReader::with_capacity(IO_BUF, f.take(pend - poff));
    let mut xz = xz2::read::XzDecoder::new(r);
    let mut res = Vec::with_capacity(IO_BUF);
    let mut dec = Vec::with_capacity(IO_BUF);
    for e in &entries {
        let mut w = match dst {
            Some(d) => {
                let p = safe_join(d, &e.path)?;
                if let Some(parent) = p.parent() {
                    fs::create_dir_all(parent)?;
                }
                Some(BufWriter::with_capacity(IO_BUF, File::create(p)?))
            }
            None => None,
        };
        for (len, l) in &e.ranges {
            let cb = lens::chunk_bytes(l) as u64;
            let mut coder = Coder::new(l);
            let mut left = *len;
            while left > 0 {
                let n = left.min(cb) as usize;
                let en = l.encoded_len(n);
                res.clear();
                if read_up_to(&mut xz, &mut res, en)? != en {
                    return Err(bad(format!("truncated payload at {}", e.path)));
                }
                dec.clear();
                coder.decode(&res, &mut dec);
                check(e, &dec)?;
                if let Some(w) = w.as_mut() {
                    w.write_all(&dec)?;
                }
                left -= n as u64;
            }
        }
        if let Some(mut w) = w {
            w.flush()?;
        }
    }
    Ok(entries)
}

/// Short lens description for printing.
pub fn describe(l: &Lens) -> String {
    match l.kind {
        Kind::Film => {
            let c = |m| l.modes.iter().filter(|&&x| x == m).count();
            format!("film L={} chain={} neg={} d2={} raw={}", l.l, c(lens::MODE_CHAIN), c(lens::MODE_NEG), c(lens::MODE_DELTA2), c(lens::MODE_RAW))
        }
        k => k.name().to_string(),
    }
}

/// File description: one lens, or a list of distinct lenses per range.
pub fn describe_entry(e: &Entry) -> String {
    if e.ranges.len() == 1 {
        return describe(&e.ranges[0].1);
    }
    let mut kinds: Vec<&str> = e.ranges.iter().map(|r| r.1.kind.name()).collect();
    kinds.dedup();
    format!("{} ranges: {}", e.ranges.len(), kinds.join(","))
}
