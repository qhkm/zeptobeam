use core::fmt;

use crate::defs::{self, SizeBytes, SizeWords};
use std::ops::{Add, Sub};

#[derive(Copy, Clone, Ord, PartialOrd, Eq, PartialEq)]
pub struct BitSize {
  pub bits: usize,
}

impl fmt::Display for BitSize {
  fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
    write!(f, "{} bits", self.bits)
  }
}

impl fmt::Debug for BitSize {
  fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
    write!(f, "{} bits", self.bits)
  }
}

impl BitSize {
  pub const fn zero() -> Self {
    Self { bits: 0 }
  }

  #[inline]
  pub const fn is_empty(self) -> bool {
    self.bits == 0
  }

  #[inline]
  pub const fn with_bits(bit_count: usize) -> Self {
    Self { bits: bit_count }
  }

  #[allow(dead_code)]
  #[inline]
  pub const fn with_bytesize(size: SizeBytes) -> Self {
    Self {
      bits: size.bytes() * defs::BYTE_BITS,
    }
  }

  #[inline]
  pub const fn with_bytes(size: usize) -> Self {
    Self {
      bits: size * defs::BYTE_BITS,
    }
  }

  pub fn with_unit(size: usize, unit: usize) -> Self {
    if cfg!(debug_assertions) {
      assert!(
        size < core::usize::MAX / unit,
        "Bitsize: the size={} * unit={} does not fit usize datatype",
        size,
        unit
      );
    }
    Self { bits: size * unit }
  }

  pub const fn with_unit_const(size: usize, unit: usize) -> Self {
    Self { bits: size * unit }
  }

  /// Returns how many hanging bits are there. Value range is 0..7, 0 means
  /// all bytes are whole and no hanging bits.
  #[inline]
  pub const fn get_last_byte_bits(&self) -> usize {
    self.bits & (defs::BYTE_BITS - 1)
  }

  #[inline]
  pub const fn get_byte_size_rounded_down(&self) -> SizeBytes {
    SizeBytes::new(self.bits >> defs::BYTE_POF2_BITS)
  }

  #[inline]
  pub const fn get_bytes_rounded_down(&self) -> usize {
    self.bits >> defs::BYTE_POF2_BITS
  }

  pub const fn get_byte_size_rounded_up(self) -> SizeBytes {
    SizeBytes::new((self.bits + defs::BYTE_BITS - 1) / defs::BYTE_BITS)
  }

  #[inline]
  pub const fn get_words_rounded_up(self) -> SizeWords {
    let b = self.get_byte_size_rounded_down().bytes();
    SizeWords::new((b + defs::WORD_BYTES - 1) / defs::WORD_BYTES)
  }
}

impl Add for BitSize {
  type Output = BitSize;

  fn add(self, other: BitSize) -> BitSize {
    BitSize {
      bits: self.bits + other.bits,
    }
  }
}

impl Sub for BitSize {
  type Output = BitSize;

  fn sub(self, other: BitSize) -> BitSize {
    BitSize {
      bits: self.bits - other.bits,
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn with_bits_and_accessors() {
    let bs = BitSize::with_bits(17);
    assert_eq!(bs.bits, 17);
    assert_eq!(bs.get_bytes_rounded_down(), 2);
    assert_eq!(bs.get_byte_size_rounded_up().bytes(), 3);
    assert_eq!(bs.get_last_byte_bits(), 1);
  }

  #[test]
  fn with_bytes() {
    let bs = BitSize::with_bytes(3);
    assert_eq!(bs.bits, 24);
    assert_eq!(bs.get_bytes_rounded_down(), 3);
    assert_eq!(bs.get_last_byte_bits(), 0);
  }

  #[test]
  fn with_unit() {
    // size=3, unit=8 → 24 bits
    let bs = BitSize::with_unit(3, 8);
    assert_eq!(bs.bits, 24);

    // size=5, unit=1 → 5 bits
    let bs2 = BitSize::with_unit(5, 1);
    assert_eq!(bs2.bits, 5);
  }

  #[test]
  fn zero_is_empty() {
    assert!(BitSize::zero().is_empty());
    assert!(!BitSize::with_bits(1).is_empty());
  }

  #[test]
  fn add_and_sub() {
    let a = BitSize::with_bits(10);
    let b = BitSize::with_bits(7);
    assert_eq!((a + b).bits, 17);
    assert_eq!((a - b).bits, 3);
  }

  #[test]
  fn ordering() {
    let small = BitSize::with_bits(5);
    let big = BitSize::with_bits(100);
    assert!(small < big);
    assert!(big > small);
    assert_eq!(small, BitSize::with_bits(5));
  }

  // --- Edge cases ---

  #[test]
  fn single_bit_rounding() {
    let bs = BitSize::with_bits(1);
    assert_eq!(bs.get_bytes_rounded_down(), 0);
    assert_eq!(bs.get_byte_size_rounded_up().bytes(), 1);
    assert_eq!(bs.get_last_byte_bits(), 1);
  }

  #[test]
  fn exact_byte_boundary_no_hanging() {
    for n in &[8, 16, 24, 64, 128] {
      let bs = BitSize::with_bits(*n);
      assert_eq!(bs.get_last_byte_bits(), 0, "bits={}", n);
      assert_eq!(bs.get_bytes_rounded_down(), n / 8);
      assert_eq!(bs.get_byte_size_rounded_up().bytes(), n / 8);
    }
  }

  #[test]
  fn one_below_byte_boundary() {
    // 7 bits: 0 full bytes, 7 hanging bits
    let bs = BitSize::with_bits(7);
    assert_eq!(bs.get_bytes_rounded_down(), 0);
    assert_eq!(bs.get_byte_size_rounded_up().bytes(), 1);
    assert_eq!(bs.get_last_byte_bits(), 7);
  }

  #[test]
  fn one_above_byte_boundary() {
    // 9 bits: 1 full byte, 1 hanging bit
    let bs = BitSize::with_bits(9);
    assert_eq!(bs.get_bytes_rounded_down(), 1);
    assert_eq!(bs.get_byte_size_rounded_up().bytes(), 2);
    assert_eq!(bs.get_last_byte_bits(), 1);
  }

  #[test]
  fn with_unit_1() {
    // unit=1 means bits directly
    let bs = BitSize::with_unit(13, 1);
    assert_eq!(bs.bits, 13);
  }

  #[test]
  fn with_unit_large() {
    // unit=128 (used in some OTP binary patterns)
    let bs = BitSize::with_unit(3, 128);
    assert_eq!(bs.bits, 384);
  }

  #[test]
  fn with_bytesize() {
    let sb = SizeBytes::new(10);
    let bs = BitSize::with_bytesize(sb);
    assert_eq!(bs.bits, 80);
  }

  #[test]
  fn words_rounded_up_edge() {
    // On 64-bit: 1 word = 8 bytes = 64 bits
    // 63 bits → 7 bytes rounded down → 1 word
    let bs63 = BitSize::with_bits(63);
    assert_eq!(bs63.get_words_rounded_up().words, 1);

    // 64 bits → 8 bytes → 1 word
    let bs64 = BitSize::with_bits(64);
    assert_eq!(bs64.get_words_rounded_up().words, 1);

    // 65 bits → 8 bytes rounded down → 1 word
    let bs65 = BitSize::with_bits(65);
    assert_eq!(bs65.get_words_rounded_up().words, 1);
  }

  #[test]
  fn sub_to_zero() {
    let a = BitSize::with_bits(42);
    let b = BitSize::with_bits(42);
    let result = a - b;
    assert_eq!(result.bits, 0);
    assert!(result.is_empty());
  }
}
