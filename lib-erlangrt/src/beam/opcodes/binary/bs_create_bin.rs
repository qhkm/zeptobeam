use crate::{
  beam::{disp_result::DispatchResult, opcodes::binary::BsFlags},
  defs::{BitSize, SizeWords},
  emulator::{atom, heap::THeapOwner, process::Process, runtime_ctx::*, vm::VM},
  fail::{self, RtResult},
  term::{
    boxed::binary::{bits_paste, bits_paste::SizeOrAll, ProcessHeapBinary, ReferenceToBinary},
    cons, Term,
  },
};

enum CreateOp {
  Integer {
    unit: usize,
    flags: BsFlags,
    src: Term,
    size: usize,
  },
  Binary {
    unit: usize,
    flags: BsFlags,
    src: Term,
    size: SizeOrAll,
  },
}

// OTP 25+ binary builder.
// Structure: bs_create_bin(Fail, Alloc, Live, Unit, Dst, OpList)
define_opcode!(
  vm, rt_ctx, proc, name: OpcodeBsCreateBin, arity: 6,
  run: {
    Self::bs_create_bin(vm, rt_ctx, proc, fail, alloc, live, unit, dst, op_list)
  },
  args: cp_or_nil(fail), usize(alloc), usize(live), usize(unit), term(dst), term(op_list),
);

impl OpcodeBsCreateBin {
  #[inline]
  fn parse_flags(raw: Term) -> RtResult<BsFlags> {
    if raw == Term::nil() {
      return Ok(BsFlags::empty());
    }
    if raw.is_small() {
      return Ok(BsFlags::from_bits_truncate(raw.get_small_unsigned() as u64));
    }
    if !raw.is_cons() {
      return fail::create::badarg();
    }

    let mut flags = BsFlags::empty();
    cons::for_each(raw, |t| {
      if t == atom::from_str("little") {
        flags |= BsFlags::LITTLE;
      } else if t == atom::from_str("signed") {
        flags |= BsFlags::SIGNED;
      } else if t == atom::from_str("exact") {
        flags |= BsFlags::EXACT;
      } else if t == atom::from_str("native") {
        flags |= BsFlags::NATIVE;
      } else if t == atom::from_str("aligned") {
        flags |= BsFlags::ALIGNED;
      }
      Ok(())
    })?;
    Ok(flags)
  }

  #[inline]
  fn flatten_op_list(op_list: Term) -> RtResult<Vec<Term>> {
    if op_list == Term::nil() || op_list == Term::empty_tuple() {
      return Ok(Vec::new());
    }

    if op_list.is_tuple() {
      let t = unsafe { &*op_list.get_tuple_ptr() };
      let mut out = Vec::with_capacity(t.get_arity());
      for i in 0..t.get_arity() {
        out.push(unsafe { t.get_element(i) });
      }
      return Ok(out);
    }

    if op_list.is_cons() {
      let mut out = Vec::new();
      cons::for_each(op_list, |t| {
        out.push(t);
        Ok(())
      })?;
      return Ok(out);
    }

    fail::create::badarg()
  }

  fn parse_ops(op_list: Term) -> RtResult<Vec<CreateOp>> {
    let terms = Self::flatten_op_list(op_list)?;
    let mut i = 0usize;
    let mut out = Vec::new();

    while i < terms.len() {
      if !terms[i].is_atom() {
        return fail::create::badarg();
      }

      let tag = terms[i];
      if tag == atom::from_str("integer") {
        if i + 5 >= terms.len() {
          return fail::create::badarg();
        }
        let unit = terms[i + 2].get_small_unsigned();
        let flags = Self::parse_flags(terms[i + 3])?;
        let src = terms[i + 4];
        let size = terms[i + 5].get_small_unsigned();
        out.push(CreateOp::Integer {
          unit,
          flags,
          src,
          size,
        });
        i += 6;
        continue;
      }

      if tag == atom::from_str("binary") {
        if i + 5 >= terms.len() {
          return fail::create::badarg();
        }
        let unit = terms[i + 2].get_small_unsigned();
        let flags = Self::parse_flags(terms[i + 3])?;
        let src = terms[i + 4];
        let size = if terms[i + 5] == atom::from_str("all") {
          SizeOrAll::All
        } else {
          SizeOrAll::Bits(BitSize::with_unit(terms[i + 5].get_small_unsigned(), unit))
        };
        out.push(CreateOp::Binary {
          unit,
          flags,
          src,
          size,
        });
        i += 6;
        continue;
      }

      return fail::create::badarg();
    }

    Ok(out)
  }

  #[inline]
  fn op_bits(
    rt_ctx: &RuntimeContext,
    proc: &mut Process,
    op: &CreateOp,
  ) -> RtResult<BitSize> {
    match op {
      CreateOp::Integer { unit, size, .. } => Ok(BitSize::with_unit(*size, *unit)),
      CreateOp::Binary { src, size, .. } => match size {
        SizeOrAll::Bits(bits) => Ok(*bits),
        SizeOrAll::All => {
          let src = rt_ctx.load(*src, proc.get_heap_mut());
          if !src.is_binary() {
            return fail::create::badarg();
          }
          let src_ptr = unsafe { crate::term::boxed::Binary::get_trait_from_term(src) };
          Ok(unsafe { (*src_ptr).get_bit_size() })
        }
      },
    }
  }

  #[inline]
  fn maybe_jump_fail(rt_ctx: &mut RuntimeContext, fail: Term) -> DispatchResult {
    if fail.is_cp() {
      rt_ctx.jump(fail);
    }
    DispatchResult::Normal
  }

  #[allow(clippy::too_many_arguments)]
  fn bs_create_bin(
    vm: &mut VM,
    rt_ctx: &mut RuntimeContext,
    proc: &mut Process,
    fail: Term,
    alloc: usize,
    live: usize,
    _unit: usize,
    dst: Term,
    op_list: Term,
  ) -> RtResult<DispatchResult> {
    let ops = Self::parse_ops(op_list)?;

    let mut total_bits = BitSize::zero();
    for op in &ops {
      total_bits = total_bits + Self::op_bits(rt_ctx, proc, op)?;
    }

    if total_bits.is_empty() {
      rt_ctx.store_value(Term::empty_binary(), dst, proc.get_heap_mut())?;
      return Ok(DispatchResult::Normal);
    }

    let extra = SizeWords::new(alloc);
    let bytes = total_bits.get_byte_size_rounded_up().bytes();
    let dst_bin = if bytes <= ProcessHeapBinary::ONHEAP_THRESHOLD {
      rt_ctx.live = live;
      proc.ensure_heap(ProcessHeapBinary::storage_size(total_bits) + extra)?;
      unsafe { crate::term::boxed::Binary::create_into(total_bits, proc.get_heap_mut())? }
    } else {
      rt_ctx.live = live;
      vm.binary_heap
        .ensure_heap(ReferenceToBinary::storage_size() + extra)?;
      unsafe { crate::term::boxed::Binary::create_into(total_bits, vm.binary_heap.get_heap_mut())? }
    };

    let mut offset = BitSize::zero();
    for op in ops {
      let step = match op {
        CreateOp::Integer {
          unit,
          flags,
          src,
          size,
        } => {
          let src_val = rt_ctx.load(src, proc.get_heap_mut());
          let bits = BitSize::with_unit(size, unit);
          unsafe { (*dst_bin).put_integer(src_val, bits, offset, flags)? };
          bits
        }

        CreateOp::Binary {
          unit: _unit,
          flags,
          src,
          size,
        } => {
          let src_val = rt_ctx.load(src, proc.get_heap_mut());
          if !src_val.is_binary() {
            return Ok(Self::maybe_jump_fail(rt_ctx, fail));
          }
          let src_ptr = unsafe { crate::term::boxed::Binary::get_trait_from_term(src_val) };
          unsafe { bits_paste::put_binary(src_ptr, size, dst_bin, offset, flags)? }
        }
      };
      offset = offset + step;
    }

    let dst_val = unsafe { (*dst_bin).make_term() };
    rt_ctx.store_value(dst_val, dst, proc.get_heap_mut())?;
    Ok(DispatchResult::Normal)
  }
}
