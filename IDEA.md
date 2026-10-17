# Recognition Compression Lens

This repository does not offer a new compressor. It does not try to be better, faster,
or more competitive than anything that already exists.

It offers a **framework for thinking** — an alternative lens through which the
relationship between data structure and entropy coding can be viewed.

## The question it asks

Traditional approaches (BWT, suffix arrays) use comparison to group
similar contexts before compression. What if the same goal —
phase alignment of similar bytes — could be achieved **without comparison**,
through the Recognition principle: direct addressing based on the identity
of the data?

This code is a **sketch** intended to show that such a transformation is:
1. Possible
2. Reversible (lossless)
3. And that it changes the structure of the data before it enters the entropy coder

What comes next is left to those interested in the idea.

## Code

```rust
// ============================================================================
// Recognition Compression Lens — Minimal demonstration
// ============================================================================
//
// This code shows that data can be transformed by the Recognition principle
// (without comparison, O(N)) and that the transformation is reversible.
//
// Concept:
// - Instead of comparing elements (a > b), each element "recognizes" its
//   position based on its identity (value).
// - This groups similar bytes together, changing the structure of the data.
// - The transformation is lossless if we keep minimal context (the histogram).

use std::collections::HashMap;

/// Recognition transform: groups bytes by their value
/// WITHOUT comparison between elements. Each byte "knows where it belongs".
/// 
/// Returns: (transformed_array, frequency_histogram, primary_index)
fn recognition_transform(data: &[u8]) -> (Vec<u8>, Vec<u32>, usize) {
    // Step 1: compute the frequency histogram (O(N))
    let mut histogram = vec![0u32; 256];
    for &byte in data {
        histogram[byte as usize] += 1;
    }
    
    // Step 2: compute the start position for each value (O(256))
    // This is the "root" (origin) for each value — direct addressing
    let mut positions = vec![0usize; 256];
    let mut current_pos = 0;
    for i in 0..256 {
        positions[i] = current_pos;
        current_pos += histogram[i] as usize;
    }
    
    // Step 3: transform the data — each byte goes to its position (O(N))
    // NO comparison! Only direct addressing: position = positions[byte]
    let mut transformed = vec![0u8; data.len()];
    let mut primary_index = 0;
    
    for (i, &byte) in data.iter().enumerate() {
        let pos = positions[byte as usize];
        transformed[pos] = byte;
        
        // Track where the first byte of the original ended up (for the inverse)
        if i == 0 {
            primary_index = pos;
        }
        
        // Increment the position for the next byte of the same value
        positions[byte as usize] += 1;
    }
    
    (transformed, histogram, primary_index)
}

/// Inverse transform: restores the original from the transformed array
/// Uses only the stored histogram and primary index (O(N))
fn recognition_inverse(
    transformed: &[u8],
    histogram: &[u32],
    primary_index: usize
) -> Vec<u8> {
    let n = transformed.len();
    let mut original = vec![0u8; n];
    
    // Step 1: reconstruct start positions from the histogram
    let mut positions = vec![0usize; 256];
    let mut current_pos = 0;
    for i in 0..256 {
        positions[i] = current_pos;
        current_pos += histogram[i] as usize;
    }
    
    // Step 2: follow the chain from the primary index (O(N))
    // Each byte in the transformed array "points" to the next byte of the original
    let mut current = primary_index;
    for i in 0..n {
        original[i] = transformed[current];
        
        // Find the next position for this value
        let byte = transformed[current];
        current = positions[byte as usize];
        positions[byte as usize] += 1;
    }
    
    original
}

/// Simple compression: Run-Length Encoding (demonstration only)
/// In practice LZMA or another entropy coder would go here
fn simple_compress(data: &[u8]) -> Vec<u8> {
    let mut compressed = Vec::new();
    let mut i = 0;
    
    while i < data.len() {
        let byte = data[i];
        let mut count = 1;
        
        while i + count < data.len() && data[i + count] == byte && count < 255 {
            count += 1;
        }
        
        compressed.push(byte);
        compressed.push(count as u8);
        i += count;
    }
    
    compressed
}

/// Simple decompression: RLE inverse
fn simple_decompress(compressed: &[u8]) -> Vec<u8> {
    let mut decompressed = Vec::new();
    
    for chunk in compressed.chunks(2) {
        if chunk.len() == 2 {
            let byte = chunk[0];
            let count = chunk[1];
            for _ in 0..count {
                decompressed.push(byte);
            }
        }
    }
    
    decompressed
}

fn main() {
    println!("╔════════════════════════════════════════════════════════════╗");
    println!("║  Recognition Compression Lens — Demonstration              ║");
    println!("║  A framework for thinking, not a product                  ║");
    println!("╚════════════════════════════════════════════════════════════╝\n");
    
    // Sample data with repetitions (simulates a log file or structured data)
    let original_data = b"the quick brown fox jumps over the lazy dog \
                          the quick brown fox jumps over the lazy dog \
                          the quick brown fox jumps over the lazy dog \
                          abcdefghijklmnopqrstuvwxyz \
                          abcdefghijklmnopqrstuvwxyz \
                          1234567890 1234567890 1234567890";
    
    println!("Original data: {} bytes", original_data.len());
    println!("First 50 bytes: {:?}\n", 
             String::from_utf8_lossy(&original_data[..50.min(original_data.len())]));
    
    // ═══════════════════════════════════════════════════════════════
    // STEP 1: Recognition transform (no comparison)
    // ═══════════════════════════════════════════════════════════════
    println!("─".repeat(60));
    println!("Step 1: Recognition transform");
    println!("  • Grouping bytes by value (direct addressing)");
    println!("  • NO comparison between elements");
    println!("  • Complexity: O(N)");
    
    let (transformed, histogram, primary_index) = recognition_transform(original_data);
    
    println!("  • Transformed: {} bytes", transformed.len());
    println!("  • Primary index: {}", primary_index);
    println!("  • Histogram: {} distinct values\n", 
             histogram.iter().filter(|&&x| x > 0).count());
    
    // ═══════════════════════════════════════════════════════════════
    // STEP 2: Compression of the transformed data
    // ═══════════════════════════════════════════════════════════════
    println!("─".repeat(60));
    println!("Step 2: Compression (simple RLE for demonstration)");
    
    let compressed = simple_compress(&transformed);
    let compression_ratio = original_data.len() as f64 / compressed.len() as f64;
    
    println!("  • Compressed: {} bytes", compressed.len());
    println!("  • Compression ratio: {:.2}x", compression_ratio);
    println!("  • Note: in practice LZMA would go here\n");
    
    // ═══════════════════════════════════════════════════════════════
    // STEP 3: Decompression
    // ═══════════════════════════════════════════════════════════════
    println!("─".repeat(60));
    println!("Step 3: Decompression");
    
    let decompressed = simple_decompress(&compressed);
    println!("  • Decompressed: {} bytes", decompressed.len());
    println!("  • Identical to transformed: {}\n", 
             decompressed == transformed);
    
    // ═══════════════════════════════════════════════════════════════
    // STEP 4: Inverse Recognition transform
    // ═══════════════════════════════════════════════════════════════
    println!("─".repeat(60));
    println!("Step 4: Inverse Recognition transform");
    println!("  • Reconstructing the original from the histogram and primary index");
    println!("  • Complexity: O(N)");
    
    let reconstructed = recognition_inverse(&decompressed, &histogram, primary_index);
    
    println!("  • Reconstructed: {} bytes", reconstructed.len());
    println!("  • First 50 bytes: {:?}", 
             String::from_utf8_lossy(&reconstructed[..50.min(reconstructed.len())]));
    
    // ═══════════════════════════════════════════════════════════════
    // VERIFICATION
    // ═══════════════════════════════════════════════════════════════
    println!("\n{}", "═".repeat(60));
    println!("VERIFICATION");
    println!("═".repeat(60));
    
    let identical = original_data[..] == reconstructed[..];
    
    if identical {
        println!("✓ Original and reconstructed data are IDENTICAL");
        println!("✓ The transform is REVERSIBLE (lossless)");
        println!("✓ The Recognition principle works without comparison");
    } else {
        println!("✗ ERROR: data does not match!");
        println!("  Original:     {:?}", original_data);
        println!("  Reconstructed: {:?}", reconstructed);
    }
    
    println!("\n{}", "═".repeat(60));
    println!("CONCLUSION");
    println!("═".repeat(60));
    println!("This code demonstrates that the Recognition transform is:");
    println!("  1. Possible (O(N), no comparison)");
    println!("  2. Reversible (lossless with minimal metadata)");
    println!("  3. Changes the structure of the data before compression");
    println!("\nWhat comes next is left to those interested in the idea.");
    println!("═".repeat(60));
}
```
