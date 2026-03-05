use crate::{
  beam::disp_result::DispatchResult,
  defs::{data_reader::TDataReader, BitSize},
  emulator::{atom, heap::THeapOwner, process::Process, runtime_ctx::*},
  fail::{self, RtResult},
  term::{
    boxed::binary::{match_state::BinaryMatchState, BinarySlice},
    Term,
  },
};

// OTP 24+ compact binary matching instruction.
// Structure: bs_match(Fail, MatchState, Commands)
define_opcode!(
  _vm, rt_ctx, proc, name: OpcodeBsMatch, arity: 3,
  run: { unsafe { Self::bs_match(rt_ctx, proc, fail, match_state, commands) } },
  args: cp_or_nil(fail), binary_match_state(match_state), term(commands),
);

impl OpcodeBsMatch {
  #[inline]
  fn fail_and_jump(rt_ctx: &mut RuntimeContext, fail: Term) {
    if fail.is_cp() {
      rt_ctx.jump(fail);
    }
  }

  #[inline]
  fn to_usize(v: Term) -> Option<usize> {
    if v.is_small() {
      Some(v.get_small_unsigned())
    } else {
      None
    }
  }

  #[inline]
  fn to_i64(v: Term) -> Option<i64> {
    if v.is_small() {
      Some(v.get_small_signed() as i64)
    } else {
      None
    }
  }

  #[inline]
  fn expected_u64(value: Term, bits: usize) -> Option<u64> {
    if bits > 64 {
      return None;
    }
    let signed = Self::to_i64(value)?;
    if signed >= 0 {
      let n = signed as u64;
      if bits < 64 && n >= (1u64 << bits) {
        return None;
      }
      return Some(n);
    }

    if bits == 64 {
      return Some(signed as u64);
    }
    let base = 1i128 << bits;
    let n = base + signed as i128;
    if n < 0 {
      return None;
    }
    Some(n as u64)
  }

  #[inline]
  unsafe fn match_bits_eq(
    match_state: *mut BinaryMatchState,
    bits: usize,
    value: Term,
  ) -> bool {
    let expected = match Self::expected_u64(value, bits) {
      Some(v) => v,
      None => return false,
    };
    let src_bin = (*match_state).get_src_binary();
    let offset = (*match_state).get_offset();
    let reader = (*src_bin).get_bit_reader().add_bit_offset(offset);

    for i in 0..bits {
      let b = reader.read(i / 8);
      let bit = (b >> (7 - (i % 8))) & 1;
      let want = ((expected >> (bits - 1 - i)) & 1) as u8;
      if bit != want {
        return false;
      }
    }
    true
  }

  #[inline]
  unsafe fn ensure_remaining(
    match_state: *mut BinaryMatchState,
    needed: BitSize,
  ) -> bool {
    (*match_state).get_bits_remaining().bits >= needed.bits
  }

  unsafe fn extract_tail(
    rt_ctx: &mut RuntimeContext,
    proc: &mut Process,
    match_state: *mut BinaryMatchState,
    live: usize,
    dst: Term,
  ) -> RtResult<bool> {
    let remaining = (*match_state).get_bits_remaining();
    if remaining.bits == 0 {
      rt_ctx.store_value(Term::empty_binary(), dst, proc.get_heap_mut())?;
      return Ok(true);
    }

    rt_ctx.live = live;
    proc.ensure_heap(BinarySlice::storage_size())?;

    let src_bin = (*match_state).get_src_binary();
    let bit_offset = (*match_state).get_offset();
    let slice =
      BinarySlice::create_into(src_bin, bit_offset, remaining, proc.get_heap_mut())?;
    rt_ctx.store_value((*slice).make_term(), dst, proc.get_heap_mut())?;
    Ok(true)
  }

  unsafe fn run_one(
    rt_ctx: &mut RuntimeContext,
    proc: &mut Process,
    match_state: *mut BinaryMatchState,
    command: Term,
  ) -> RtResult<bool> {
    if !command.is_tuple() || command == Term::empty_tuple() {
      return fail::create::badarg();
    }
    let t = &*command.get_tuple_ptr();
    let arity = t.get_arity();
    if arity == 0 {
      return fail::create::badarg();
    }

    let op = t.get_element(0);
    if !op.is_atom() {
      return fail::create::badarg();
    }

    if op == atom::from_str("ensure_at_least") && arity == 3 {
      let size = match Self::to_usize(t.get_element(1)) {
        Some(v) => v,
        None => return Ok(false),
      };
      let unit = match Self::to_usize(t.get_element(2)) {
        Some(v) => v,
        None => return Ok(false),
      };
      return Ok(Self::ensure_remaining(
        match_state,
        BitSize::with_unit(size, unit),
      ));
    }

    if op == atom::from_str("ensure_exactly") && arity == 2 {
      let size = match Self::to_usize(t.get_element(1)) {
        Some(v) => v,
        None => return Ok(false),
      };
      return Ok((*match_state).get_bits_remaining().bits == size);
    }

    if op == atom::from_str("=:=") && arity == 4 {
      if t.get_element(1) != Term::nil() {
        return Ok(false);
      }
      let bits = match Self::to_usize(t.get_element(2)) {
        Some(v) => v,
        None => return Ok(false),
      };
      if !Self::ensure_remaining(match_state, BitSize::with_bits(bits)) {
        return Ok(false);
      }
      if !Self::match_bits_eq(match_state, bits, t.get_element(3)) {
        return Ok(false);
      }
      (*match_state).increase_offset(BitSize::with_bits(bits));
      return Ok(true);
    }

    if op == atom::from_str("skip") && arity == 2 {
      let bits = match Self::to_usize(t.get_element(1)) {
        Some(v) => v,
        None => return Ok(false),
      };
      if !Self::ensure_remaining(match_state, BitSize::with_bits(bits)) {
        return Ok(false);
      }
      (*match_state).increase_offset(BitSize::with_bits(bits));
      return Ok(true);
    }

    if op == atom::from_str("get_tail") && arity == 4 {
      let live = match Self::to_usize(t.get_element(1)) {
        Some(v) => v,
        None => return Ok(false),
      };
      let dst = t.get_element(3);
      return Self::extract_tail(rt_ctx, proc, match_state, live, dst);
    }

    // Unsupported command for now behaves as nomatch.
    Ok(false)
  }

  unsafe fn run_commands(
    rt_ctx: &mut RuntimeContext,
    proc: &mut Process,
    match_state: *mut BinaryMatchState,
    commands: Term,
  ) -> RtResult<bool> {
    if commands == Term::nil() || commands == Term::empty_tuple() {
      return Ok(true);
    }

    if commands.is_tuple() {
      let tuple_ptr = commands.get_tuple_ptr();
      let tuple = &*tuple_ptr;
      let arity = tuple.get_arity();
      if arity > 0 && tuple.get_element(0).is_atom() {
        let mut flat = Vec::with_capacity(arity);
        for i in 0..arity {
          flat.push(tuple.get_element(i));
        }
        return Self::run_flat_terms(rt_ctx, proc, match_state, &flat);
      }
      for i in 0..arity {
        if !Self::run_one(rt_ctx, proc, match_state, tuple.get_element(i))? {
          return Ok(false);
        }
      }
      return Ok(true);
    }

    if commands.is_cons() {
      let mut p = commands.get_cons_ptr();
      loop {
        let cmd = (*p).hd();
        if !Self::run_one(rt_ctx, proc, match_state, cmd)? {
          return Ok(false);
        }
        let tl = (*p).tl();
        if tl.is_cons() {
          p = tl.get_cons_ptr();
          continue;
        }
        return Ok(tl == Term::nil());
      }
    }

    fail::create::badarg()
  }

  unsafe fn run_flat_terms(
    rt_ctx: &mut RuntimeContext,
    proc: &mut Process,
    match_state: *mut BinaryMatchState,
    terms: &[Term],
  ) -> RtResult<bool> {
    let mut i = 0usize;
    while i < terms.len() {
      let op = terms[i];
      if !op.is_atom() {
        return Ok(false);
      }

      if op == atom::from_str("ensure_at_least") {
        if i + 2 >= terms.len() {
          return Ok(false);
        }
        let size = match Self::to_usize(terms[i + 1]) {
          Some(v) => v,
          None => return Ok(false),
        };
        let unit = match Self::to_usize(terms[i + 2]) {
          Some(v) => v,
          None => return Ok(false),
        };
        if !Self::ensure_remaining(match_state, BitSize::with_unit(size, unit)) {
          return Ok(false);
        }
        i += 3;
        continue;
      }

      if op == atom::from_str("ensure_exactly") {
        if i + 1 >= terms.len() {
          return Ok(false);
        }
        let size = match Self::to_usize(terms[i + 1]) {
          Some(v) => v,
          None => return Ok(false),
        };
        if (*match_state).get_bits_remaining().bits != size {
          return Ok(false);
        }
        i += 2;
        continue;
      }

      if op == atom::from_str("=:=") {
        if i + 3 >= terms.len() {
          return Ok(false);
        }
        if terms[i + 1] != Term::nil() {
          return Ok(false);
        }
        let bits = match Self::to_usize(terms[i + 2]) {
          Some(v) => v,
          None => return Ok(false),
        };
        if !Self::ensure_remaining(match_state, BitSize::with_bits(bits)) {
          return Ok(false);
        }
        if !Self::match_bits_eq(match_state, bits, terms[i + 3]) {
          return Ok(false);
        }
        (*match_state).increase_offset(BitSize::with_bits(bits));
        i += 4;
        continue;
      }

      if op == atom::from_str("skip") {
        if i + 1 >= terms.len() {
          return Ok(false);
        }
        let bits = match Self::to_usize(terms[i + 1]) {
          Some(v) => v,
          None => return Ok(false),
        };
        if !Self::ensure_remaining(match_state, BitSize::with_bits(bits)) {
          return Ok(false);
        }
        (*match_state).increase_offset(BitSize::with_bits(bits));
        i += 2;
        continue;
      }

      if op == atom::from_str("get_tail") {
        if i + 3 >= terms.len() {
          return Ok(false);
        }
        let live = match Self::to_usize(terms[i + 1]) {
          Some(v) => v,
          None => return Ok(false),
        };
        if !Self::extract_tail(rt_ctx, proc, match_state, live, terms[i + 3])? {
          return Ok(false);
        }
        i += 4;
        continue;
      }

      // Unsupported command in flattened representation.
      return Ok(false);
    }

    Ok(true)
  }

  #[inline]
  unsafe fn bs_match(
    rt_ctx: &mut RuntimeContext,
    proc: &mut Process,
    fail: Term,
    match_state: *mut BinaryMatchState,
    commands: Term,
  ) -> RtResult<DispatchResult> {
    let saved_offset = (*match_state).get_offset();
    let ok = Self::run_commands(rt_ctx, proc, match_state, commands)?;
    if !ok {
      (*match_state).set_offset(saved_offset);
      Self::fail_and_jump(rt_ctx, fail);
    }
    Ok(DispatchResult::Normal)
  }

  // Expose helpers for testing
  #[cfg(test)]
  pub fn test_expected_u64(value: Term, bits: usize) -> Option<u64> {
    Self::expected_u64(value, bits)
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn expected_u64_positive_small() {
    let val = Term::make_small_signed(42);
    assert_eq!(OpcodeBsMatch::test_expected_u64(val, 8), Some(42));
    assert_eq!(OpcodeBsMatch::test_expected_u64(val, 6), Some(42));
    // 42 doesn't fit in 5 bits (max 31)
    assert_eq!(OpcodeBsMatch::test_expected_u64(val, 5), None);
  }

  #[test]
  fn expected_u64_zero() {
    let val = Term::make_small_signed(0);
    assert_eq!(OpcodeBsMatch::test_expected_u64(val, 1), Some(0));
    assert_eq!(OpcodeBsMatch::test_expected_u64(val, 8), Some(0));
    assert_eq!(OpcodeBsMatch::test_expected_u64(val, 64), Some(0));
  }

  #[test]
  fn expected_u64_max_unsigned_8bit() {
    let val = Term::make_small_signed(255);
    assert_eq!(OpcodeBsMatch::test_expected_u64(val, 8), Some(255));
    assert_eq!(OpcodeBsMatch::test_expected_u64(val, 7), None); // 255 > 127
  }

  #[test]
  fn expected_u64_negative_8bit() {
    // -1 in 8-bit two's complement = 0xFF = 255
    let val = Term::make_small_signed(-1);
    assert_eq!(OpcodeBsMatch::test_expected_u64(val, 8), Some(255));
  }

  #[test]
  fn expected_u64_negative_16bit() {
    // -1 in 16-bit = 0xFFFF = 65535
    let val = Term::make_small_signed(-1);
    assert_eq!(OpcodeBsMatch::test_expected_u64(val, 16), Some(65535));
  }

  #[test]
  fn expected_u64_too_many_bits() {
    let val = Term::make_small_signed(1);
    // bits > 64 should return None
    assert_eq!(OpcodeBsMatch::test_expected_u64(val, 65), None);
    assert_eq!(OpcodeBsMatch::test_expected_u64(val, 128), None);
  }

  #[test]
  fn expected_u64_full_64bit() {
    let val = Term::make_small_signed(-1);
    // -1 in 64-bit = u64::MAX
    assert_eq!(
      OpcodeBsMatch::test_expected_u64(val, 64),
      Some(u64::MAX)
    );
  }

  // --- Edge cases ---

  #[test]
  fn expected_u64_one_bit() {
    // 0 fits in 1 bit, 1 fits in 1 bit, 2 does not
    assert_eq!(OpcodeBsMatch::test_expected_u64(Term::make_small_signed(0), 1), Some(0));
    assert_eq!(OpcodeBsMatch::test_expected_u64(Term::make_small_signed(1), 1), Some(1));
    assert_eq!(OpcodeBsMatch::test_expected_u64(Term::make_small_signed(2), 1), None);
  }

  #[test]
  fn expected_u64_zero_bits() {
    // 0 bits: only 0 fits (bits < 64, n >= 1<<0 = 1 for anything > 0)
    assert_eq!(OpcodeBsMatch::test_expected_u64(Term::make_small_signed(0), 0), Some(0));
    assert_eq!(OpcodeBsMatch::test_expected_u64(Term::make_small_signed(1), 0), None);
  }

  #[test]
  fn expected_u64_exact_boundary_values() {
    // 127 fits in 7 bits (max = 127), 128 does not
    assert_eq!(OpcodeBsMatch::test_expected_u64(Term::make_small_signed(127), 7), Some(127));
    assert_eq!(OpcodeBsMatch::test_expected_u64(Term::make_small_signed(128), 7), None);
    // 128 fits in 8 bits
    assert_eq!(OpcodeBsMatch::test_expected_u64(Term::make_small_signed(128), 8), Some(128));
  }

  #[test]
  fn expected_u64_negative_too_small() {
    // -129 in 8 bits: base = 256, 256 + (-129) = 127... wait that's positive
    // Actually: -128 in 8 bits = 128 (0x80). -129 would be 256-129=127, but
    // conceptually -129 doesn't fit in signed 8-bit range [-128, 127].
    // The function wraps: base=256, 256 + (-129) = 127, which is >= 0 → Some(127)
    // This is how Erlang bs_match =:= works: it wraps around.
    let val = Term::make_small_signed(-129);
    let result = OpcodeBsMatch::test_expected_u64(val, 8);
    assert_eq!(result, Some(127));
  }

  #[test]
  fn expected_u64_negative_exact_min_8bit() {
    // -128 in 8 bits = 0x80 = 128
    let val = Term::make_small_signed(-128);
    assert_eq!(OpcodeBsMatch::test_expected_u64(val, 8), Some(128));
  }

  #[test]
  fn expected_u64_large_positive_32bit() {
    // 0x7FFFFFFF (2^31 - 1) in 32 bits
    let val = Term::make_small_signed(0x7FFFFFFF);
    assert_eq!(OpcodeBsMatch::test_expected_u64(val, 32), Some(0x7FFFFFFF));
    assert_eq!(OpcodeBsMatch::test_expected_u64(val, 31), Some(0x7FFFFFFF));
    assert_eq!(OpcodeBsMatch::test_expected_u64(val, 30), None);
  }

  #[test]
  fn expected_u64_non_small_returns_none() {
    // A nil term is not a small integer
    let val = Term::nil();
    assert_eq!(OpcodeBsMatch::test_expected_u64(val, 8), None);
  }
}
