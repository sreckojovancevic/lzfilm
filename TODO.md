# TODO

## Research: comparison-free match finder (Recognition principle)

LZMA's encoder (BT4 match finder) has two parts:

1. **Hash heads (hash2/hash3/hash4)** — the next 2–4 bytes form an address that jumps directly to the last position where that sequence occurred. This is already the Recognition principle: direct addressing by identity, no comparison.
2. **Binary tree** — from the hash candidates, the longest match is found by *comparing* strings, and the tree is kept sorted. Most encoder time is spent here, waiting on memory (random access over the dictionary and match-finder structures, far larger than L2/L3).

**Idea:** replace the tree with a suffix array of the block built without comparisons (as in `src/bwt.rs`), plus a linear PSV/NSV pass (previous/next smaller value) that gives, for every position, the nearest earlier position with the longest common prefix. This is LZ77 factorization via suffix array: longest match per position in O(N), no tree, no string comparison.

**Caveats:**
- Not necessarily faster: the suffix array also accesses memory randomly (inverse SA, LCP), and costs 5–9 bytes per block byte.
- Block-based, not streaming: the whole block must be available up front.
- Changes the engine: needs an own LZMA encoder front-end (the output can still be standard LZMA, readable by xz), which departs from the "LZMA untouched" premise.

**Experiment:** on the same block, measure BT4 vs SA + PSV/NSV — time and lengths of the matches found. Connects to the `recognition_sort` work.

## Benchmarks pending

- [x] `lrzip -U -L 9` on the real VM disk set: 3.04 GiB, 11.2 GB RAM — 40% smaller than the chain at ~52× the RAM.
- [ ] Chain with `-p 9`, and a stronger v0 (long-range dedup inside the first version), to see how much of the lrzip gap closes at bounded RAM.
- [ ] `borg` (compression `lzma,6`) on the same v0/v1/v2 — the real competitor for versioned backups (content-defined dedup across the whole history).

## Engine (before any GUI)

- [ ] Multithreading per disk range (single-threaded LZMA ≈ 8–10 MB/s → ~35 h per 1 TB).
- [ ] Separate stream + checksum per version, so one damaged byte does not break everything after it.
- [ ] Restore without decoding the whole chain: periodic full versions or reverse incremental.
- [ ] Resume after interruption.
- [ ] Progress reporting (percent, speed, ETA) for long jobs.
- [ ] Lens selection: reduce false positives (e.g. the L=24 period on a `.so` file).

## Later

- [ ] Simple Windows GUI (backup / progress / restore) as a separate program calling `lzfilm.exe`; it should state clearly where the chain helps (large versioned images) and where it does not (already-compressed documents).
