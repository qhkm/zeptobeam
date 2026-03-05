use erlangrt::{command_line_args::ErlStartArgs, lib_main::run_emulator};
use std::{env, process};

fn read_path_list(var_name: &str) -> Vec<String> {
  let Ok(raw) = env::var(var_name) else {
    return Vec::new();
  };
  env::split_paths(&raw)
    .map(|path| path.to_string_lossy().trim().to_string())
    .filter(|path| !path.is_empty())
    .collect()
}

fn default_search_path() -> Vec<String> {
  let mut paths = vec!["priv/".to_string()];

  if let Ok(stdlib_ebin) = env::var("ZEPTOBEAM_STDLIB_EBIN") {
    if !stdlib_ebin.trim().is_empty() {
      paths.push(stdlib_ebin.trim().to_string());
    }
  }

  paths.extend(read_path_list("ZEPTOBEAM_BEAM_PATHS"));
  paths.extend(read_path_list("ERL_LIBS"));
  paths
}

fn main() {
  let in_args: Vec<String> = env::args().collect();
  let mut args = ErlStartArgs::new(&in_args);
  args.populate_with(in_args.iter());
  println!("{args:?}");

  // Parse -m module -f function from command line (default: test2:test)
  let mut module = "test2".to_string();
  let mut function = "test".to_string();
  let mut iter = in_args.iter().skip(1);
  while let Some(arg) = iter.next() {
    match arg.as_str() {
      "-m" => {
        if let Some(val) = iter.next() {
          module = val.clone();
        }
      }
      "-f" => {
        if let Some(val) = iter.next() {
          function = val.clone();
        }
      }
      _ => {}
    }
  }

  // TODO: For windows, support ERL_CONSOLE_MODE, with ERL_EMULATOR_DLL from erlexec.c
  // TODO: For non-Windows, support CERL_DETACHED_PROG?

  // TODO: add -pa, -pz options
  args.search_path = default_search_path();

  // Get going now
  if let Err(err) = run_emulator(&mut args, &module, &function) {
    eprintln!("erlexec: {:?}", err);
    process::exit(1);
  }
  println!("erlexec: Finished.");
}
