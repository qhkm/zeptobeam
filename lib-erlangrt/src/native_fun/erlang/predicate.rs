use crate::{
  emulator::{process::Process, vm::VM},
  fail::{self, RtResult},
  native_fun::assert_arity,
  term::{boxed::Map, Term},
};

#[allow(dead_code)]
fn module() -> &'static str {
  "native funs module for erlang[predicate]: "
}

/// Return `true` if the value is a boolean (atom `true` or atom `false`)
pub fn nativefun_is_boolean_1(
  _vm: &mut VM,
  _curr_p: &mut Process,
  args: &[Term],
) -> RtResult<Term> {
  assert_arity("erlang:is_boolean", 1, args);
  Ok(Term::make_bool(args[0].is_bool()))
}

// ---- erlang:is_map_key/2 ----
// Check if key exists in map.
pub fn nativefun_is_map_key_2(
  _vm: &mut VM,
  _curr_p: &mut Process,
  args: &[Term],
) -> RtResult<Term> {
  assert_arity("erlang:is_map_key", 2, args);
  let key = args[0];
  let map = args[1];

  if !map.is_map() {
    return fail::create::badarg();
  }

  if map == Term::empty_map() {
    return Ok(Term::make_bool(false));
  }

  let map_p = map.get_box_ptr::<Map>();
  let found = unsafe { Map::get(map_p, key)? }.is_some();
  Ok(Term::make_bool(found))
}

// ---- erlang:map_size/1 ----
// Get map size.
pub fn nativefun_map_size_1(
  _vm: &mut VM,
  _curr_p: &mut Process,
  args: &[Term],
) -> RtResult<Term> {
  assert_arity("erlang:map_size", 1, args);
  let map = args[0];

  if !map.is_map() {
    return fail::create::badarg();
  }

  if map == Term::empty_map() {
    return Ok(Term::make_small_unsigned(0));
  }

  Ok(Term::make_small_unsigned(map.map_size()))
}
