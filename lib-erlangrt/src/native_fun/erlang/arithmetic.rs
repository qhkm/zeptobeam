use crate::{
  big,
  emulator::{arith::multiplication, heap::THeapOwner, process::Process, vm::VM},
  fail::{self, RtResult},
  term::*,
};

fn module() -> &'static str {
  "native funs module for erlang[arith]: "
}

#[inline]
fn small_i(a: Term) -> Option<isize> {
  if a.is_small() {
    Some(a.get_small_signed())
  } else {
    None
  }
}

/// Subtraction for 2 mixed terms. Algorithm comes from Erlang/OTP file
/// `erl_arith.c`, function `erts_mixed_minus`
pub fn nativefun_minus_2(
  _vm: &mut VM,
  cur_proc: &mut Process,
  args: &[Term],
) -> RtResult<Term> {
  assert_eq!(args.len(), 2, "{}'-'/2 takes 2 args", module());
  let a: Term = args[0];
  let b: Term = args[1];
  if a.is_small() {
    if b.is_small() {
      subtract_two_small(cur_proc, a, b)
    } else {
      // TODO: See Erlang OTP erl_arith.c function erts_mixed_minus
      unimplemented!("{}subtract: b={} other than small", module(), b)
    }
  } else {
    unimplemented!("{}subtract: a={} other than small", module(), a)
  }
}

/// Addition for 2 mixed terms.
pub fn nativefun_plus_2(
  _vm: &mut VM,
  cur_proc: &mut Process,
  args: &[Term],
) -> RtResult<Term> {
  assert_eq!(args.len(), 2, "{}ubif_sminus_2_2 takes 2 args", module());
  let a: Term = args[0];
  let b: Term = args[1];
  if a.is_small() {
    if b.is_small() {
      add_two_small(cur_proc, a, b)
    } else {
      // TODO: See Erlang OTP erl_arith.c function erts_mixed_plus
      unimplemented!("{}subtract: b={} other than small", module(), b)
    }
  } else {
    unimplemented!("{}subtract: a={} other than small", module(), a)
  }
}

/// So the check above has concluded that `a` and `b` are both small integers.
/// Implement subtraction, possibly creating a big integer.
fn subtract_two_small(cur_proc: &mut Process, a: Term, b: Term) -> RtResult<Term> {
  // Both a and b are small, we've got an easy time
  let iresult = a.get_small_signed() - b.get_small_signed();
  // Even better: the result is also a small
  if Term::small_fits(iresult) {
    return Ok(Term::make_small_signed(iresult));
  }

  create_bigint(cur_proc, iresult)
}

/// So the check above has concluded that `a` and `b` are both small integers.
/// Implement addition, possibly creating a big integer.
fn add_two_small(cur_proc: &mut Process, a: Term, b: Term) -> RtResult<Term> {
  // Both a and b are small, we've got an easy time.
  // The overflow in addition of two smalls will always fit Rust integer because
  // small use less bits than a Rust integer.
  let iresult = a.get_small_signed() + b.get_small_signed();
  // Even better: the result is also a small
  if Term::small_fits(iresult) {
    return Ok(Term::make_small_signed(iresult));
  }
  create_bigint(cur_proc, iresult)
}

/// Multiplication for 2 mixed terms.
pub fn nativefun_multiply_2(
  _vm: &mut VM,
  cur_proc: &mut Process,
  args: &[Term],
) -> RtResult<Term> {
  assert_eq!(args.len(), 2, "{}ubif_stimes_2_2 takes 2 args", module());
  let a: Term = args[0];
  let b: Term = args[1];
  return multiplication::multiply(cur_proc.get_heap_mut(), a, b);
}

pub fn nativefun_band_2(
  _vm: &mut VM,
  _cur_proc: &mut Process,
  args: &[Term],
) -> RtResult<Term> {
  assert_eq!(args.len(), 2, "{}band/2 takes 2 args", module());
  let (Some(a), Some(b)) = (small_i(args[0]), small_i(args[1])) else {
    return fail::create::badarg();
  };
  Ok(Term::make_small_signed(a & b))
}

pub fn nativefun_bor_2(
  _vm: &mut VM,
  _cur_proc: &mut Process,
  args: &[Term],
) -> RtResult<Term> {
  assert_eq!(args.len(), 2, "{}bor/2 takes 2 args", module());
  let (Some(a), Some(b)) = (small_i(args[0]), small_i(args[1])) else {
    return fail::create::badarg();
  };
  Ok(Term::make_small_signed(a | b))
}

pub fn nativefun_bxor_2(
  _vm: &mut VM,
  _cur_proc: &mut Process,
  args: &[Term],
) -> RtResult<Term> {
  assert_eq!(args.len(), 2, "{}bxor/2 takes 2 args", module());
  let (Some(a), Some(b)) = (small_i(args[0]), small_i(args[1])) else {
    return fail::create::badarg();
  };
  Ok(Term::make_small_signed(a ^ b))
}

pub fn nativefun_bnot_1(
  _vm: &mut VM,
  _cur_proc: &mut Process,
  args: &[Term],
) -> RtResult<Term> {
  assert_eq!(args.len(), 1, "{}bnot/1 takes 1 arg", module());
  let Some(a) = small_i(args[0]) else {
    return fail::create::badarg();
  };
  Ok(Term::make_small_signed(!a))
}

pub fn nativefun_bsl_2(
  _vm: &mut VM,
  _cur_proc: &mut Process,
  args: &[Term],
) -> RtResult<Term> {
  assert_eq!(args.len(), 2, "{}bsl/2 takes 2 args", module());
  let (Some(a), Some(shift)) = (small_i(args[0]), small_i(args[1])) else {
    return fail::create::badarg();
  };
  if shift < 0 {
    return fail::create::badarg();
  }
  match a.checked_shl(shift as u32) {
    Some(v) if Term::small_fits(v) => Ok(Term::make_small_signed(v)),
    _ => fail::create::badarg(),
  }
}

pub fn nativefun_bsr_2(
  _vm: &mut VM,
  _cur_proc: &mut Process,
  args: &[Term],
) -> RtResult<Term> {
  assert_eq!(args.len(), 2, "{}bsr/2 takes 2 args", module());
  let (Some(a), Some(shift)) = (small_i(args[0]), small_i(args[1])) else {
    return fail::create::badarg();
  };
  if shift < 0 {
    return fail::create::badarg();
  }
  let sh = (shift as u32).min(isize::BITS - 1);
  Ok(Term::make_small_signed(a >> sh))
}

pub fn nativefun_div_2(
  _vm: &mut VM,
  cur_proc: &mut Process,
  args: &[Term],
) -> RtResult<Term> {
  assert_eq!(args.len(), 2, "{}div/2 takes 2 args", module());
  let (Some(a), Some(b)) = (small_i(args[0]), small_i(args[1])) else {
    return fail::create::badarith();
  };
  if b == 0 {
    return fail::create::badarith();
  }
  let Some(v) = a.checked_div(b) else {
    return fail::create::badarith();
  };
  if Term::small_fits(v) {
    Ok(Term::make_small_signed(v))
  } else {
    create_bigint(cur_proc, v)
  }
}

pub fn nativefun_rem_2(
  _vm: &mut VM,
  _cur_proc: &mut Process,
  args: &[Term],
) -> RtResult<Term> {
  assert_eq!(args.len(), 2, "{}rem/2 takes 2 args", module());
  let (Some(a), Some(b)) = (small_i(args[0]), small_i(args[1])) else {
    return fail::create::badarith();
  };
  if b == 0 {
    return fail::create::badarith();
  }
  let Some(v) = a.checked_rem(b) else {
    return fail::create::badarith();
  };
  Ok(Term::make_small_signed(v))
}

/// Float division (`erlang:'/'/2`). Always returns a float.
/// Both arguments can be integer or float. Division by zero raises badarith.
pub fn nativefun_float_div_2(
  _vm: &mut VM,
  cur_proc: &mut Process,
  args: &[Term],
) -> RtResult<Term> {
  assert_eq!(args.len(), 2, "{}'/'/2 takes 2 args", module());
  let a = args[0];
  let b = args[1];

  let fa = term_to_f64_opt(a);
  let fb = term_to_f64_opt(b);

  match (fa, fb) {
    (Some(av), Some(bv)) => {
      if bv == 0.0 {
        return fail::create::badarith();
      }
      let hp = cur_proc.get_heap_mut();
      Ok(Term::make_float(hp, av / bv)?)
    }
    _ => fail::create::badarith(),
  }
}

/// `erlang:abs/1`. Returns absolute value preserving type (int->int, float->float).
pub fn nativefun_abs_1(
  _vm: &mut VM,
  cur_proc: &mut Process,
  args: &[Term],
) -> RtResult<Term> {
  assert_eq!(args.len(), 1, "{}abs/1 takes 1 arg", module());
  let a = args[0];

  if a.is_small() {
    let v = a.get_small_signed();
    // Use wrapping_abs: for isize::MIN this wraps back to isize::MIN (negative),
    // but small ints have a much narrower range than isize, so in practice
    // abs will always succeed here.
    let abs_v = v.wrapping_abs();
    if Term::small_fits(abs_v) {
      Ok(Term::make_small_signed(abs_v))
    } else {
      create_bigint(cur_proc, abs_v)
    }
  } else if a.is_float() {
    let fv = unsafe { a.get_float_unchecked() };
    let hp = cur_proc.get_heap_mut();
    Ok(Term::make_float(hp, fv.abs())?)
  } else if a.is_big_int() {
    // TODO: implement proper big int abs
    fail::create::badarg()
  } else {
    fail::create::badarg()
  }
}

/// `erlang:float/1`. Converts integer to float; float stays float.
pub fn nativefun_float_1(
  _vm: &mut VM,
  cur_proc: &mut Process,
  args: &[Term],
) -> RtResult<Term> {
  assert_eq!(args.len(), 1, "{}float/1 takes 1 arg", module());
  let a = args[0];

  if a.is_float() {
    Ok(a)
  } else if a.is_small() {
    let v = a.get_small_signed() as f64;
    let hp = cur_proc.get_heap_mut();
    Ok(Term::make_float(hp, v)?)
  } else if a.is_big_int() {
    // TODO: convert big int to float
    fail::create::badarg()
  } else {
    fail::create::badarg()
  }
}

/// Helper: convert a Term (small int or boxed float) to f64, or None.
#[inline]
fn term_to_f64_opt(t: Term) -> Option<f64> {
  if t.is_small() {
    Some(t.get_small_signed() as f64)
  } else if t.is_float() {
    Some(unsafe { t.get_float_unchecked() })
  } else {
    None
  }
}

// TODO: shorten, use only heap of the process, inline, move to a lib module in arith
fn create_bigint(cur_proc: &mut Process, iresult: isize) -> RtResult<Term> {
  // We're out of luck - the result is not a small, but we have BigInt!
  let p = big::from_isize(cur_proc.get_heap_mut(), iresult)?;
  Ok(p)
}
