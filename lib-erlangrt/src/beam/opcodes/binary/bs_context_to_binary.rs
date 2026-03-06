use crate::{
  beam::disp_result::DispatchResult,
  emulator::{heap::THeapOwner, process::Process, runtime_ctx::*},
  fail::RtResult,
  term::{
    boxed::{self, binary::match_state::BinaryMatchState, binary::BinarySlice},
    Term,
  },
};

// If the register contains a match context, extract the remaining binary from
// it and store the result back into the same register. If the register already
// contains a binary, leave it unchanged.
// Structure: bs_context_to_binary(Reg)
define_opcode!(
  _vm, rt_ctx, proc, name: OpcodeBsContextToBinary, arity: 1,
  run: { Self::bs_context_to_binary(rt_ctx, proc, reg) },
  args: term(reg),
);

impl OpcodeBsContextToBinary {
  #[inline]
  fn bs_context_to_binary(
    runtime_ctx: &mut RuntimeContext,
    proc: &mut Process,
    reg: Term,
  ) -> RtResult<DispatchResult> {
    let val = runtime_ctx.load(reg, proc.get_heap());

    // If it's not boxed, or it's not a match state, leave it alone
    if !val.is_boxed() {
      return Ok(DispatchResult::Normal);
    }

    let header = val.get_box_ptr_mut::<boxed::BoxHeader>();
    let trait_ptr = unsafe { (*header).get_trait_ptr() };
    let box_type = unsafe { (*trait_ptr).get_type() };

    if box_type != boxed::BOXTYPETAG_BINARY_MATCH_STATE {
      // Already a binary or something else — leave unchanged
      return Ok(DispatchResult::Normal);
    }

    // It's a match context. Extract the remaining binary.
    let ms = header as *mut BinaryMatchState;
    let remaining = unsafe { (*ms).get_bits_remaining() };

    let result = if remaining.bits == 0 {
      Term::empty_binary()
    } else {
      proc.ensure_heap(BinarySlice::storage_size())?;
      let src_bin = unsafe { (*ms).get_src_binary() };
      let bit_offset = unsafe { (*ms).get_offset() };
      let slice = unsafe {
        BinarySlice::create_into(src_bin, bit_offset, remaining, proc.get_heap_mut())?
      };
      unsafe { (*slice).make_term() }
    };

    runtime_ctx.store_value(result, reg, proc.get_heap_mut())?;
    Ok(DispatchResult::Normal)
  }
}
