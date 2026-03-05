use crate::{
  command_line_args::ErlStartArgs,
  emulator::{atom, mfa::ModFunArgs, spawn_options::SpawnOptions, vm::VM},
  fail::RtResult,
  term::*,
};
use std::{
  io::{stdout, Write},
  thread, time,
};

/// Entry point for the command-line interface. Pre-parse command line args
/// by calling StartArgs methods, or just use default constructed StartArgs.
pub fn start_emulator(args: &mut ErlStartArgs) {
  if let Err(err) = run_emulator(args, "test2", "test") {
    eprintln!("erlangrt: emulator failed: {:?}", err);
  }
}

/// Fallible emulator entrypoint for binaries that need proper exit codes.
pub fn run_emulator(
  args: &mut ErlStartArgs,
  module: &str,
  function: &str,
) -> RtResult<()> {
  if cfg!(feature = "r20") {
    println!("Erlang Runtime (compat OTP 20)");
  }
  if cfg!(feature = "r21") {
    println!("Erlang Runtime (compat OTP 21)");
  }
  if cfg!(feature = "r22") {
    println!("Erlang Runtime (compat OTP 22)");
  }

  let mut beam_vm = VM::new(args);

  let mfargs = ModFunArgs::with_args_list(
    atom::from_str(module),
    atom::from_str(function),
    Term::nil(),
  );
  let _rootp = beam_vm.create_process(Term::nil(), &mfargs, &SpawnOptions::default())?;

  println!("Running {}:{}/0...", module, function);
  while beam_vm.tick()? {
    thread::sleep(time::Duration::from_millis(0));
  }
  let _ = stdout().flush();
  Ok(())
}
