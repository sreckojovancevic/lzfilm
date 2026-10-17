use lzfilm::archive::{self, Entry, Options};
use lzfilm::lens::{Detect, Switches};
use std::fs::File;
use std::io::{self, BufReader, Read};
use std::path::{Path, PathBuf};
use std::time::Instant;
use std::{env, fs, process};

fn usage() -> ! {
    eprintln!(
        r#"lzfilm — a lens layer over LZMA

  lzfilm a <dir> <archive.lzd> [switches] [options]  pack; no switch = base (plain solid LZMA)
  lzfilm x <archive.lzd> <destination>               extract
  lzfilm l <archive.lzd>                             list files and lens per file
  lzfilm t <archive.lzd>                             test (decode, write nothing)
  lzfilm bench <dir> [options]                       base vs --film/--bwt/--auto + verification
  lzfilm probe <file>                                what the lenses see in a file
  lzfilm film-bench [<file> -L N] [-p 0-9]           v0.1 film modes, incl. B-frames
  lzfilm snap c <out.lzs> <v0> <v1> ... [-p 0-9] [--motion]
                                                     chain of versions (disk images, snapshots)
  lzfilm snap x <in.lzs> <dir>                       extracts v0.img, v1.img, ...

switches:
  --film          period L, chain/negative per column (binary records, raw audio/images)
  --bwt           comparison-free BWT (+MTF) (text, logs)
  --auto          all lenses; picks the best one per file

options:
  -p 0-9          LZMA preset (default 9)
  -T N            LZMA2 threads (64 MB blocks; slightly worse ratio)
  --no-group      do not group files by extension
  --ranges        choose the lens per 16 MB range (mixed lenses within a file)
  --fast-period   periods only as powers of two ±10%
  --proxy         entropy estimate instead of trial compression
  --ctx           trial in the context of the solid stream (~3.7× slower)
  --delta2        allow the DELTA2 (second derivative) column mode
  --bwt-block N   BWT block in MB (default 8)
  --no-skip       try lenses on known compressed extensions too
  -v              print the lens per file
  --motion        (snap) detect relocated 4 KB blocks"#
    );
    process::exit(2)
}

fn has(a: &[String], f: &str) -> bool {
    a.iter().any(|x| x == f)
}

fn preset(a: &[String]) -> u32 {
    a.iter()
        .position(|x| x == "-p")
        .and_then(|i| a.get(i + 1))
        .map(|s| s.parse().unwrap_or_else(|_| usage()))
        .unwrap_or(9)
}

fn die(e: io::Error) -> ! {
    eprintln!("error: {e}");
    process::exit(1)
}

fn mb(b: u64) -> f64 {
    b as f64 / 1048576.0
}

fn describe(e: &Entry) -> String {
    archive::describe_entry(e)
}

fn num(a: &[String], k: &str) -> Option<u64> {
    a.iter().position(|x| x == k).and_then(|i| a.get(i + 1)).map(|s| s.parse().unwrap_or_else(|_| usage()))
}

/// All switches and lens-selection options from the command line.
fn options(a: &[String], sw: Switches) -> Options {
    let mut d = Detect::new(sw);
    d.fast_period = has(a, "--fast-period");
    d.proxy = has(a, "--proxy");
    d.ctx = has(a, "--ctx");
    d.delta2 = has(a, "--delta2");
    if let Some(mb) = num(a, "--bwt-block") {
        d.bwt_block = (mb.clamp(1, 512) as usize) << 20;
    }
    let mut o = Options::new(d);
    o.preset = preset(a);
    o.group = !has(a, "--no-group");
    o.verbose = has(a, "-v");
    o.ranges = has(a, "--ranges");
    o.threads = num(a, "-T").unwrap_or(1).clamp(1, 64) as u32;
    o.skip_packed = !has(a, "--no-skip");
    o
}

fn switches(a: &[String]) -> Switches {
    let auto = has(a, "--auto");
    Switches { film: auto || has(a, "--film"), bwt: auto || has(a, "--bwt") }
}

/// Verifies that the extracted content matches the source directory bit for bit.
fn verify(arch: &Path, src: &Path) -> io::Result<()> {
    let mut cur: Option<(String, BufReader<File>)> = None;
    let mut buf = Vec::new();
    archive::extract(arch, None, |e, chunk| {
        if cur.as_ref().map_or(true, |c| c.0 != e.path) {
            let p: PathBuf = e.path.split('/').fold(src.to_path_buf(), |p, s| p.join(s));
            cur = Some((e.path.clone(), BufReader::with_capacity(8 << 20, File::open(p)?)));
        }
        let r = &mut cur.as_mut().unwrap().1;
        buf.resize(chunk.len(), 0);
        r.read_exact(&mut buf)?;
        if buf != chunk {
            return Err(io::Error::new(io::ErrorKind::InvalidData, format!("MISMATCH in {}", e.path)));
        }
        Ok(())
    })?;
    Ok(())
}

fn main() {
    let a: Vec<String> = env::args().collect();
    match a.get(1).map(String::as_str) {
        Some("a") if a.len() >= 4 => {
            let o = options(&a, switches(&a));
            let t = Instant::now();
            let es = archive::create(Path::new(&a[2]), Path::new(&a[3]), &o).unwrap_or_else(|e| die(e));
            let total: u64 = es.iter().map(|e| e.size).sum();
            let out = fs::metadata(&a[3]).map(|m| m.len()).unwrap_or(0);
            println!("{} files, {:.1} MB → {:.1} MB ({:.2}%), {:.1} s",
                es.len(), mb(total), mb(out), 100.0 * out as f64 / total.max(1) as f64, t.elapsed().as_secs_f64());
        }
        Some("x") if a.len() >= 4 => {
            let t = Instant::now();
            let es = archive::extract(Path::new(&a[2]), Some(Path::new(&a[3])), |_, _| Ok(())).unwrap_or_else(|e| die(e));
            println!("{} files extracted, {:.1} s", es.len(), t.elapsed().as_secs_f64());
        }
        Some("l") if a.len() >= 3 => {
            let (es, _, _) = archive::read_table(Path::new(&a[2])).unwrap_or_else(|e| die(e));
            for e in &es {
                println!("{:>14}  {:<40}  {}", e.size, describe(e), e.path);
            }
        }
        Some("t") if a.len() >= 3 => {
            let t = Instant::now();
            let es = archive::extract(Path::new(&a[2]), None, |_, _| Ok(())).unwrap_or_else(|e| die(e));
            println!("OK, {} files, {:.1} s", es.len(), t.elapsed().as_secs_f64());
        }
        Some("snap") if a.len() >= 5 && a[2] == "c" => {
            let vs: Vec<PathBuf> = a[4..].iter().filter(|x| !x.starts_with('-') && x.parse::<u32>().is_err()).map(PathBuf::from).collect();
            let t = Instant::now();
            let st = lzfilm::snap::compress(&vs, Path::new(&a[3]), preset(&a), has(&a, "--motion")).unwrap_or_else(|e| die(e));
            if has(&a, "--motion") {
                println!("4 KB blocks in versions 1..: unchanged {}, changed in place {}, moved {}, no prediction {}", st.same_zero, st.same, st.moved, st.raw);
            }
            let total: u64 = vs.iter().map(|v| fs::metadata(v).map(|m| m.len()).unwrap_or(0)).sum();
            let out = fs::metadata(&a[3]).map(|m| m.len()).unwrap_or(0);
            println!("{} versions, {:.1} MB → {:.2} MB, {:.1} s", vs.len(), mb(total), mb(out), t.elapsed().as_secs_f64());
        }
        Some("snap") if a.len() >= 5 && a[2] == "x" => {
            let t = Instant::now();
            let vs = lzfilm::snap::extract(Path::new(&a[3]), Path::new(&a[4])).unwrap_or_else(|e| die(e));
            println!("{} versions extracted to {}, {:.1} s", vs.len(), a[4], t.elapsed().as_secs_f64());
        }
        Some("film-bench") => {
            // v0.1 film mode (frames inside one file): mode comparison, incl. B-frames
            use lzfilm::film::{self, Mode, Params};
            let (data, l, name) = match a.get(2).filter(|s| !s.starts_with('-')) {
                Some(f) => (fs::read(f).unwrap_or_else(|e| die(e)), num(&a, "-L").unwrap_or_else(|| usage()) as usize, f.clone()),
                None => {
                    // synthetic telemetry: 64 B record, 50 000 records
                    let mut d = Vec::new();
                    let mut x: u64 = 0x9E3779B97F4A7C15;
                    let mut s = [20.0f32, 1013.0, 50.0, 3.3, 0.0, 120.0, 7.5, 60.0];
                    for i in 0..50_000u64 {
                        d.extend_from_slice(&(1_700_000_000_000u64 + i * 1000).to_le_bytes());
                        d.extend_from_slice(&(i as u32).to_le_bytes());
                        for v in s.iter_mut() {
                            x ^= x << 13; x ^= x >> 7; x ^= x << 17;
                            *v += ((x >> 40) as f32 / 16777216.0 - 0.5) * 0.02;
                            d.extend_from_slice(&v.to_le_bytes());
                        }
                        d.extend_from_slice(&[0u8; 20]);
                    }
                    (d, 64, "synthetic telemetry".to_string())
                }
            };
            let p = preset(&a);
            let n = (data.len() + l - 1) / l;
            let mut e = xz2::write::XzEncoder::new(Vec::new(), p);
            std::io::Write::write_all(&mut e, &data).unwrap();
            let base = e.finish().unwrap().len();
            println!("{name}: {} B, L={l}, {n} frames, preset {p}", data.len());
            println!("{:<30} {:>10} B  {:>8}", "xz (no lens)", base, "+0.00%");
            for gop in [256usize, n.max(1)] {
                for m in [Mode::Sub, Mode::SubTranspose, Mode::BFrames, Mode::BFramesTranspose] {
                    let c = film::compress(&data, &Params { frame_len: l, gop, mode: m, preset: p }).unwrap();
                    assert_eq!(film::decompress(&c).unwrap(), data);
                    let g = if gop >= n { "all".to_string() } else { gop.to_string() };
                    println!("{:<30} {:>10} B  {:>+7.2}%", format!("{m:?} gop={g}"), c.len(), 100.0 * (c.len() as f64 / base as f64 - 1.0));
                }
            }
        }
        Some("probe") if a.len() >= 3 => {
            let mut d = Vec::new();
            File::open(&a[2]).and_then(|f| f.take(8 << 20).read_to_end(&mut d)).unwrap_or_else(|e| die(e));
            for (name, sw) in [("--film", Switches { film: true, bwt: false }), ("--bwt", Switches { film: false, bwt: true }), ("--auto", Switches { film: true, bwt: true })] {
                let l = lzfilm::lens::detect(&d, &options(&a, sw).detect, &[]);
                println!("{:<8} {}", name, archive::describe(&l));
            }
        }
        Some("bench") if a.len() >= 3 => {
            let src = Path::new(&a[2]);
            let group = !has(&a, "--no-group");
            let p = preset(&a);
            let runs = [
                ("base (plain solid LZMA)", Switches { film: false, bwt: false }),
                ("--film", Switches { film: true, bwt: false }),
                ("--bwt", Switches { film: false, bwt: true }),
                ("--auto", Switches { film: true, bwt: true }),
            ];
            let mut base = 0u64;
            let mut total = 0u64;
            let mut lines = Vec::new();
            let mut last: Vec<Entry> = Vec::new();
            for (i, (name, sw)) in runs.iter().enumerate() {
                let path = env::temp_dir().join(format!("lzfilm_bench_{i}.lzd"));
                let t = Instant::now();
                let mut o = options(&a, *sw);
                o.verbose = false;
                let es = archive::create(src, &path, &o).unwrap_or_else(|e| die(e));
                let ct = t.elapsed().as_secs_f64();
                let sz = fs::metadata(&path).unwrap().len();
                total = es.iter().map(|e| e.size).sum();
                if i == 0 {
                    base = sz;
                }
                let t = Instant::now();
                verify(&path, src).unwrap_or_else(|e| die(e));
                let dt = t.elapsed().as_secs_f64();
                let _ = fs::remove_file(&path);
                let n_l = es.iter().filter(|e| !e.is_raw()).count();
                lines.push(format!("{:<24} {:>9.2} MB  {:>6.2}%  {:>+7.2}%  {:>6.1} s  {:>5.1} s  {:>4}",
                    name, mb(sz), 100.0 * sz as f64 / total.max(1) as f64, 100.0 * (sz as f64 / base as f64 - 1.0), ct, dt, n_l));
                eprintln!("  done: {name}");
                last = es;
            }
            println!("\nInput: {} files, {:.1} MB  (preset {p}, grouping {})", last.len(), mb(total), if group { "on" } else { "off" });
            println!("\n{:<24} {:>12}  {:>7}  {:>8}  {:>8}  {:>7}  {:>4}", "switch", "archive", "ratio", "vs base", "pack", "unpack*", "lens");
            for l in &lines {
                println!("{l}");
            }
            println!("* unpack includes a bit-exact comparison with the source (all passed)");
            let mut by: std::collections::BTreeMap<String, (usize, u64)> = Default::default();
            for e in last.iter().filter(|e| !e.is_raw()) {
                let k = describe(e).split(' ').next().unwrap().to_string();
                let v = by.entry(k).or_default();
                v.0 += 1;
                v.1 += e.size;
            }
            if !by.is_empty() {
                println!("\n--auto, lenses per file:");
                for (k, (n, b)) in by {
                    println!("   {:<8} {:>5} files  {:>9.1} MB", k, n, mb(b));
                }
            }
        }
        _ => usage(),
    }
}
