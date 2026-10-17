//! LZFILM — a "lens" layer over LZMA.
//!
//! - `lens`    recognizes file structure (period L, negative/chain per column)
//! - `archive` directory archiver (solid LZMA2, sequential I/O)
//! - `bwt`     comparison-free BWT (counting-sort prefix doubling) + MTF
//! - `snap`    chain of versions (snapshots): coded against the previous version, streamed
//! - `film`    v0.1: sequence of fixed-length frames with GOPs and random access

pub mod archive;
pub mod bwt;
pub mod film;
pub mod lens;
pub mod snap;
