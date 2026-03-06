use crate::{
  beam::disp_result::DispatchResult,
  defs::BitSize,
  emulator::{process::Process, runtime_ctx::*},
  fail::RtResult,
  term::Term,
};

// Copy Len bytes from the literal String into the current binary builder.
// The string is stored as a binary literal in the code stream.
// Structure: bs_put_string(Len, String)
define_opcode!(
  _vm, rt_ctx, _proc, name: OpcodeBsPutString, arity: 2,
  run: { unsafe { Self::bs_put_string(rt_ctx, len, string) } },
  args: usize(len), term(string),
);

impl OpcodeBsPutString {
  #[inline]
  unsafe fn bs_put_string(
    ctx: &mut RuntimeContext,
    len: usize,
    string: Term,
  ) -> RtResult<DispatchResult> {
    debug_assert!(
      ctx.current_bin.valid(),
      "bs_put_string with no ctx.current_bin"
    );

    let dst_bin = ctx.current_bin.dst.unwrap();
    let dst_data = (*dst_bin).get_data_mut();
    let offset_bytes = ctx.current_bin.offset.get_bytes_rounded_down();

    // The string literal is stored as a binary term containing raw bytes.
    if string.is_binary() {
      let src_ptr = crate::term::boxed::Binary::get_trait_from_term(string);
      let src_data = (*src_ptr).get_data();
      let copy_len = core::cmp::min(len, src_data.len());

      core::ptr::copy_nonoverlapping(
        src_data.as_ptr(),
        dst_data.as_mut_ptr().add(offset_bytes),
        copy_len,
      );
    } else {
      // Fallback: the string might be stored as a raw pointer in the code.
      // For now, zero-fill if we can't decode it.
      // TODO: Handle raw code pointer strings
      for i in 0..len {
        if offset_bytes + i < dst_data.len() {
          dst_data[offset_bytes + i] = 0;
        }
      }
    }

    ctx.current_bin.offset =
      ctx.current_bin.offset + BitSize::with_bytes(len);
    Ok(DispatchResult::Normal)
  }
}
