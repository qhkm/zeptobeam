use crate::defs::{self, BitSize, SizeBytes};

/// A generic byte reader interface, to be replaced by either byte aligned reader
/// or bit reader, depending whether the data is byte-aligned. In the future
/// there might as well appear another word-aligned reader?
pub trait TDataReader {
  fn get_bit_size(&self) -> BitSize;
  fn read(&self, n: usize) -> u8;
}

/// Points to bit data with possible misaligned bit offset from the beginning.
/// Bit reader is naturally slower than byte-aligned reader.
pub struct BitReader {
  data: &'static [u8],
  offset: BitSize,
}

impl BitReader {
  #[allow(dead_code)]
  pub fn new(data: &'static [u8], offset: BitSize) -> Self {
    Self { data, offset }
  }

  pub fn add_bit_offset(&self, offs: BitSize) -> Self {
    Self {
      data: self.data,
      offset: self.offset + offs,
    }
  }
}

impl TDataReader for BitReader {
  fn get_bit_size(&self) -> BitSize {
    // Length of the data minus the bit offset.
    // The last byte will be possibly incomplete.
    BitSize::with_bits(self.data.len() * defs::BYTE_BITS - self.offset.bits)
  }

  #[inline]
  fn read(&self, n: usize) -> u8 {
    let bit_pos = self.offset.bits + n * defs::BYTE_BITS;
    let byte_ix = bit_pos >> defs::BYTE_POF2_BITS;
    if byte_ix >= self.data.len() {
      return 0;
    }

    let shift = bit_pos & (defs::BYTE_BITS - 1);
    let b0 = self.data[byte_ix];
    if shift == 0 {
      return b0;
    }

    let b1 = if byte_ix + 1 < self.data.len() {
      self.data[byte_ix + 1]
    } else {
      0
    };
    (b0 << shift) | (b1 >> (defs::BYTE_BITS - shift))
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::defs::BitSize;

  #[test]
  fn bit_reader_aligned_reads_bytes_directly() {
    let data: &'static [u8] = &[0xDE, 0xAD, 0xBE, 0xEF];
    let reader = BitReader::new(data, BitSize::zero());
    assert_eq!(reader.read(0), 0xDE);
    assert_eq!(reader.read(1), 0xAD);
    assert_eq!(reader.read(2), 0xBE);
    assert_eq!(reader.read(3), 0xEF);
  }

  #[test]
  fn bit_reader_offset_4bits_shifts_correctly() {
    // Data: [0xAB, 0xCD] → bits: 1010_1011 1100_1101
    // With 4-bit offset, reading byte 0 should give: 1011_1100 = 0xBC
    let data: &'static [u8] = &[0xAB, 0xCD];
    let reader = BitReader::new(data, BitSize::with_bits(4));
    let b = reader.read(0);
    assert_eq!(b, 0xBC, "Expected 0xBC, got 0x{:02X}", b);
  }

  #[test]
  fn bit_reader_offset_1bit() {
    // Data: [0x80] → bits: 1000_0000
    // With 1-bit offset, reading byte 0 should give: 0000_0000 = 0x00
    let data: &'static [u8] = &[0x80, 0x00];
    let reader = BitReader::new(data, BitSize::with_bits(1));
    assert_eq!(reader.read(0), 0x00);
  }

  #[test]
  fn bit_reader_add_offset() {
    let data: &'static [u8] = &[0xFF, 0x00];
    let reader = BitReader::new(data, BitSize::zero());
    let shifted = reader.add_bit_offset(BitSize::with_bits(8));
    assert_eq!(shifted.read(0), 0x00);
  }

  #[test]
  fn bit_reader_get_bit_size() {
    let data: &'static [u8] = &[0, 0, 0]; // 3 bytes = 24 bits
    let reader = BitReader::new(data, BitSize::with_bits(5));
    // 24 - 5 = 19 bits remaining
    assert_eq!(reader.get_bit_size().bits, 19);
  }

  #[test]
  fn bit_reader_out_of_bounds_returns_zero() {
    let data: &'static [u8] = &[0xFF];
    let reader = BitReader::new(data, BitSize::zero());
    // Reading beyond the data should return 0
    assert_eq!(reader.read(1), 0);
    assert_eq!(reader.read(100), 0);
  }

  #[test]
  fn byte_reader_basic() {
    let data: Vec<u8> = vec![1, 2, 3, 4, 5];
    let ptr = data.as_ptr();
    let reader = ByteReader::new(ptr, data.len());
    assert_eq!(reader.read(0), 1);
    assert_eq!(reader.read(4), 5);
    assert_eq!(reader.get_bit_size().bits, 40);
    std::mem::forget(data); // prevent dealloc while reader holds static ref
  }

  // --- Edge cases ---

  #[test]
  fn bit_reader_offset_7bits() {
    // Data: [0xFF, 0x00] → bits: 1111_1111 0000_0000
    // Offset 7 bits: skip 1111_111 → remaining starts at last bit of 0xFF
    // Reading byte 0: take bit7 of 0xFF (=1) then bits 0-6 of 0x00 (=0000000)
    // Result: 1000_0000 = 0x80
    let data: &'static [u8] = &[0xFF, 0x00];
    let reader = BitReader::new(data, BitSize::with_bits(7));
    assert_eq!(reader.read(0), 0x80);
  }

  #[test]
  fn bit_reader_single_byte_data() {
    let data: &'static [u8] = &[0xAA]; // 1010_1010
    let reader = BitReader::new(data, BitSize::zero());
    assert_eq!(reader.read(0), 0xAA);
    // Out of bounds
    assert_eq!(reader.read(1), 0);
  }

  #[test]
  fn bit_reader_empty_data() {
    let data: &'static [u8] = &[];
    let reader = BitReader::new(data, BitSize::zero());
    assert_eq!(reader.get_bit_size().bits, 0);
    assert_eq!(reader.read(0), 0);
  }

  #[test]
  fn bit_reader_chained_offsets() {
    // Data: [0x12, 0x34, 0x56]
    // Apply offsets step by step
    let data: &'static [u8] = &[0x12, 0x34, 0x56];
    let r0 = BitReader::new(data, BitSize::zero());
    assert_eq!(r0.read(0), 0x12);

    let r8 = r0.add_bit_offset(BitSize::with_bits(8));
    assert_eq!(r8.read(0), 0x34);

    let r16 = r8.add_bit_offset(BitSize::with_bits(8));
    assert_eq!(r16.read(0), 0x56);

    // Double-jump from zero
    let r16b = r0.add_bit_offset(BitSize::with_bits(16));
    assert_eq!(r16b.read(0), 0x56);
  }

  #[test]
  fn bit_reader_cross_byte_boundary_all_ones() {
    // Data: [0xFF, 0xFF] → all 1s
    // Any offset should still read 0xFF for the first virtual byte
    let data: &'static [u8] = &[0xFF, 0xFF];
    for offset in 0..8 {
      let reader = BitReader::new(data, BitSize::with_bits(offset));
      assert_eq!(reader.read(0), 0xFF, "offset={}", offset);
    }
  }

  #[test]
  fn bit_reader_alternating_bits_various_offsets() {
    // Data: [0xAA, 0xAA] → 1010_1010 1010_1010
    // Offset 1: 010_1010_1 = 0x55
    // Offset 2: 10_1010_10 = 0xAA
    let data: &'static [u8] = &[0xAA, 0xAA];
    let r1 = BitReader::new(data, BitSize::with_bits(1));
    assert_eq!(r1.read(0), 0x55);
    let r2 = BitReader::new(data, BitSize::with_bits(2));
    assert_eq!(r2.read(0), 0xAA);
  }

  #[test]
  fn bit_reader_last_byte_partial_shift() {
    // Data: [0x80] → 1000_0000
    // Offset 0, reading byte 0 → 0x80
    // Offset 1 → 0000_000 + no next byte → 0x00
    let data: &'static [u8] = &[0x80];
    let r0 = BitReader::new(data, BitSize::zero());
    assert_eq!(r0.read(0), 0x80);
    let r1 = BitReader::new(data, BitSize::with_bits(1));
    assert_eq!(r1.read(0), 0x00);
  }

  #[test]
  fn byte_reader_set_offset_and_size() {
    let data: Vec<u8> = vec![10, 20, 30, 40, 50];
    let ptr = data.as_ptr();
    let reader = ByteReader::new(ptr, data.len());
    // Skip 2 bytes, take 2 bytes
    let sub = unsafe {
      reader.set_offset_and_size(SizeBytes::new(2), SizeBytes::new(2))
    };
    assert_eq!(sub.read(0), 30);
    assert_eq!(sub.read(1), 40);
    assert_eq!(sub.get_bit_size().bits, 16);
    std::mem::forget(data);
  }

  #[test]
  fn byte_reader_zero_length() {
    let data: Vec<u8> = vec![];
    let ptr = data.as_ptr();
    let reader = ByteReader::new(ptr, 0);
    assert_eq!(reader.get_bit_size().bits, 0);
    std::mem::forget(data);
  }
}

/// Points to read-only byte-aligned data.
/// Byte reader is naturally faster than the bit reader.
pub struct ByteReader {
  data: &'static [u8],
}

impl ByteReader {
  pub fn new(data: *const u8, size: usize) -> Self {
    // println!("Creating data reader size={}", size);
    Self {
      data: unsafe { core::slice::from_raw_parts(data, size) },
    }
  }

  /// From a byte reader create one shorter byte reader, offset forward by `size`
  pub unsafe fn set_offset_and_size(self, offs: SizeBytes, size: SizeBytes) -> Self {
    // println!("Setting data reader offset={}", offs);
    let new_ptr = self.data.as_ptr().add(offs.bytes());
    let new_len = core::cmp::min(self.data.len() - offs.bytes(), size.bytes());
    Self {
      data: core::slice::from_raw_parts(new_ptr, new_len),
    }
  }
}

impl TDataReader for ByteReader {
  #[inline]
  fn get_bit_size(&self) -> BitSize {
    BitSize::with_bytes(self.data.len())
  }

  #[inline]
  fn read(&self, n: usize) -> u8 {
    self.data[n]
  }
}
