use crate::{
  beam::disp_result::DispatchResult,
  defs::{BitSize, SizeBytes},
  emulator::{heap::THeapOwner, process::Process, runtime_ctx::*, vm::VM},
  fail::{self, RtResult},
  term::{
    boxed::{self, binary::*},
    Term,
  },
};

// Create a binary builder for Sz*Unit bits. Store in Dst.
// Similar to bs_init2 but operates at bit level instead of byte level.
// Structure: bs_init_bits(Fail, Sz, Live, Unit, Flags, Dst)
define_opcode!(
  vm, rt_ctx, proc, name: OpcodeBsInitBits, arity: 6,
  run: { Self::bs_init_bits(vm, rt_ctx, proc, fail, sz, live, unit, flags, dst) },
  args: cp_or_nil(fail), load_usize(sz), usize(live), usize(unit),
        usize(flags), term(dst),
);

impl OpcodeBsInitBits {
  #[inline]
  #[allow(clippy::too_many_arguments)]
  fn bs_init_bits(
    vm: &mut VM,
    runtime_ctx: &mut RuntimeContext,
    proc: &mut Process,
    fail: Term,
    sz: usize,
    _live: usize,
    unit: usize,
    _flags: usize,
    dst: Term,
  ) -> RtResult<DispatchResult> {
    let bit_sz = BitSize::with_unit(sz, unit);

    if bit_sz.is_empty() {
      runtime_ctx.store_value(Term::empty_binary(), dst, proc.get_heap_mut())?;
      return Ok(DispatchResult::Normal);
    }

    let byte_sz = bit_sz.get_byte_size_rounded_up().bytes();
    if fail != Term::nil()
      && boxed::Binary::is_size_too_big(SizeBytes::new(byte_sz))
    {
      return fail::create::system_limit();
    }

    let bin = if byte_sz <= ProcessHeapBinary::ONHEAP_THRESHOLD {
      proc.ensure_heap(ProcessHeapBinary::storage_size(bit_sz))?;
      unsafe { boxed::Binary::create_into(bit_sz, proc.get_heap_mut())? }
    } else {
      vm.binary_heap
        .ensure_heap(ReferenceToBinary::storage_size())?;
      unsafe {
        boxed::Binary::create_into(bit_sz, vm.binary_heap.get_heap_mut())?
      }
    };

    let bin_term = unsafe { (*bin).make_term() };
    runtime_ctx.current_bin.reset(bin_term);
    runtime_ctx.store_value(bin_term, dst, proc.get_heap_mut())?;
    Ok(DispatchResult::Normal)
  }
}
