use crate::{
  beam::disp_result::DispatchResult,
  defs::{data_reader::TDataReader, BitSize},
  emulator::{heap::THeapOwner, process::Process, runtime_ctx::*},
  fail::RtResult,
  term::{boxed::binary::match_state::BinaryMatchState, Term},
};

// Match a literal byte sequence against the current position in a binary
// match context. Bits is the number of bits to match (must be divisible by 8).
// The Literal argument is a pointer to the byte data in the code stream.
// Structure: bs_match_string(Fail, Ctx, Bits, Literal)
define_opcode!(
  _vm, rt_ctx, proc, name: OpcodeBsMatchString, arity: 4,
  run: { unsafe { Self::bs_match_string(rt_ctx, fail, match_state, bits, literal) } },
  args: cp_or_nil(fail), binary_match_state(match_state), usize(bits), term(literal),
);

impl OpcodeBsMatchString {
  #[inline]
  unsafe fn bs_match_string(
    runtime_ctx: &mut RuntimeContext,
    fail: Term,
    match_state: *mut BinaryMatchState,
    bits: usize,
    literal: Term,
  ) -> RtResult<DispatchResult> {
    let bit_size = BitSize::with_bits(bits);
    let remaining = (*match_state).get_bits_remaining();

    // Check if enough bits remain
    if remaining.bits < bits {
      runtime_ctx.jump(fail);
      return Ok(DispatchResult::Normal);
    }

    // We need to compare the literal bytes against the source binary data.
    // The literal Term is a binary containing the bytes to match.
    // In the BEAM file format, bs_match_string stores literal data as a
    // binary term in the code stream.
    let n_bytes = bits / 8;

    if literal.is_binary() {
      let lit_ptr = crate::term::boxed::Binary::get_trait_from_term(literal);
      let src_bin = (*match_state).get_src_binary();
      let offset = (*match_state).get_offset();
      let reader = (*src_bin).get_bit_reader().add_bit_offset(offset);

      // Compare byte by byte
      let lit_reader = (*lit_ptr).get_bit_reader();
      for i in 0..n_bytes {
        let src_byte = reader.read(i);
        let lit_byte = lit_reader.read(i);
        if src_byte != lit_byte {
          runtime_ctx.jump(fail);
          return Ok(DispatchResult::Normal);
        }
      }

      // Match succeeded, advance the offset
      (*match_state).increase_offset(bit_size);
    } else {
      // Literal is not a binary — fail
      runtime_ctx.jump(fail);
    }

    Ok(DispatchResult::Normal)
  }
}
