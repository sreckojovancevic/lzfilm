//! BWT without comparisons (Recognition principle).
//!
//! Cyclic rotations are sorted by prefix doubling, where every step is a
//! counting sort: an element goes to the position given by its class (identity),
//! by direct addressing. Two elements are never compared with `<`.
//! Complexity O(N log N) worst case; stops as soon as all classes are distinct.
//!
//! The inverse is LF-mapping, also by counting, O(N).

/// Returns (BWT output, primary index = row holding the original rotation).
pub fn forward(s: &[u8]) -> (Vec<u8>, u32) {
    let n = s.len();
    if n == 0 {
        return (Vec::new(), 0);
    }
    let mut p = vec![0u32; n];
    let mut c = vec![0u32; n];
    let mut pn = vec![0u32; n];
    let mut cn = vec![0u32; n];

    // Phase 0: counting sort by the byte itself.
    let mut cnt = vec![0u32; 256.max(n)];
    for &b in s {
        cnt[b as usize] += 1;
    }
    for i in 1..256 {
        cnt[i] += cnt[i - 1];
    }
    for i in (0..n).rev() {
        let b = s[i] as usize;
        cnt[b] -= 1;
        p[cnt[b] as usize] = i as u32;
    }
    let mut classes = 1u32;
    c[p[0] as usize] = 0;
    for i in 1..n {
        if s[p[i] as usize] != s[p[i - 1] as usize] {
            classes += 1;
        }
        c[p[i] as usize] = classes - 1;
    }

    // Phases h: rotations of length 2^(h+1) are a pair (class of left half, class of right half).
    let mut h = 0usize;
    while (1usize << h) < n && (classes as usize) < n {
        let k = 1usize << h;
        // Order by the right half is already given by p, shifted by k.
        for i in 0..n {
            let v = p[i] as usize;
            pn[i] = ((v + n - k % n) % n) as u32;
        }
        // Stable counting sort by the class of the left half.
        cnt[..classes as usize].iter_mut().for_each(|x| *x = 0);
        for i in 0..n {
            cnt[c[pn[i] as usize] as usize] += 1;
        }
        for i in 1..classes as usize {
            cnt[i] += cnt[i - 1];
        }
        for i in (0..n).rev() {
            let cl = c[pn[i] as usize] as usize;
            cnt[cl] -= 1;
            p[cnt[cl] as usize] = pn[i];
        }
        // New classes: equal if both halves are equal.
        cn[p[0] as usize] = 0;
        classes = 1;
        for i in 1..n {
            let a = p[i] as usize;
            let b = p[i - 1] as usize;
            if c[a] != c[b] || c[(a + k) % n] != c[(b + k) % n] {
                classes += 1;
            }
            cn[a] = classes - 1;
        }
        std::mem::swap(&mut c, &mut cn);
        h += 1;
    }

    let mut out = Vec::with_capacity(n);
    let mut primary = 0u32;
    for (row, &start) in p.iter().enumerate() {
        let start = start as usize;
        if start == 0 {
            primary = row as u32;
        }
        out.push(s[(start + n - 1) % n]);
    }
    (out, primary)
}

/// Inverse via LF-mapping.
pub fn inverse(l: &[u8], primary: u32) -> Vec<u8> {
    let n = l.len();
    if n == 0 {
        return Vec::new();
    }
    // C[b] = number of bytes smaller than b; occ = occurrence rank in L.
    let mut count = [0u32; 256];
    for &b in l {
        count[b as usize] += 1;
    }
    let mut start = [0u32; 256];
    let mut sum = 0;
    for b in 0..256 {
        start[b] = sum;
        sum += count[b];
    }
    // lf[i] = row where the rotation of row i shifted left by 1 starts
    let mut lf = vec![0u32; n];
    let mut seen = [0u32; 256];
    for (i, &b) in l.iter().enumerate() {
        lf[i] = start[b as usize] + seen[b as usize];
        seen[b as usize] += 1;
    }
    let mut out = vec![0u8; n];
    let mut row = primary as usize;
    for k in (0..n).rev() {
        out[k] = l[row];
        row = lf[row] as usize;
    }
    out
}

/// Move-to-front: after BWT, repetitions become small values (mostly 0).
pub fn mtf(d: &mut [u8]) {
    let mut t: [u8; 256] = std::array::from_fn(|i| i as u8);
    for x in d.iter_mut() {
        let v = *x;
        let i = t.iter().position(|&y| y == v).unwrap();
        t.copy_within(0..i, 1);
        t[0] = v;
        *x = i as u8;
    }
}

pub fn imtf(d: &mut [u8]) {
    let mut t: [u8; 256] = std::array::from_fn(|i| i as u8);
    for x in d.iter_mut() {
        let i = *x as usize;
        let v = t[i];
        t.copy_within(0..i, 1);
        t[0] = v;
        *x = v;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn naive(s: &[u8]) -> (Vec<u8>, u32) {
        let n = s.len();
        let mut rots: Vec<usize> = (0..n).collect();
        rots.sort_by(|&a, &b| {
            (0..n).map(|k| s[(a + k) % n]).cmp((0..n).map(|k| s[(b + k) % n])).then(a.cmp(&b))
        });
        let out = rots.iter().map(|&r| s[(r + n - 1) % n]).collect();
        let p = rots.iter().position(|&r| r == 0).unwrap() as u32;
        (out, p)
    }

    #[test]
    fn bwt_matches_naive_and_inverts() {
        let mut seed = 1u64;
        let mut rnd = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        let mut cases: Vec<Vec<u8>> = vec![
            b"banana".to_vec(),
            b"abracadabra".to_vec(),
            b"aaaaaaa".to_vec(),
            b"abababab".to_vec(),
            b"x".to_vec(),
            b"the quick brown fox jumps over the lazy dog the quick brown fox".to_vec(),
        ];
        for _ in 0..200 {
            let n = (rnd() % 300) as usize + 1;
            let alpha = (rnd() % 4) as u8 + 1;
            cases.push((0..n).map(|_| (rnd() % alpha as u64) as u8).collect());
        }
        for s in &cases {
            let (l, p) = forward(s);
            let (ln, _) = naive(s);
            assert_eq!(l, ln, "BWT output for {:?}", s);
            assert_eq!(inverse(&l, p), *s, "inverse for {:?}", s);
            let mut m = l.clone();
            mtf(&mut m);
            imtf(&mut m);
            assert_eq!(m, l);
        }
    }
}
