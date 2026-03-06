use crate::{
  beam::disp_result::DispatchResult,
  defs::{data_reader::TDataReader, BitSize},
  emulator::{heap::THeapOwner, process::Process, runtime_ctx::*},
  fail::{self, RtResult},
  term::{boxed::binary::match_state::BinaryMatchState, Term},
};

// ---------------------------------------------------------------------------
// bs_get_utf8: Extract one UTF-8 codepoint from a match context.
// Structure: bs_get_utf8(Fail, Ctx, Live, Flags, Dst)
// ---------------------------------------------------------------------------
define_opcode!(
  _vm, rt_ctx, proc, name: OpcodeBsGetUtf8, arity: 5,
  run: { unsafe { Self::bs_get_utf8(rt_ctx, proc, fail, match_state, live, dst) } },
  args: cp_or_nil(fail), binary_match_state(match_state), usize(live),
        IGNORE(flags), term(dst),
);

impl OpcodeBsGetUtf8 {
  #[inline]
  unsafe fn bs_get_utf8(
    runtime_ctx: &mut RuntimeContext,
    proc: &mut Process,
    fail: Term,
    match_state: *mut BinaryMatchState,
    _live: usize,
    dst: Term,
  ) -> RtResult<DispatchResult> {
    let remaining = (*match_state).get_bits_remaining();
    if remaining.bits < 8 {
      runtime_ctx.jump(fail);
      return Ok(DispatchResult::Normal);
    }

    let src_bin = (*match_state).get_src_binary();
    let offset = (*match_state).get_offset();
    let reader = (*src_bin).get_bit_reader().add_bit_offset(offset);

    let first_byte = reader.read(0);

    // Determine UTF-8 sequence length from the first byte
    let (char_len, codepoint) = match utf8_decode_first_byte(first_byte) {
      Some(v) => v,
      None => {
        runtime_ctx.jump(fail);
        return Ok(DispatchResult::Normal);
      }
    };

    // Check we have enough remaining bits
    let needed_bits = char_len * 8;
    if remaining.bits < needed_bits {
      runtime_ctx.jump(fail);
      return Ok(DispatchResult::Normal);
    }

    // Decode continuation bytes
    let codepoint = match utf8_decode_continuation(&reader, codepoint, char_len) {
      Some(cp) => cp,
      None => {
        runtime_ctx.jump(fail);
        return Ok(DispatchResult::Normal);
      }
    };

    // Validate the codepoint
    if char::from_u32(codepoint).is_none() {
      runtime_ctx.jump(fail);
      return Ok(DispatchResult::Normal);
    }

    (*match_state).increase_offset(BitSize::with_bits(needed_bits));
    let result = Term::make_small_unsigned(codepoint as usize);
    runtime_ctx.store_value(result, dst, proc.get_heap_mut())?;
    Ok(DispatchResult::Normal)
  }
}

// ---------------------------------------------------------------------------
// bs_skip_utf8: Skip one UTF-8 character in a match context.
// Structure: bs_skip_utf8(Fail, Ctx, Live, Flags)
// ---------------------------------------------------------------------------
define_opcode!(
  _vm, rt_ctx, proc, name: OpcodeBsSkipUtf8, arity: 4,
  run: { unsafe { Self::bs_skip_utf8(rt_ctx, fail, match_state) } },
  args: cp_or_nil(fail), binary_match_state(match_state), IGNORE(live),
        IGNORE(flags),
);

impl OpcodeBsSkipUtf8 {
  #[inline]
  unsafe fn bs_skip_utf8(
    runtime_ctx: &mut RuntimeContext,
    fail: Term,
    match_state: *mut BinaryMatchState,
  ) -> RtResult<DispatchResult> {
    let remaining = (*match_state).get_bits_remaining();
    if remaining.bits < 8 {
      runtime_ctx.jump(fail);
      return Ok(DispatchResult::Normal);
    }

    let src_bin = (*match_state).get_src_binary();
    let offset = (*match_state).get_offset();
    let reader = (*src_bin).get_bit_reader().add_bit_offset(offset);
    let first_byte = reader.read(0);

    let (char_len, codepoint) = match utf8_decode_first_byte(first_byte) {
      Some(v) => v,
      None => {
        runtime_ctx.jump(fail);
        return Ok(DispatchResult::Normal);
      }
    };

    let needed_bits = char_len * 8;
    if remaining.bits < needed_bits {
      runtime_ctx.jump(fail);
      return Ok(DispatchResult::Normal);
    }

    // Validate continuation bytes
    if utf8_decode_continuation(&reader, codepoint, char_len).is_none() {
      runtime_ctx.jump(fail);
      return Ok(DispatchResult::Normal);
    }

    (*match_state).increase_offset(BitSize::with_bits(needed_bits));
    Ok(DispatchResult::Normal)
  }
}

// ---------------------------------------------------------------------------
// bs_utf8_size: Compute the number of bytes needed to encode a codepoint
// as UTF-8.
// Structure: bs_utf8_size(Fail, Src, Dst)
// ---------------------------------------------------------------------------
define_opcode!(
  _vm, rt_ctx, proc, name: OpcodeBsUtf8Size, arity: 3,
  run: { Self::bs_utf8_size(rt_ctx, proc, fail, src, dst) },
  args: cp_or_nil(fail), load(src), term(dst),
);

impl OpcodeBsUtf8Size {
  #[inline]
  fn bs_utf8_size(
    runtime_ctx: &mut RuntimeContext,
    proc: &mut Process,
    fail: Term,
    src: Term,
    dst: Term,
  ) -> RtResult<DispatchResult> {
    if !src.is_small() {
      if fail != Term::nil() {
        runtime_ctx.jump(fail);
        return Ok(DispatchResult::Normal);
      }
      return fail::create::badarg();
    }

    let codepoint = src.get_small_unsigned() as u32;
    let utf8_len = match char::from_u32(codepoint) {
      Some(c) => c.len_utf8(),
      None => {
        if fail != Term::nil() {
          runtime_ctx.jump(fail);
          return Ok(DispatchResult::Normal);
        }
        return fail::create::badarg();
      }
    };

    let result = Term::make_small_unsigned(utf8_len);
    runtime_ctx.store_value(result, dst, proc.get_heap_mut())?;
    Ok(DispatchResult::Normal)
  }
}

// ---------------------------------------------------------------------------
// bs_put_utf8: Encode a codepoint as UTF-8 into the current binary builder.
// Structure: bs_put_utf8(Fail, Flags, Src)
// ---------------------------------------------------------------------------
define_opcode!(
  _vm, rt_ctx, proc, name: OpcodeBsPutUtf8, arity: 3,
  run: { unsafe { Self::bs_put_utf8(rt_ctx, proc, fail, src) } },
  args: cp_or_nil(fail), IGNORE(flags), load(src),
);

impl OpcodeBsPutUtf8 {
  #[inline]
  unsafe fn bs_put_utf8(
    ctx: &mut RuntimeContext,
    _proc: &mut Process,
    fail: Term,
    src: Term,
  ) -> RtResult<DispatchResult> {
    debug_assert!(
      ctx.current_bin.valid(),
      "bs_put_utf8 with no ctx.current_bin"
    );

    if !src.is_small() {
      if fail != Term::nil() {
        ctx.jump(fail);
        return Ok(DispatchResult::Normal);
      }
      return fail::create::badarg();
    }

    let codepoint = src.get_small_unsigned() as u32;
    let c = match char::from_u32(codepoint) {
      Some(c) => c,
      None => {
        if fail != Term::nil() {
          ctx.jump(fail);
          return Ok(DispatchResult::Normal);
        }
        return fail::create::badarg();
      }
    };

    let mut buf = [0u8; 4];
    let encoded = c.encode_utf8(&mut buf);
    let utf8_bytes = encoded.as_bytes();

    let dst_bin = ctx.current_bin.dst.unwrap();
    let dst_data = (*dst_bin).get_data_mut();
    let offset_bytes = ctx.current_bin.offset.get_bytes_rounded_down();

    for (i, &b) in utf8_bytes.iter().enumerate() {
      if offset_bytes + i < dst_data.len() {
        dst_data[offset_bytes + i] = b;
      }
    }

    ctx.current_bin.offset =
      ctx.current_bin.offset + BitSize::with_bytes(utf8_bytes.len());
    Ok(DispatchResult::Normal)
  }
}

// ---------------------------------------------------------------------------
// Helper functions for UTF-8 decoding
// ---------------------------------------------------------------------------

/// Decode the first byte of a UTF-8 sequence.
/// Returns (sequence_length_in_bytes, partial_codepoint_from_first_byte).
fn utf8_decode_first_byte(byte: u8) -> Option<(usize, u32)> {
  if byte < 0x80 {
    Some((1, byte as u32))
  } else if byte & 0xE0 == 0xC0 {
    Some((2, (byte & 0x1F) as u32))
  } else if byte & 0xF0 == 0xE0 {
    Some((3, (byte & 0x0F) as u32))
  } else if byte & 0xF8 == 0xF0 {
    Some((4, (byte & 0x07) as u32))
  } else {
    None // Invalid start byte
  }
}

/// Decode continuation bytes from a bit reader.
/// `partial` is the partial codepoint from the first byte.
/// `char_len` is the total sequence length.
fn utf8_decode_continuation(
  reader: &crate::defs::data_reader::BitReader,
  mut partial: u32,
  char_len: usize,
) -> Option<u32> {
  for i in 1..char_len {
    let cont_byte = reader.read(i);
    if cont_byte & 0xC0 != 0x80 {
      return None; // Invalid continuation byte
    }
    partial = (partial << 6) | (cont_byte & 0x3F) as u32;
  }

  // Check for overlong sequences
  match char_len {
    1 => {} // always valid
    2 => {
      if partial < 0x80 {
        return None;
      }
    }
    3 => {
      if partial < 0x800 {
        return None;
      }
    }
    4 => {
      if partial < 0x10000 {
        return None;
      }
    }
    _ => return None,
  }

  // Check for surrogate pairs (0xD800..0xDFFF)
  if (0xD800..=0xDFFF).contains(&partial) {
    return None;
  }

  Some(partial)
}
