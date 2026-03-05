//! Module implements opcodes related to floating-point arithmetic.
//! FP registers are raw f64 values stored in RuntimeContext::fpregs[].
//! All arithmetic opcodes (fadd, fsub, fmul, fdiv, fnegate) operate
//! directly on FP registers. fmove and fconv handle transfers between
//! FP registers and X/Y registers (boxed floats on the heap).

use crate::{
  beam::disp_result::DispatchResult,
  emulator::{heap::THeapOwner, process::Process, runtime_ctx::*},
  fail::RtResult,
  term::Term,
};

fn module() -> &'static str {
  "opcodes::op_float: "
}

/// Extract FP register index from a raw Term operand.
#[inline]
fn fp_index(t: Term) -> usize {
  debug_assert!(
    t.is_register_float(),
    "{}expected FP register, got {}",
    module(),
    t
  );
  t.get_reg_value()
}

/// Extract the f64 value from a loaded Term (boxed float or small integer).
#[inline]
fn term_to_f64(val: Term) -> f64 {
  if val.is_float() {
    unsafe { val.get_float_unchecked() }
  } else if val.is_small() {
    val.get_small_signed() as f64
  } else {
    panic!("{}cannot convert {} to float", module(), val);
  }
}

// ============================================================
// fclearerror/0 -- clear FP error flag (no-op for now)
// ============================================================
define_opcode!(_vm, _ctx, _curr_p,
  name: OpcodeFclearerror, arity: 0,
  run: {
    // No-op: Rust floating-point does not use a global FP error flag.
    Ok(DispatchResult::Normal)
  },
  args:
);

// ============================================================
// fcheckerror/1 -- check for FP error (no-op for now)
// Structure: fcheckerror(fail_label)
// ============================================================
define_opcode!(_vm, _ctx, _curr_p,
  name: OpcodeFcheckerror, arity: 1,
  run: {
    // No-op: Rust floating-point operations produce NaN/Inf rather than
    // setting error flags. In the future we could check for NaN here.
    Ok(DispatchResult::Normal)
  },
  args: IGNORE(_fail),
);

// ============================================================
// fmove/2 -- move between FP and X/Y registers
// Structure: fmove(src, dst)
//
// Cases:
//   FP -> X/Y: read raw f64 from fpregs, box it on heap, store to X/Y
//   X/Y -> FP: load boxed float from X/Y, extract f64, store to fpregs
//   FP -> FP:  copy raw f64 between fpregs
// ============================================================
define_opcode!(_vm, ctx, curr_p,
  name: OpcodeFmove, arity: 2,
  run: { Self::fmove_impl(ctx, curr_p, src, dst) },
  args: term(src), term(dst),
);

impl OpcodeFmove {
  #[inline]
  fn fmove_impl(
    ctx: &mut RuntimeContext,
    curr_p: &mut Process,
    src: Term,
    dst: Term,
  ) -> RtResult<DispatchResult> {
    let src_is_fp = src.is_register_float();
    let dst_is_fp = dst.is_register_float();

    if src_is_fp && dst_is_fp {
      // FP -> FP
      let val = ctx.fpregs[src.get_reg_value()];
      ctx.fpregs[dst.get_reg_value()] = val;
    } else if src_is_fp {
      // FP -> X/Y: box the float on the heap
      let val = ctx.fpregs[src.get_reg_value()];
      let hp = curr_p.get_heap_mut();
      let boxed_float = Term::make_float(hp, val)?;
      ctx.store_value(boxed_float, dst, curr_p.get_heap_mut())?;
    } else if dst_is_fp {
      // X/Y -> FP: load boxed float, extract raw f64
      let hp = curr_p.get_heap_mut();
      let loaded = ctx.load(src, hp);
      ctx.fpregs[dst.get_reg_value()] = term_to_f64(loaded);
    } else {
      // Neither src nor dst is an FP register -- fall back to generic move
      let hp = curr_p.get_heap_mut();
      let val = ctx.load(src, hp);
      ctx.store_value(val, dst, curr_p.get_heap_mut())?;
    }

    Ok(DispatchResult::Normal)
  }
}

// ============================================================
// fconv/2 -- convert integer/float to FP register
// Structure: fconv(src, dst_fpreg)
// src is an X/Y register or literal; dst is always an FP register.
// ============================================================
define_opcode!(_vm, ctx, curr_p,
  name: OpcodeFconv, arity: 2,
  run: { Self::fconv_impl(ctx, curr_p, src, dst) },
  args: term(src), term(dst),
);

impl OpcodeFconv {
  #[inline]
  fn fconv_impl(
    ctx: &mut RuntimeContext,
    curr_p: &mut Process,
    src: Term,
    dst: Term,
  ) -> RtResult<DispatchResult> {
    let hp = curr_p.get_heap_mut();
    let loaded = ctx.load(src, hp);
    let f_val = term_to_f64(loaded);

    debug_assert!(
      dst.is_register_float(),
      "{}fconv: dst must be an FP register, got {}",
      module(),
      dst
    );
    ctx.fpregs[dst.get_reg_value()] = f_val;
    Ok(DispatchResult::Normal)
  }
}

// ============================================================
// fadd/4 -- FP addition
// Structure: fadd(fail, src1, src2, dst) -- all FP regs
// ============================================================
define_opcode!(_vm, ctx, _curr_p,
  name: OpcodeFadd, arity: 4,
  run: {
    let a = ctx.fpregs[fp_index(src1)];
    let b = ctx.fpregs[fp_index(src2)];
    ctx.fpregs[fp_index(dst)] = a + b;
    Ok(DispatchResult::Normal)
  },
  args: IGNORE(_fail), term(src1), term(src2), term(dst),
);

// ============================================================
// fsub/4 -- FP subtraction
// Structure: fsub(fail, src1, src2, dst) -- all FP regs
// ============================================================
define_opcode!(_vm, ctx, _curr_p,
  name: OpcodeFsub, arity: 4,
  run: {
    let a = ctx.fpregs[fp_index(src1)];
    let b = ctx.fpregs[fp_index(src2)];
    ctx.fpregs[fp_index(dst)] = a - b;
    Ok(DispatchResult::Normal)
  },
  args: IGNORE(_fail), term(src1), term(src2), term(dst),
);

// ============================================================
// fmul/4 -- FP multiplication
// Structure: fmul(fail, src1, src2, dst) -- all FP regs
// ============================================================
define_opcode!(_vm, ctx, _curr_p,
  name: OpcodeFmul, arity: 4,
  run: {
    let a = ctx.fpregs[fp_index(src1)];
    let b = ctx.fpregs[fp_index(src2)];
    ctx.fpregs[fp_index(dst)] = a * b;
    Ok(DispatchResult::Normal)
  },
  args: IGNORE(_fail), term(src1), term(src2), term(dst),
);

// ============================================================
// fdiv/4 -- FP division
// Structure: fdiv(fail, src1, src2, dst) -- all FP regs
// ============================================================
define_opcode!(_vm, ctx, _curr_p,
  name: OpcodeFdiv, arity: 4,
  run: {
    let a = ctx.fpregs[fp_index(src1)];
    let b = ctx.fpregs[fp_index(src2)];
    ctx.fpregs[fp_index(dst)] = a / b;
    Ok(DispatchResult::Normal)
  },
  args: IGNORE(_fail), term(src1), term(src2), term(dst),
);

// ============================================================
// fnegate/3 -- negate FP register
// Structure: fnegate(fail, src, dst) -- all FP regs
// ============================================================
define_opcode!(_vm, ctx, _curr_p,
  name: OpcodeFnegate, arity: 3,
  run: {
    let val = ctx.fpregs[fp_index(src)];
    ctx.fpregs[fp_index(dst)] = -val;
    Ok(DispatchResult::Normal)
  },
  args: IGNORE(_fail), term(src), term(dst),
);
