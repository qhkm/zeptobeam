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

// Append to an existing binary. Creates a new binary builder that contains the
// data from Bin plus space for Sz*Unit additional bits.
// Structure: bs_append(Fail, Sz, Extra, Live, Unit, Bin, Flags, Dst)
define_opcode!(
  vm, rt_ctx, proc, name: OpcodeBsAppend, arity: 8,
  run: {
    Self::bs_append(vm, rt_ctx, proc, fail, sz, extra, live, unit, bin, flags, dst)
  },
  args: cp_or_nil(fail), load_usize(sz), usize(extra), usize(live),
        usize(unit), load(bin), usize(flags), term(dst),
);

impl OpcodeBsAppend {
  #[inline]
  #[allow(clippy::too_many_arguments)]
  fn bs_append(
    vm: &mut VM,
    runtime_ctx: &mut RuntimeContext,
    proc: &mut Process,
    fail: Term,
    sz: usize,
    _extra: usize,
    _live: usize,
    unit: usize,
    bin: Term,
    _flags: usize,
    dst: Term,
  ) -> RtResult<DispatchResult> {
    let additional_bits = BitSize::with_unit(sz, unit);

    // Get the existing binary size
    let existing_bits = if bin == Term::empty_binary() {
      BitSize::zero()
    } else if bin.is_binary() {
      let bin_ptr =
        unsafe { boxed::Binary::get_trait_from_term(bin) };
      unsafe { (*bin_ptr).get_bit_size() }
    } else {
      if fail != Term::nil() {
        runtime_ctx.jump(fail);
        return Ok(DispatchResult::Normal);
      }
      return fail::create::badarg();
    };

    let total_bits = existing_bits + additional_bits;

    if total_bits.is_empty() {
      runtime_ctx.store_value(
        Term::empty_binary(),
        dst,
        proc.get_heap_mut(),
      )?;
      return Ok(DispatchResult::Normal);
    }

    let byte_sz = total_bits.get_byte_size_rounded_up().bytes();
    if fail != Term::nil()
      && boxed::Binary::is_size_too_big(SizeBytes::new(byte_sz))
    {
      return fail::create::system_limit();
    }

    // Create a new binary large enough for existing + additional data
    let new_bin = if byte_sz <= ProcessHeapBinary::ONHEAP_THRESHOLD {
      proc.ensure_heap(ProcessHeapBinary::storage_size(total_bits))?;
      unsafe {
        boxed::Binary::create_into(total_bits, proc.get_heap_mut())?
      }
    } else {
      vm.binary_heap
        .ensure_heap(ReferenceToBinary::storage_size())?;
      unsafe {
        boxed::Binary::create_into(
          total_bits,
          vm.binary_heap.get_heap_mut(),
        )?
      }
    };

    // Copy existing binary data into the new binary
    if !existing_bits.is_empty() && bin != Term::empty_binary() {
      let src_ptr =
        unsafe { boxed::Binary::get_trait_from_term(bin) };
      let src_data = unsafe { (*src_ptr).get_data() };
      let dst_data = unsafe { (*new_bin).get_data_mut() };
      let copy_bytes = existing_bits.get_byte_size_rounded_up().bytes();
      let copy_len = core::cmp::min(copy_bytes, src_data.len());
      let copy_len = core::cmp::min(copy_len, dst_data.len());
      unsafe {
        core::ptr::copy_nonoverlapping(
          src_data.as_ptr(),
          dst_data.as_mut_ptr(),
          copy_len,
        );
      }
    }

    let bin_term = unsafe { (*new_bin).make_term() };

    // Set the current binary state for subsequent bs_put_* operations.
    // The offset starts at the end of the existing data.
    runtime_ctx.current_bin.reset(bin_term);
    runtime_ctx.current_bin.offset = existing_bits;

    runtime_ctx.store_value(bin_term, dst, proc.get_heap_mut())?;
    Ok(DispatchResult::Normal)
  }
}
