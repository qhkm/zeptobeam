use crate::{
  beam::disp_result::{DispatchResult, YieldType},
  emulator::{heap::THeapOwner, process::Process, runtime_ctx::*, vm::VM},
  fail::{self, RtResult},
  term::*,
};

// Sends to x0 value x1, x1 is moved to x0 as result of the operation.
// If process with pid x0 does not exist, no error is raised.
// Structure: send()
define_opcode!(vm, ctx, _curr_p,
  name: OpcodeSend, arity: 0,
  run: { Self::send(vm, ctx) },
  args:
);

impl OpcodeSend {
  #[inline]
  pub fn send(vm: &mut VM, ctx: &mut RuntimeContext) -> RtResult<DispatchResult> {
    // let sched = vm.get_scheduler_p();
    let x1 = ctx.get_x(1);
    let x0 = ctx.get_x(0);
    if !x0.is_pid() {
      return fail::create::badarg();
    }
    let p = vm.processes.unsafe_lookup_pid_mut(x0);
    if !p.is_null() {
      unsafe {
        (*p).deliver_message(&mut vm.processes, x1)?;
      }
    }

    ctx.set_x(0, x1);
    Ok(DispatchResult::Normal)
  }
}

// Picks up next message in the message queue and places it into `x0`.
// If there is no next message, jumps to `fail` label which points to a `wait`
// or `wait_timeout` instruction.
// Structure: loop_rec(fail:cp, _source)
define_opcode!(_vm, ctx, curr_p,
  name: OpcodeLoopRec, arity: 2,
  run: { Self::loop_rec(ctx, curr_p, fail) },
  args: cp_or_nil(fail), IGNORE(source),
);

impl OpcodeLoopRec {
  #[inline]
  pub fn loop_rec(
    ctx: &mut RuntimeContext,
    curr_p: &mut Process,
    fail: Term,
  ) -> RtResult<DispatchResult> {
    if let Some(msg) = curr_p.mailbox.get_current() {
      ctx.set_x(0, msg);
    } else {
      ctx.jump(fail);
    }
    Ok(DispatchResult::Normal)
  }
}

// Advances message receive pointer to the next message then jumps to label
// which points to a `loop_rec` instruction.
// Structure: loop_rec_end(label:cp)
define_opcode!(_vm, ctx, curr_p,
  name: OpcodeLoopRecEnd, arity: 1,
  run: { Self::loop_rec_end(ctx, curr_p, label) },
  args: cp_or_nil(label),
);

impl OpcodeLoopRecEnd {
  #[inline]
  pub fn loop_rec_end(
    ctx: &mut RuntimeContext,
    curr_p: &mut Process,
    label: Term,
  ) -> RtResult<DispatchResult> {
    curr_p.mailbox.step_over();
    ctx.jump(label);
    Ok(DispatchResult::Normal)
  }
}

// Removes the current message in the process message list and moves it to `x0`
// Structure: remove_message()
define_opcode!(_vm, ctx, curr_p,
  name: OpcodeRemoveMessage, arity: 0,
  run: {
    let message = curr_p.mailbox.remove_current();
    ctx.set_x(0, message);
    Ok(DispatchResult::Normal)
  },
  args:
);

// Suspends the current process and sets the ip to the label (beginning of the
// receive loop).
// Structure: wait(label:cp)
define_opcode!(_vm, ctx, _curr_p,
  name: OpcodeWait, arity: 1,
  run: { Self::wait(ctx, label) },
  args: cp_or_nil(label),
);

impl OpcodeWait {
  #[inline]
  pub fn wait(ctx: &mut RuntimeContext, label: Term) -> RtResult<DispatchResult> {
    ctx.jump(label);
    Ok(DispatchResult::Yield(YieldType::InfiniteWait))
  }
}

// Resets the receive timeout state. Called after a message has been selected
// from the mailbox in a receive with a timeout, or after a timeout fires.
// Structure: timeout()
define_opcode!(_vm, _ctx, curr_p,
  name: OpcodeTimeout, arity: 0,
  run: {
    // Reset the mailbox read pointer to the beginning
    curr_p.mailbox.reset_read_index();
    Ok(DispatchResult::Normal)
  },
  args:
);

// Sets up a timeout for a receive expression. If the timeout value is 0,
// immediately jump to the label (immediate timeout). Otherwise set up a timed
// wait. For now, we implement immediate timeout for timeout value 0, and
// yield for all other values.
// Structure: wait_timeout(fail_label:cp, timeout_val:term)
define_opcode!(_vm, ctx, curr_p,
  name: OpcodeWaitTimeout, arity: 2,
  run: { Self::wait_timeout(ctx, fail_label, timeout_val) },
  args: cp_or_nil(fail_label), load(timeout_val),
);

impl OpcodeWaitTimeout {
  #[inline]
  pub fn wait_timeout(
    ctx: &mut RuntimeContext,
    fail_label: Term,
    timeout_val: Term,
  ) -> RtResult<DispatchResult> {
    // If timeout is 0, the timeout fires immediately
    if timeout_val.is_small() && timeout_val.get_small_unsigned() == 0 {
      // Do not jump — fall through to the timeout code (next instruction)
      return Ok(DispatchResult::Normal);
    }

    // For non-zero timeout, yield and wait. When rescheduled, jump to the
    // receive loop label to retry.
    ctx.jump(fail_label);
    Ok(DispatchResult::Yield(YieldType::InfiniteWait))
  }
}
