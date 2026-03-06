use crate::{
  beam::disp_result::DispatchResult,
  defs::exc_type::ExceptionType,
  emulator::{
    gen_atoms,
    heap::THeapOwner,
    process::Process,
    runtime_ctx::*,
    vm::VM,
  },
  fail::{self, RtErr, RtResult},
  term::{
    term_builder::{
      list_builder::ListBuilder,
      tuple_builder::{tuple2, tuple4},
    },
    Term,
  },
};

// Set up a try-catch stack frame for possible stack unwinding. Label points
// at a `try_case` opcode where the error will be investigated.
// We just write the cp given to the given Y register as a catch-value.
// Structure: try(reg:regy, label:cp)
define_opcode!(_vm, ctx, curr_p,
  name: OpcodeTry, arity: 2,
  run: { Self::try_opcode(curr_p, yreg, catch_label) },
  args: yreg(yreg), cp_or_nil(catch_label),
);

impl OpcodeTry {
  #[inline]
  pub fn try_opcode(
    curr_p: &mut Process,
    yreg: Term,
    catch_label: Term,
  ) -> RtResult<DispatchResult> {
    curr_p.num_catches += 1;

    let hp = curr_p.get_heap_mut();

    // Write catch value into the given stack register
    let catch_val = Term::make_catch(catch_label.get_cp_ptr());
    hp.set_y(yreg.get_reg_value(), catch_val)?;
    // curr_p.heap.print_stack();

    Ok(DispatchResult::Normal)
  }
}

// End try-catch by clearing the catch value on stack
// Structure: try_end(reg:regy)
define_opcode!(_vm, ctx, curr_p,
  name: OpcodeTryEnd, arity: 1,
  run: { Self::try_end(ctx, curr_p, y) },
  args: yreg(y),
);

impl OpcodeTryEnd {
  #[inline]
  pub fn try_end(
    ctx: &mut RuntimeContext,
    curr_p: &mut Process,
    yreg: Term,
  ) -> RtResult<DispatchResult> {
    curr_p.num_catches -= 1;

    let hp = curr_p.get_heap_mut();
    hp.set_y(yreg.get_reg_value(), Term::nil())?;

    // Not sure why this is happening here, copied from Erlang/OTP
    if ctx.get_x(0).is_non_value() {
      // Clear error and shift regs x1-x2-x3 to x0-x1-x2
      curr_p.clear_exception();
      ctx.set_x(0, ctx.get_x(1));
      ctx.set_x(1, ctx.get_x(2));
      ctx.set_x(2, ctx.get_x(3));
    }

    Ok(DispatchResult::Normal)
  }
}

// Concludes the catch, removes catch value from stack and shifts registers
// contents to prepare for exception checking.
// Structure: try_case(reg:regy)
define_opcode!(_vm, ctx, curr_p,
  name: OpcodeTryCase, arity: 1,
  run: { Self::try_case(ctx, curr_p, y) },
  args: yreg(y),
);

impl OpcodeTryCase {
  #[inline]
  pub fn try_case(
    ctx: &mut RuntimeContext,
    curr_p: &mut Process,
    yreg: Term,
  ) -> RtResult<DispatchResult> {
    curr_p.num_catches -= 1;

    let hp = curr_p.get_heap_mut();
    hp.set_y(yreg.get_reg_value(), Term::nil())?;

    // Clear error and shift regs x1-x2-x3 to x0-x1-x2
    curr_p.clear_exception();
    ctx.set_x(0, ctx.get_x(1));
    ctx.set_x(1, ctx.get_x(2));
    ctx.set_x(2, ctx.get_x(3));

    Ok(DispatchResult::Normal)
  }
}

// Raises the exception. The instruction is encumbered by backward
// compatibility. Arg0 is a stack trace and Arg1 is the value accompanying
// the exception. The reason of the raised exception is dug up from the stack
// trace. Fixed by `raw_raise` in otp21.
// Structure: raise(stacktrace:term, exc_value:term)
define_opcode!(_vm, ctx, curr_p,
  name: OpcodeRaise, arity: 2,
  run: { Self::raise(stacktrace, exc_value) },
  args: load(stacktrace), load(exc_value),
);

impl OpcodeRaise {
  #[inline]
  pub fn raise(raise_trace: Term, raise_val: Term) -> RtResult<DispatchResult> {
    let exc_type = match get_trace_from_exc(raise_trace) {
      None => ExceptionType::Error,
      Some(et) => et,
    };

    // curr_p.set_exception(exc_type, raise_val);
    // curr_p.set_stacktrace(raise_trace);

    Err(RtErr::Exception(exc_type, raise_val))
  }
}

/// In BEAM this extracts pointer to StackTrace struct stored inside bignum on
/// heap. Here for now we just assume it is always error.
fn get_trace_from_exc(trace: Term) -> Option<ExceptionType> {
  if trace == Term::nil() {
    return None;
  }
  Some(ExceptionType::Error)
}

// Raises a {try_clause, Value} error when no clause in a try matched.
// Structure: try_case_end(val:term)
define_opcode!(_vm, _ctx, curr_p,
  name: OpcodeTryCaseEnd, arity: 1,
  run: { Self::try_case_end(curr_p, val) },
  args: load(val),
);

impl OpcodeTryCaseEnd {
  #[inline]
  pub fn try_case_end(
    curr_p: &mut Process,
    val: Term,
  ) -> RtResult<DispatchResult> {
    fail::create::generic_tuple2_fail(
      gen_atoms::TRY_CLAUSE,
      val,
      curr_p.get_heap_mut(),
    )
  }
}

// Re-raises an exception from x(0)=class, x(1)=value, x(2)=stacktrace.
// Structure: raw_raise()
define_opcode!(_vm, ctx, _curr_p,
  name: OpcodeRawRaise, arity: 0,
  run: { Self::raw_raise(ctx) },
  args:
);

impl OpcodeRawRaise {
  #[inline]
  pub fn raw_raise(ctx: &mut RuntimeContext) -> RtResult<DispatchResult> {
    let class = ctx.get_x(0);
    let value = ctx.get_x(1);
    // x(2) is the stacktrace, which we ignore for now

    let exc_type = if class == gen_atoms::ERROR {
      ExceptionType::Error
    } else if class == gen_atoms::EXIT {
      ExceptionType::Exit
    } else if class == gen_atoms::THROW {
      ExceptionType::Throw
    } else {
      ExceptionType::Error
    };

    Err(RtErr::Exception(exc_type, value))
  }
}

// Builds a stacktrace term from the raw stacktrace in x(0).
// Replaces x(0) with an Erlang stacktrace list.
// Structure: build_stacktrace()
define_opcode!(vm, ctx, curr_p,
  name: OpcodeBuildStacktrace, arity: 0,
  run: { Self::build_stacktrace(vm, ctx, curr_p) },
  args:
);

impl OpcodeBuildStacktrace {
  #[inline]
  pub fn build_stacktrace(
    vm: &mut VM,
    ctx: &mut RuntimeContext,
    curr_p: &mut Process,
  ) -> RtResult<DispatchResult> {
    // Build a simple stacktrace from the current CP.
    // Erlang stacktrace format: [{M, F, A, Info} | ...]
    // where Info is a proplist like [{file, File}, {line, Line}]
    let mut trace = Term::nil();

    if !ctx.cp.is_null() {
      if let Some(mfa) = vm.code_server.code_reverse_lookup(ctx.cp) {
        let hp = curr_p.get_heap_mut();
        let arity_term = Term::make_small_unsigned(mfa.arity);
        let info = Term::nil(); // empty info list for now
        let entry = tuple4(hp, mfa.m, mfa.f, arity_term, info)?;
        // Build list: [entry]
        unsafe {
          let mut lb = ListBuilder::new()?;
          lb.append(entry, hp)?;
          trace = lb.make_term();
        }
      }
    }

    ctx.set_x(0, trace);
    Ok(DispatchResult::Normal)
  }
}

// Set up a catch frame. Similar to `try` — stores a catch value in the Y
// register and increments the catch counter. The difference from `try` is in
// how exceptions are unwound by `catch_end` vs `try_end`/`try_case`.
// Structure: catch(reg:regy, label:cp)
define_opcode!(_vm, ctx, curr_p,
  name: OpcodeCatch, arity: 2,
  run: { Self::catch_opcode(curr_p, yreg, catch_label) },
  args: yreg(yreg), cp_or_nil(catch_label),
);

impl OpcodeCatch {
  #[inline]
  pub fn catch_opcode(
    curr_p: &mut Process,
    yreg: Term,
    catch_label: Term,
  ) -> RtResult<DispatchResult> {
    curr_p.num_catches += 1;

    let hp = curr_p.get_heap_mut();

    // Write catch value into the given stack register
    let catch_val = Term::make_catch(catch_label.get_cp_ptr());
    hp.set_y(yreg.get_reg_value(), catch_val)?;

    Ok(DispatchResult::Normal)
  }
}

// Ends a catch block. Clears the catch value, decrements catch count.
// If an exception was caught (x(0) is non-value), wraps the result as
// {'EXIT', Reason} in x(0) for the catch expression.
// Structure: catch_end(reg:regy)
define_opcode!(_vm, ctx, curr_p,
  name: OpcodeCatchEnd, arity: 1,
  run: { Self::catch_end(ctx, curr_p, y) },
  args: yreg(y),
);

impl OpcodeCatchEnd {
  #[inline]
  pub fn catch_end(
    ctx: &mut RuntimeContext,
    curr_p: &mut Process,
    yreg: Term,
  ) -> RtResult<DispatchResult> {
    curr_p.num_catches -= 1;

    let hp = curr_p.get_heap_mut();
    hp.set_y(yreg.get_reg_value(), Term::nil())?;

    if ctx.get_x(0).is_non_value() {
      // An exception was caught
      curr_p.clear_exception();

      let class = ctx.get_x(1);
      let value = ctx.get_x(2);

      if class == gen_atoms::THROW {
        // For throw exceptions, the value is used directly
        ctx.set_x(0, value);
      } else {
        // For error and exit exceptions, wrap as {'EXIT', Reason}
        let hp = curr_p.get_heap_mut();
        let exit_tuple = tuple2(hp, gen_atoms::UPPER_EXIT, value)?;
        ctx.set_x(0, exit_tuple);
      }
    }

    Ok(DispatchResult::Normal)
  }
}
