// Optimized masking implementation with SIMD support

use std::mem::MaybeUninit;

/// Mask/unmask a frame with optimal strategy selection.
///
/// This function automatically selects the fastest masking implementation based on:
/// - Buffer size
/// - CPU features (AVX2, NEON when available)
/// - Architecture alignment
#[inline(always)]
pub fn apply_mask(buf: &mut [u8], mask: [u8; 4]) {
    #[cfg(all(target_arch = "aarch64", target_feature = "neon"))]
    if buf.len() >= 16 {
        // SAFETY: NEON is guaranteed by target_feature.
        unsafe {
            return apply_mask_neon(buf, mask);
        }
    }

    if buf.len() <= 128 {
        return apply_mask_fast32(buf, mask);
    }

    // Try SIMD implementations first for larger buffers
    #[cfg(any(target_arch = "x86_64", target_arch = "x86"))]
    if buf.len() >= 256 && std::is_x86_feature_detected!("avx2") {
        // SAFETY: The runtime check guarantees AVX2 support.
        unsafe {
            return apply_mask_avx2(buf, mask);
        }
    }

    apply_mask_fast64(buf, mask);
}

/// Copy a payload into uninitialized output while applying its mask.
#[inline]
pub(crate) fn copy_mask(src: &[u8], dst: &mut [MaybeUninit<u8>], mask: [u8; 4]) {
    assert_eq!(src.len(), dst.len());
    #[cfg(any(target_arch = "x86_64", target_arch = "x86"))]
    if src.len() >= 256 && std::is_x86_feature_detected!("avx2") {
        // SAFETY: The slices have equal lengths and cannot overlap; AVX2 is available.
        unsafe {
            xor_avx2(src.as_ptr(), dst.as_mut_ptr().cast(), src.len(), mask);
        }
        return;
    }

    let mut input = src.chunks_exact(4);
    let mut output = dst.chunks_exact_mut(4);
    for (source, target) in input.by_ref().zip(output.by_ref()) {
        let word = u32::from_ne_bytes([source[0], source[1], source[2], source[3]]);
        let bytes = (word ^ u32::from_ne_bytes(mask)).to_ne_bytes();
        for (slot, byte) in target.iter_mut().zip(bytes) {
            slot.write(byte);
        }
    }
    for ((slot, byte), mask) in output
        .into_remainder()
        .iter_mut()
        .zip(input.remainder())
        .zip(mask)
    {
        slot.write(byte ^ mask);
    }
}

/// A safe unoptimized mask application.
#[inline(always)]
fn apply_mask_fallback(buf: &mut [u8], mask: [u8; 4]) {
    for (i, byte) in buf.iter_mut().enumerate() {
        *byte ^= mask[i & 3];
    }
}

/// Faster version of `apply_mask()` which operates on 4-byte blocks.
#[doc(hidden)]
#[inline(always)]
pub fn apply_mask_fast32(buf: &mut [u8], mask: [u8; 4]) {
    let mask_u32 = u32::from_ne_bytes(mask);
    let (prefix, words, suffix) = unsafe { buf.align_to_mut::<u32>() };
    apply_mask_fallback(prefix, mask);

    let head = prefix.len() & 3;
    let mask_u32 = if head > 0 {
        if cfg!(target_endian = "big") {
            mask_u32.rotate_left(8 * head as u32)
        } else {
            mask_u32.rotate_right(8 * head as u32)
        }
    } else {
        mask_u32
    };

    for word in words.iter_mut() {
        *word ^= mask_u32;
    }

    apply_mask_fallback(suffix, mask_u32.to_ne_bytes());
}

/// Even faster version using 64-bit blocks for larger buffers.
#[doc(hidden)]
#[inline(always)]
pub fn apply_mask_fast64(buf: &mut [u8], mask: [u8; 4]) {
    // Create 64-bit mask by repeating the 32-bit mask
    let mask_u32 = u32::from_ne_bytes(mask);
    let mask_u64 = ((mask_u32 as u64) << 32) | (mask_u32 as u64);

    let (prefix, words, suffix) = unsafe { buf.align_to_mut::<u64>() };
    apply_mask_fallback(prefix, mask);

    let head = prefix.len() & 3;
    let mask_u64 = if head > 0 {
        if cfg!(target_endian = "big") {
            mask_u64.rotate_left(8 * head as u32)
        } else {
            mask_u64.rotate_right(8 * head as u32)
        }
    } else {
        mask_u64
    };

    for word in words.iter_mut() {
        *word ^= mask_u64;
    }

    apply_mask_fallback(suffix, (mask_u64 as u32).to_ne_bytes());
}

/// AVX2-accelerated masking for x86_64 with 256-bit vectors.
///
/// # Safety
/// Requires AVX2 support. Caller must ensure the CPU supports AVX2.
#[cfg(any(target_arch = "x86_64", target_arch = "x86"))]
#[doc(hidden)]
#[target_feature(enable = "avx2")]
#[inline]
unsafe fn apply_mask_avx2(buf: &mut [u8], mask: [u8; 4]) {
    let ptr = buf.as_mut_ptr();
    xor_avx2(ptr, ptr, buf.len(), mask);
}

/// The pointers must cover `len` bytes and be identical or non-overlapping.
#[cfg(any(target_arch = "x86_64", target_arch = "x86"))]
#[target_feature(enable = "avx2")]
unsafe fn xor_avx2(src: *const u8, dst: *mut u8, len: usize, mask: [u8; 4]) {
    #[cfg(target_arch = "x86")]
    use std::arch::x86::*;
    #[cfg(target_arch = "x86_64")]
    use std::arch::x86_64::*;

    let mask_256 = _mm256_set1_epi32(u32::from_ne_bytes(mask) as i32);
    let mut offset = 0;
    while len - offset >= 128 {
        let a = _mm256_loadu_si256(src.add(offset).cast());
        let b = _mm256_loadu_si256(src.add(offset + 32).cast());
        let c = _mm256_loadu_si256(src.add(offset + 64).cast());
        let d = _mm256_loadu_si256(src.add(offset + 96).cast());
        _mm256_storeu_si256(dst.add(offset).cast(), _mm256_xor_si256(a, mask_256));
        _mm256_storeu_si256(dst.add(offset + 32).cast(), _mm256_xor_si256(b, mask_256));
        _mm256_storeu_si256(dst.add(offset + 64).cast(), _mm256_xor_si256(c, mask_256));
        _mm256_storeu_si256(dst.add(offset + 96).cast(), _mm256_xor_si256(d, mask_256));
        offset += 128;
    }
    while len - offset >= 32 {
        let word = _mm256_loadu_si256(src.add(offset).cast());
        _mm256_storeu_si256(dst.add(offset).cast(), _mm256_xor_si256(word, mask_256));
        offset += 32;
    }
    for index in offset..len {
        dst.add(index)
            .write(src.add(index).read() ^ mask[index & 3]);
    }
}

/// NEON-accelerated masking for ARM64 with 128-bit vectors.
///
/// # Safety
/// Requires NEON support. Caller must ensure the CPU supports NEON.
#[cfg(target_arch = "aarch64")]
#[doc(hidden)]
#[target_feature(enable = "neon")]
#[inline]
unsafe fn apply_mask_neon(buf: &mut [u8], mask: [u8; 4]) {
    use std::arch::aarch64::*;

    let len = buf.len();
    if len < 16 {
        return apply_mask_fast32(buf, mask);
    }

    // Create 128-bit mask by repeating the 4-byte mask
    let mask_u32 = u32::from_ne_bytes(mask);
    let mask_128 = vdupq_n_u32(mask_u32);

    let mut ptr = buf.as_mut_ptr();
    let end = ptr.add(len);
    let aligned_end = ptr.add(len - (len % 16));

    // Process 16-byte chunks with NEON
    while ptr < aligned_end {
        let data = vld1q_u8(ptr);
        let masked = veorq_u8(data, vreinterpretq_u8_u32(mask_128));
        vst1q_u8(ptr, masked);
        ptr = ptr.add(16);
    }

    // Handle remaining bytes with scalar code
    let remaining = end.offset_from(ptr) as usize;
    if remaining > 0 {
        apply_mask_fast32(std::slice::from_raw_parts_mut(ptr, remaining), mask);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn masked_copy_matches_scalar_and_preserves_guards() {
        for size in (0..=260).chain([1023, 1024, 16384]) {
            for offset in 0..32 {
                let input: Vec<_> = (0..size + 32).map(|i| i as u8).collect();
                let source = &input[offset..offset + size];
                let mut output = vec![MaybeUninit::new(0xa5); size + 64];
                let start = 31 - offset;
                let mask = [0x12, 0x39, 0xab, 0xfe];
                copy_mask(source, &mut output[start..start + size], mask);
                // SAFETY: The output starts initialized, and copy_mask only writes bytes.
                let output: Vec<_> = output
                    .into_iter()
                    .map(|byte| unsafe { byte.assume_init() })
                    .collect();
                assert!(output[..start].iter().all(|byte| *byte == 0xa5));
                assert!(output[start + size..].iter().all(|byte| *byte == 0xa5));
                for index in 0..size {
                    assert_eq!(output[start + index], source[index] ^ mask[index & 3]);
                }
            }
        }
    }

    #[test]
    fn dispatched_mask_matches_scalar_at_vector_boundaries() {
        for size in [
            0, 1, 15, 16, 31, 32, 127, 128, 129, 255, 256, 257, 1023, 16384,
        ] {
            for offset in 0..32 {
                let mut actual: Vec<_> = (0..size + offset + 32).map(|i| i as u8).collect();
                let mut expected = actual.clone();
                let mask = [0x12, 0x39, 0xab, 0xfe];
                apply_mask_fallback(&mut expected[offset..offset + size], mask);
                apply_mask(&mut actual[offset..offset + size], mask);
                assert_eq!(actual, expected, "size {size}, offset {offset}");
            }
        }
    }

    #[test]
    #[wasm_bindgen_test::wasm_bindgen_test]
    fn test_apply_mask() {
        let mask = [0x6d, 0xb6, 0xb2, 0x80];
        let unmasked = [
            0xf3, 0x00, 0x01, 0x02, 0x03, 0x80, 0x81, 0x82, 0xff, 0xfe, 0x00, 0x17, 0x74, 0xf9,
            0x12, 0x03,
        ];

        for data_len in 0..=unmasked.len() {
            let unmasked = &unmasked[0..data_len];
            // Check masking with different alignment.
            for off in 0..=3 {
                if unmasked.len() < off {
                    continue;
                }
                let mut masked = unmasked.to_vec();
                apply_mask_fallback(&mut masked[off..], mask);

                let mut masked_fast32 = unmasked.to_vec();
                apply_mask_fast32(&mut masked_fast32[off..], mask);

                let mut masked_fast64 = unmasked.to_vec();
                apply_mask_fast64(&mut masked_fast64[off..], mask);

                assert_eq!(masked, masked_fast32);
                assert_eq!(masked, masked_fast64);
            }
        }
    }

    #[test]
    #[wasm_bindgen_test::wasm_bindgen_test]
    fn test_mask_unmask_identity() {
        // Test that applying mask twice returns original data
        let mask = [0xAA, 0xBB, 0xCC, 0xDD];
        let original = b"Hello, World! This is a test message with various lengths.";

        let mut data = original.to_vec();
        apply_mask(&mut data, mask);

        // Data should be masked now
        assert_ne!(&data[..], &original[..]);

        // Apply mask again to unmask
        apply_mask(&mut data, mask);

        // Should be back to original
        assert_eq!(&data[..], &original[..]);
    }

    #[test]
    #[wasm_bindgen_test::wasm_bindgen_test]
    fn test_mask_all_zeros() {
        let mask = [0x00, 0x00, 0x00, 0x00];
        let original = b"Test data";

        let mut data = original.to_vec();
        apply_mask(&mut data, mask);

        // With zero mask, data should be unchanged
        assert_eq!(&data[..], &original[..]);
    }

    #[test]
    #[wasm_bindgen_test::wasm_bindgen_test]
    fn test_mask_all_ones() {
        let mask = [0xFF, 0xFF, 0xFF, 0xFF];
        let original = vec![0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77];
        let expected = vec![0xFF, 0xEE, 0xDD, 0xCC, 0xBB, 0xAA, 0x99, 0x88];

        let mut data = original.clone();
        apply_mask(&mut data, mask);

        assert_eq!(data, expected);
    }

    #[test]
    #[wasm_bindgen_test::wasm_bindgen_test]
    fn test_mask_edge_cases() {
        let mask = [0x12, 0x34, 0x56, 0x78];

        // Test empty buffer
        let mut empty: Vec<u8> = vec![];
        apply_mask(&mut empty, mask);
        assert_eq!(empty.len(), 0);

        // Test single byte
        let mut single = vec![0xAB];
        apply_mask(&mut single, mask);
        assert_eq!(single, vec![0xAB ^ 0x12]);

        // Test two bytes
        let mut two = vec![0xAB, 0xCD];
        apply_mask(&mut two, mask);
        assert_eq!(two, vec![0xAB ^ 0x12, 0xCD ^ 0x34]);

        // Test three bytes
        let mut three = vec![0xAB, 0xCD, 0xEF];
        apply_mask(&mut three, mask);
        assert_eq!(three, vec![0xAB ^ 0x12, 0xCD ^ 0x34, 0xEF ^ 0x56]);
    }

    #[test]
    #[wasm_bindgen_test::wasm_bindgen_test]
    fn test_mask_large_buffer() {
        // Test with a large buffer to exercise the word-aligned path
        let mask = [0x01, 0x02, 0x03, 0x04];
        let size = 10000;
        let mut data: Vec<u8> = (0..size).map(|i| (i % 256) as u8).collect();
        let original = data.clone();

        apply_mask(&mut data, mask);

        // Verify every byte is correctly masked
        for (i, &byte) in data.iter().enumerate() {
            let expected = original[i] ^ mask[i % 4];
            assert_eq!(byte, expected, "Mismatch at index {}", i);
        }
    }

    #[test]
    #[wasm_bindgen_test::wasm_bindgen_test]
    fn test_mask_alignment() {
        // Test that masking works correctly with different alignments
        let mask = [0xAA, 0xBB, 0xCC, 0xDD];

        // Create a buffer with extra padding to test different alignments
        let mut buffer = vec![0u8; 20];
        #[allow(clippy::needless_range_loop)]
        for i in 0..buffer.len() {
            buffer[i] = i as u8;
        }

        // Test masking at different offsets
        for offset in 0..4 {
            let mut test_buf = buffer.clone();
            let original_slice = test_buf[offset..].to_vec();

            apply_mask(&mut test_buf[offset..], mask);

            // Verify masking is correct
            for (i, &byte) in test_buf[offset..].iter().enumerate() {
                let expected = original_slice[i] ^ mask[i % 4];
                assert_eq!(byte, expected, "Alignment {} failed at index {}", offset, i);
            }

            // Verify double masking restores original
            apply_mask(&mut test_buf[offset..], mask);
            assert_eq!(&test_buf[offset..], &original_slice[..]);
        }
    }

    #[test]
    #[wasm_bindgen_test::wasm_bindgen_test]
    fn test_mask_fast_matches_fallback() {
        // Comprehensive test that fast and fallback produce identical results
        let masks = [
            [0x00, 0x00, 0x00, 0x00],
            [0xFF, 0xFF, 0xFF, 0xFF],
            [0x12, 0x34, 0x56, 0x78],
            [0xAA, 0xBB, 0xCC, 0xDD],
            [0x01, 0x23, 0x45, 0x67],
        ];

        for mask in masks {
            for size in 0..=200 {
                let data: Vec<u8> = (0..size).map(|i| (i * 7) as u8).collect();

                let mut fallback_result = data.clone();
                apply_mask_fallback(&mut fallback_result, mask);

                let mut fast32_result = data.clone();
                apply_mask_fast32(&mut fast32_result, mask);

                let mut fast64_result = data.clone();
                apply_mask_fast64(&mut fast64_result, mask);

                assert_eq!(
                    fallback_result, fast32_result,
                    "fast32 mismatch for mask {:?} with size {}",
                    mask, size
                );

                assert_eq!(
                    fallback_result, fast64_result,
                    "fast64 mismatch for mask {:?} with size {}",
                    mask, size
                );
            }
        }
    }

    #[test]
    #[wasm_bindgen_test::wasm_bindgen_test]
    fn test_mask_endianness() {
        // Test that demonstrates endianness handling in fast path
        let mask = [0x11, 0x22, 0x33, 0x44];
        let data = vec![0xFF; 16]; // Nice aligned size

        let mut fallback = data.clone();
        apply_mask_fallback(&mut fallback, mask);

        let mut fast = data.clone();
        apply_mask_fast32(&mut fast, mask);

        // Both should produce identical results regardless of endianness
        assert_eq!(fallback, fast);

        // Verify the pattern repeats correctly
        for i in 0..fallback.len() {
            assert_eq!(fallback[i], 0xFF ^ mask[i % 4]);
        }
    }
}
