use crate::{
  beam::disp_result::DispatchResult,
  emulator::{heap::THeapOwner, process::Process, runtime_ctx::*},
  fail::RtResult,
  term::{boxed::binary::match_state::BinaryMatchState, Term},
};

// Test that the remaining bits in the match context are divisible by Unit.
// Jump to Fail if not.
// Structure: bs_test_unit(Fail, Ctx, Unit)
define_opcode!(
  _vm, rt_ctx, proc, name: OpcodeBsTestUnit, arity: 3,
  run: { Self::bs_test_unit(rt_ctx, fail, match_state, unit) },
  args: cp_or_nil(fail), binary_match_state(match_state), usize(unit),
);

impl OpcodeBsTestUnit {
  #[inline]
  fn bs_test_unit(
    runtime_ctx: &mut RuntimeContext,
    fail: Term,
    match_state: *mut BinaryMatchState,
    unit: usize,
  ) -> RtResult<DispatchResult> {
    let remaining = unsafe { (*match_state).get_bits_remaining().bits };
    if unit == 0 || remaining % unit != 0 {
      runtime_ctx.jump(fail);
    }
    Ok(DispatchResult::Normal)
  }
}
