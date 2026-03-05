//! Module implements binary/bit syntax matching and data creation & extraction
//! opcodes for binaries.
mod bs_get_binary;
mod bs_init;
mod bs_create_bin;
mod bs_match;
mod bs_put_binary;
mod bs_put_integer;
mod bs_start_match;

pub use self::{
  bs_create_bin::*, bs_get_binary::*, bs_init::*, bs_match::*, bs_put_binary::*,
  bs_put_integer::*, bs_start_match::*,
};

use crate::{
  beam::disp_result::DispatchResult,
  defs::BitSize,
  emulator::{heap::THeapOwner, process::Process, runtime_ctx::*},
  fail::RtResult,
  term::{boxed::binary::match_state::BinaryMatchState, *},
};

#[cfg(target_pointer_width = "64")]
type ArchUsize = u64;
#[cfg(target_pointer_width = "32")]
type ArchUsize = u32;

// Values used in bs_* opcodes for flags
// pub const BSF_ALIGNED: usize = 1;
// pub const BSF_LITTLE: usize = 2; )
// pub const BSF_SIGNED: usize = 4;
// pub const BSF_EXACT: usize = 8;
// pub const BSF_NATIVE: usize = 16;
bitflags! {
    pub struct BsFlags: ArchUsize {
        const ALIGNED = 0b00000001; // Field is guaranteed to be byte-aligned
        const LITTLE  = 0b00000010; // Field is little-endian (otherwise big)
        const SIGNED  = 0b00000100; // Field is signed (otherwise unsigned)
        const EXACT   = 0b00001000; // Size in bs_init is exact
        const NATIVE  = 0b00010000; // Native endian
    }
}

// Having started binary matching, check that the match state has so many `Bits`
// remaining otherwise will jump to the `Fail` label.
// Structure: bs_test_tail2(Fail, MatchState, Bits)
define_opcode!(
  _vm, rt_ctx, proc, name: OpcodeBsTestTail2, arity: 3,
  run: { Self::bs_test_tail2(rt_ctx, proc, fail, match_state, bits) },
  args: cp_or_nil(fail), binary_match_state(match_state), load_usize(bits),
);


impl OpcodeBsTestTail2 {
  #[inline]
  fn bs_test_tail2(
    runtime_ctx: &mut RuntimeContext,
    _proc: &mut Process,
    fail: Term,
    match_state: *mut BinaryMatchState,
    bits: usize,
  ) -> RtResult<DispatchResult> {
    let remaining = unsafe { (*match_state).get_bits_remaining().bits };
    if remaining != bits {
      runtime_ctx.jump(fail);
    }
    Ok(DispatchResult::Normal)
  }
}

// Skip Size*Unit bits in an existing binary match context.
// Structure: bs_skip_bits2(Fail, MatchState, Size, Unit, Flags)
define_opcode!(
  _vm, rt_ctx, _proc, name: OpcodeBsSkipBits2, arity: 5,
  run: { Self::bs_skip_bits2(rt_ctx, fail, match_state, size, unit) },
  args: cp_or_nil(fail), binary_match_state(match_state), load_usize(size), usize(unit), IGNORE(flags),
);

impl OpcodeBsSkipBits2 {
  #[inline]
  fn bs_skip_bits2(
    runtime_ctx: &mut RuntimeContext,
    fail: Term,
    match_state: *mut BinaryMatchState,
    size: usize,
    unit: usize,
  ) -> RtResult<DispatchResult> {
    let skip = BitSize::with_unit(size, unit);
    let remaining = unsafe { (*match_state).get_bits_remaining() };
    if skip.bits > remaining.bits {
      runtime_ctx.jump(fail);
      return Ok(DispatchResult::Normal);
    }
    unsafe { (*match_state).increase_offset(skip) };
    Ok(DispatchResult::Normal)
  }
}

// This instruction is rewritten on Erlang/OTP to `move S2, Dst`
// Structure: bs_add(Fail, S1_ignored, S2, Unit, Dst)
define_opcode!(
  _vm, rt_ctx, proc, name: OpcodeBsAdd, arity: 5,
  run: {
    rt_ctx.store_value(s2, dst, proc.get_heap_mut())?;
    Ok(DispatchResult::Normal)
  },
  args: cp_or_nil(fail), IGNORE(s1), load(s2), IGNORE(unit), term(dst),
);

// Extract the remaining binary from a match context.
// Structure: bs_get_tail(Ctx, Dst, Live)
define_opcode!(
  _vm, rt_ctx, proc, name: OpcodeBsGetTail, arity: 3,
  run: { Self::bs_get_tail(rt_ctx, proc, match_state, dst, live) },
  args: binary_match_state(match_state), term(dst), usize(live),
);

impl OpcodeBsGetTail {
  #[inline]
  fn bs_get_tail(
    rt_ctx: &mut RuntimeContext,
    proc: &mut Process,
    match_state: *mut BinaryMatchState,
    dst: Term,
    live: usize,
  ) -> RtResult<DispatchResult> {
    use crate::term::boxed::binary::BinarySlice;
    let remaining = unsafe { (*match_state).get_bits_remaining() };
    if remaining.bits == 0 {
      rt_ctx.store_value(Term::empty_binary(), dst, proc.get_heap_mut())?;
      return Ok(DispatchResult::Normal);
    }

    rt_ctx.live = live;
    proc.ensure_heap(BinarySlice::storage_size())?;

    let src_bin = unsafe { (*match_state).get_src_binary() };
    let bit_offset = unsafe { (*match_state).get_offset() };
    let slice = unsafe {
      BinarySlice::create_into(src_bin, bit_offset, remaining, proc.get_heap_mut())?
    };
    rt_ctx.store_value(unsafe { (*slice).make_term() }, dst, proc.get_heap_mut())?;
    Ok(DispatchResult::Normal)
  }
}

// Save the current match position as a small integer.
// Structure: bs_get_position(Ctx, Dst, Live)
define_opcode!(
  _vm, rt_ctx, proc, name: OpcodeBsGetPosition, arity: 3,
  run: {
    let offset = unsafe { (*match_state).get_offset().bits };
    let pos = Term::make_small_unsigned(offset);
    rt_ctx.store_value(pos, dst, proc.get_heap_mut())?;
    Ok(DispatchResult::Normal)
  },
  args: binary_match_state(match_state), term(dst), IGNORE(live),
);

// Restore a previously saved match position.
// Structure: bs_set_position(Ctx, Pos)
define_opcode!(
  _vm, rt_ctx, proc, name: OpcodeBsSetPosition, arity: 2,
  run: {
    let pos = rt_ctx.load(pos, proc.get_heap());
    let bits = if pos.is_small() {
      pos.get_small_unsigned()
    } else {
      0
    };
    unsafe { (*match_state).set_offset(BitSize::with_bits(bits)) };
    Ok(DispatchResult::Normal)
  },
  args: binary_match_state(match_state), term(pos),
);
