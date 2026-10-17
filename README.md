# LZFILM

An experimental **lens** layer over LZMA. The base is plain solid LZMA; lenses are enabled with switches, and each one "looks at" a file in its own way before handing it to LZMA. The LZMA engine itself stays untouched — all the logic is in the lenses.

This is a research prototype. It does not aim to replace 7-Zip, xz or zstd. The benchmarks below report wins and losses alike.

| Switch | Lens | Meant for |
|---|---|---|
| (none) | base: plain solid LZMA | reference |
| `--film` | film: frame = row of length L, a mode per column | binary records, measurements, raw audio/images, tables |
| `--bwt` | comparison-free BWT (+ a variant with MTF) | text, logs, CSV |
| `--auto` | all lenses; picks the best one per file | mixed directories |

Extraction does not depend on switches: the archive table records which lens was applied to which file.

### Film lens

The file is viewed as a film: a frame is a row of fixed length L, and for every column (byte position within the frame) the coding is chosen:

| Mode | What it does | When it helps |
|---|---|---|
| `raw` | byte goes as is | text, noise, already compressed data |
| `chain` | difference to the same column in the previous frame | counters, timestamps, drifting signals, pixels |
| `neg` | difference to the "negative" (most frequent value of the column) | static background with occasional deviations |
| `delta2` | second derivative (optional, `--delta2`) | linear growth |

Residuals are written column by column (transposed, in ~1 MB segments). Transposition follows the Recognition principle: every byte goes to the position given by its identity (column index), by direct addressing, without comparisons, O(N), reversibly.

### BWT lens

The Burrows–Wheeler transform groups bytes with similar context. Rotations are sorted by prefix doubling in which every step is a counting sort: an element goes to the position given by its class, by direct addressing, without ever comparing two elements. The inverse is LF-mapping, also by counting, O(N). Works in 8 MB blocks; the `bwt+mtf` variant adds move-to-front.

### Lens selection

Selection is automatic: the period L is detected (autocorrelation on a sample), a mode is chosen per column (minimum residual entropy), and a trial compression of the sample checks whether the lens beats plain LZMA by at least 3%. If it does not, the file goes `raw`. A trial on an isolated sample cannot see the solid context, so the lens sometimes chooses wrongly (measured: +0.09% on 832 MB of office documents, and a false period on one `.so` file costing ~1 MB) — see Results.

### Chain of versions (`snap`)

A separate mode for disk images and snapshots: a frame is a whole version. Version i is coded against version i−1, block by block, and everything goes into one LZMA stream. Both versions are read sequentially, so RAM does not depend on the image size. With `--motion`, relocated 4 KB blocks are found by content anywhere in the previous version, and blocks unrelated to the previous version are stored without prediction.

## Build

```
cargo build --release
```

On Windows, `xz2` builds liblzma from source, so a C compiler is required (MSVC from Visual Studio Build Tools, or MinGW).

## Usage

```
lzfilm a <dir> <archive.lzd> [switches] [options]  pack; no switch = base (plain solid LZMA)
lzfilm x <archive.lzd> <destination>               extract
lzfilm l <archive.lzd>                             list files and lens per file
lzfilm t <archive.lzd>                             test (decode, write nothing)
lzfilm bench <dir> [options]                       base vs --film/--bwt/--auto + verification
lzfilm probe <file>                                what the lenses see in a file
lzfilm film-bench [<file> -L N] [-p 0-9]           v0.1 film modes, incl. B-frames
lzfilm snap c <out.lzs> <v0> <v1> ... [-p 0-9] [--motion]   chain of versions (disk images)
lzfilm snap x <in.lzs> <dir>                       extracts v0.img, v1.img, ...
```

| Option | What it does |
|---|---|
| `-p 0-9` | LZMA preset (default 9, 64 MB dictionary) |
| `-T N` | LZMA2 threads (64 MB blocks); reading stays single-threaded for HDDs |
| `--no-group` | do not group files by extension |
| `--ranges` | choose the lens separately for every 16 MB range (mixed lenses within a file) |
| `--fast-period` | periods only as powers of two, then ±10% around the best |
| `--proxy` | entropy estimate instead of trial compression |
| `--ctx` | trial measured as the increment after the last 512 KB of the solid stream |
| `--delta2` | allow the DELTA2 (second derivative) column mode |
| `--bwt-block N` | BWT block in MB (default 8) |
| `--no-skip` | try lenses on known compressed extensions too (jpg, zip, docx…) |
| `--motion` | (snap) detect relocated 4 KB blocks and blocks unrelated to the previous version |

## I/O (HDD)

Every file is read exactly once, sequentially, in 8 MB blocks; the first block is also used for lens detection. The archive is written forward only, with the file table at the end. Extraction is a single sequential pass as well. Several files are never read in parallel.

## Formats

`.lzd` (directory archive):
```
"LZFD" ver:u8 (=3)
payload   one xz stream — encoded content of all files in order
table     xz stream — per file: path, size, number of ranges, then per range:
          length, lens (0 raw, 1 film, 2 bwt, 3 bwt+mtf), L (for BWT: block size),
          for film also modes[L] (0 raw, 1 chain, 2 neg, 3 delta2) and negative[L]
          (a BWT block in the payload: primary index u32 + BWT output)
footer    table_off:u64 table_len:u64 "LZFE"
```

`.lzs` (chain of versions):
```
"LZSN" ver:u8 (=2) flags:u8 (bit 0 = motion) n:u32 n×size:u64, then one xz stream
with --motion, per 4 KB block: tag (0 same position, 1 moved + u64 source, 2 no prediction) + residual
```

## Results

All archives were verified bit-exact against the source. Ratios are relative to base (plain solid LZMA, same file order), xz preset 9.

### Lenses by data type

| Data | `--film` | `--bwt` | `--auto` |
|---|---|---|---|
| telemetry (64 B records) — film mode | −25% to −34% | — | — |
| mixed directory (telemetry, WAV, BMP, SQLite, CSV, code, binaries, archives), 91 MB | −3.55% | +0.11% | −4.0% |
| office documents (PDF, docx, xlsx, jpg), 62 MB and 250 MB | 0% | 0% | 0% |
| office documents, 832 MB (measured by the author on Windows, v0.3) | +0.09% | 0% | +0.09% |

The film lens pays off where raw structured binary data exists. On already-compressed formats no lens has anything to see; even fully unpacking docx/xlsx/PDF content yields only ~−4% as an upper bound, and Office deflate streams can be reproduced bit-exactly in <1% of cases, so that route is not viable.

### Suggested improvements — measured (mixed directory, 91 MB)

| Point | Option | Archive | vs base | Time | Conclusion |
|---|---|---|---|---|---|
| — | base | 38.60 MB | 0% | 52 s | reference |
| — | `--auto` (default) | 37.06 MB | −3.99% | 55 s | default |
| 1 | `--fast-period` | 36.05 MB | **−6.60%** | 174 s* | better, but by accident: it avoids a false period (L=24) on a `.so` file rather than finding a better one |
| 2 | `--proxy` | 40.06 MB | **+3.80%** | 69 s | worse than base — order-0 entropy cannot see LZ matches |
| 3 | one pass + NEG criterion | — | — | — | NEG entropy is always equal to RAW entropy (a shifted histogram); NEG is now chosen for mostly static columns |
| 4 | `--delta2` | 37.06 MB | −3.98% | — | no effect; LZMA packs a constant run almost as cheaply as zeros |
| 5 | `--ranges` | 37.06 MB | −3.99% | — | no effect on this set (no mixed files >16 MB) |
| 6 | `-T 2` (base) | 38.63 MB | +0.08% | 47 s | ~10% faster on 2 cores, negligible loss |
| 7 | `--bwt-block 32` | 37.06 MB | −3.99% | — | no effect |
| 9 | B-frames (telemetry, `film-bench`) | — | −32.9% vs −34.7% for the plain chain | — | worse than the plain chain |
| — | `--ctx` (selection in context) | 37.06 MB | −3.98% | 201 s | 3.7× slower, no gain; did not catch the false period either |

\* Times marked with an asterisk were measured while another test was running (2 cores); size comparisons are exact. Points 1, 2, 5, 7 and `--ctx` were measured with the then-default `--ctx --delta2`.

Point 10 (context mixing instead of LZMA) was deliberately not implemented: it replaces the engine, while the premise of the project is that LZMA stays untouched.

### Chain of disk versions (`snap`, point 8) — simulation

A 512 MB ext4 disk with real documents, 4 versions (between versions ~20 MB of new documents, 12 modified, 15 deleted, growing logs), free space filled with remnants of old files. 2 GB in total.

| Tool | Archive | Pack time | Pack RAM | Unpack RAM |
|---|---|---|---|---|
| xz -6 | 1384 MB | 807 s | 95 MB | — |
| xz -9 | 1080 MB | 980 s | 675 MB | — |
| lrzip -L 9 | **297 MB** | 99 s | 3886 MB | — |
| zstd -19 --long=31 | 299 MB | 201 s | 2321 MB | 2052 MB |
| xz -6 + xdelta3 -9 | 430 MB | 222 s | 258 MB | — |
| `snap -p 6` | 463 MB | 258 s | 112 MB | 22 MB |
| `snap -p 9` | 380 MB | 290 s | 692 MB | 22 MB |
| `snap -p 6 --motion` | 402 MB | 219 s | **124 MB** | 26 MB |
| `snap -p 9 --motion` | 318 MB | 260 s | 704 MB | 26 MB |

v0 alone through xz -9 is 265 MB, so the three following versions cost: zstd 34 MB, `snap -p 9 --motion` 54 MB.

Conclusion: the chain with `--motion` is **6–7% larger** than zstd `--long` / lrzip, with **3–5× less RAM when packing and ~80× less when unpacking**. The hypothesis "same ratio with less RAM" does not fully hold. Part of the gap is likely an artefact of the simulation: the free-space remnants contain the same files that are later "added", but at unaligned positions that zstd finds and a 4 KB block search does not.

### Chain of disk versions — real VM disk

Ubuntu VM in VMware Workstation, 3 snapshots: fresh install (powered off) → booted system (taken while running) → after `apt full-upgrade` (powered off). Each snapshot was flattened to a raw 10 GB disk with `vmware-vdiskmanager -r … -t 2`. Input: 3 × 10 GB = 30 GB. Windows, Intel i9, 32 GB RAM, single-threaded runs (`-T1` for zstd).

| Tool | Disk / OS | Archive | Pack time | Pack RAM | Unpack time | Unpack RAM |
|---|---|---|---|---|---|---|
| zstd -19 --long=31 (1 thread) | NVMe / Windows | 9.90 GiB (33.0%) | 6281 s | 3487 MB | — | — |
| **lrzip -U -L 9** (multi-threaded) | SATA SSD / WSL | **3.04 GiB (10.6%)** | 7518 s | **11 444 MB** | — | — |
| `snap -p 6 --motion` (1 thread) | HDD / Windows | 5.11 GiB (17.0%) | 3810 s | 220 MB | — | — |
| `snap -p 6 --motion` (1 thread) | NVMe / Windows | 5.11 GiB (17.0%) | 3635 s | 220 MB | 181 s | 28 MB |
| `snap -p 6 --motion` (1 thread) | SATA SSD / WSL | 5.11 GiB (17.0%) | 3016 s | 224 MB | — | — |

The chain archive is byte-identical across Windows and Linux. The Linux build is ~17% faster on the slower disk, which confirms the chain is CPU-bound (likely MSVC vs GCC builds of liblzma).

All three versions extracted from the chain archive were verified bit-exact (SHA-256) against the original flattened disks.

Breakdown of the 5.24 M blocks (4 KB) in v1 and v2: 88.9% unchanged, **5.1% relocated (~1.05 GB)** — same content found elsewhere in the previous version, likely apt cache → unpacked files and the ext4 journal — 5.7% new content (~1.17 GB), 0.3% changed in place. v0 alone compresses to 4.27 GiB, so v1 + v2 cost 0.84 GiB.

**zstd vs chain:** zstd's long-range window is at most 2 GB (`--long=31`). In the simulation the versions were 512 MB, inside the window, and zstd won. Here each version is 10 GB, so the previous version lies beyond the window and zstd packs v1 and v2 almost as new data. When a frame is larger than the compressor's window, the chain still sees the previous version — with constant RAM regardless of version size.

**lrzip vs chain — lrzip wins on size:** with `-U` (unlimited window) lrzip finds matches across all 30 GB at any alignment, including redundancy *inside* v0 itself (the same files and packages stored in several places on the disk). The chain only compares against the previous version and compresses v0 with an 8 MB dictionary (`-p 6`): v0 alone is 4.27 GiB in the chain, while lrzip packs all three versions into 3.04 GiB. Most of the gap is therefore in v0, not in the changes between versions. Part of it is also the preset (`-p 6` vs `-L 9`).

**Resulting claim:** the chain does not give the best ratio. It gives a good ratio at bounded, constant RAM: lrzip's archive is 40% smaller but needs ~52× more memory (11.2 GB vs 220 MB). The chain is useful where RAM is limited (hypervisor host, NAS, small server) or where an image is larger than the available RAM; where RAM is plentiful, lrzip is the better tool for this data.

On HDD vs NVMe the chain differs by only 5% in time: it is CPU-bound (single-threaded LZMA), not disk-bound.

## Modules

- `src/lens.rs` — recognition, lens selection by switches, coding (streamed, in chunks)
- `src/bwt.rs` — comparison-free BWT, inverse, MTF
- `src/snap.rs` — chain of disk versions, streamed; `--motion` for relocated blocks
- `src/archive.rs` — directory archiver
- `src/film.rs` — v0.1: sequence of fixed-length frames with GOPs, B-frames and random access

See [IDEA.md](IDEA.md) for the original "Recognition Compression Lens" idea sketch.

## License

MIT — see [LICENSE](LICENSE).

## Authors

- Srećko Jovančević — idea (film / negative / lens approach), direction, testing

## Acknowledgments

- Claude (Anthropic) — code elaboration and codebase extension
