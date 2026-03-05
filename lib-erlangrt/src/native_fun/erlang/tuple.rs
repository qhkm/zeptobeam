use crate::{
  emulator::{heap::THeapOwner, process::Process, vm::VM},
  fail::{self, RtResult},
  native_fun::assert_arity,
  term::{
    boxed::{self, Tuple},
    cons,
    term_builder::ListBuilder,
    Term,
  },
};

// Return size of a tuple or a binary object.
define_nativefun!(_vm, _proc, args,
  name: "erlang:size/1", struct_name: NfErlangSize1, arity: 1,
  invoke: { size_1(t) },
  args: term(t),
);

#[inline]
fn size_1(t: Term) -> RtResult<Term> {
  if t.is_tuple() {
    let t_ptr = t.get_tuple_ptr();
    let arity = unsafe { (*t_ptr).get_arity() };
    Ok(Term::make_small_unsigned(arity))
  } else if t.is_binary() {
    let bin_ptr = unsafe { boxed::Binary::get_trait_from_term(t) };
    let bin_size = unsafe { (*bin_ptr).get_byte_size() };
    Ok(Term::make_small_unsigned(bin_size.bytes()))
  } else {
    fail::create::badarg()
  }
}

// ---- erlang:element/2 ----
// Get tuple element (1-based index).
pub fn nativefun_element_2(
  _vm: &mut VM,
  _curr_p: &mut Process,
  args: &[Term],
) -> RtResult<Term> {
  assert_arity("erlang:element", 2, args);
  let index_term = args[0];
  let tuple_term = args[1];

  if !index_term.is_small() || !tuple_term.is_tuple() {
    return fail::create::badarg();
  }

  let index = index_term.get_small_signed();
  if index < 1 {
    return fail::create::badarg();
  }

  let tuple_p = tuple_term.get_tuple_ptr();
  let arity = unsafe { (*tuple_p).get_arity() };
  let zero_index = (index - 1) as usize;

  if zero_index >= arity {
    return fail::create::badarg();
  }

  Ok(unsafe { (*tuple_p).get_element(zero_index) })
}

// ---- erlang:setelement/3 ----
// Set tuple element (1-based index), return new tuple.
pub fn nativefun_setelement_3(
  _vm: &mut VM,
  curr_p: &mut Process,
  args: &[Term],
) -> RtResult<Term> {
  assert_arity("erlang:setelement", 3, args);
  let index_term = args[0];
  let tuple_term = args[1];
  let value = args[2];

  if !index_term.is_small() || !tuple_term.is_tuple() {
    return fail::create::badarg();
  }

  let index = index_term.get_small_signed();
  if index < 1 {
    return fail::create::badarg();
  }

  let old_tuple_p = tuple_term.get_tuple_ptr();
  let arity = unsafe { (*old_tuple_p).get_arity() };
  let zero_index = (index - 1) as usize;

  if zero_index >= arity {
    return fail::create::badarg();
  }

  // Create a copy of the tuple with the modified element
  let hp = curr_p.get_heap_mut();
  let new_tuple_p = Tuple::create_into(hp, arity)?;
  unsafe {
    for i in 0..arity {
      if i == zero_index {
        (*new_tuple_p).set_element(i, value);
      } else {
        (*new_tuple_p).set_element(i, (*old_tuple_p).get_element(i));
      }
    }
  }

  Ok(Term::make_boxed(new_tuple_p))
}

// ---- erlang:tuple_size/1 ----
// Get tuple arity.
pub fn nativefun_tuple_size_1(
  _vm: &mut VM,
  _curr_p: &mut Process,
  args: &[Term],
) -> RtResult<Term> {
  assert_arity("erlang:tuple_size", 1, args);
  let tuple_term = args[0];

  if !tuple_term.is_tuple() {
    if tuple_term == Term::empty_tuple() {
      return Ok(Term::make_small_unsigned(0));
    }
    return fail::create::badarg();
  }

  let tuple_p = tuple_term.get_tuple_ptr();
  let arity = unsafe { (*tuple_p).get_arity() };
  Ok(Term::make_small_unsigned(arity))
}

// ---- erlang:tuple_to_list/1 ----
// Convert tuple to list.
pub fn nativefun_tuple_to_list_1(
  _vm: &mut VM,
  curr_p: &mut Process,
  args: &[Term],
) -> RtResult<Term> {
  assert_arity("erlang:tuple_to_list", 1, args);
  let tuple_term = args[0];

  if tuple_term == Term::empty_tuple() {
    return Ok(Term::nil());
  }

  if !tuple_term.is_tuple() {
    return fail::create::badarg();
  }

  let tuple_p = tuple_term.get_tuple_ptr();
  let arity = unsafe { (*tuple_p).get_arity() };

  let hp = curr_p.get_heap_mut();
  let mut lb = ListBuilder::new()?;
  for i in 0..arity {
    let elem = unsafe { (*tuple_p).get_element(i) };
    unsafe { lb.append(elem, hp)? };
  }

  Ok(lb.make_term())
}

// ---- erlang:list_to_tuple/1 ----
// Convert list to tuple.
pub fn nativefun_list_to_tuple_1(
  _vm: &mut VM,
  curr_p: &mut Process,
  args: &[Term],
) -> RtResult<Term> {
  assert_arity("erlang:list_to_tuple", 1, args);
  let list_term = args[0];

  if !list_term.is_list() {
    return fail::create::badarg();
  }

  if list_term == Term::nil() {
    return Ok(Term::empty_tuple());
  }

  let count = cons::list_length(list_term)?;
  let hp = curr_p.get_heap_mut();
  let new_tuple_p = Tuple::create_into(hp, count)?;

  let mut index = 0usize;
  cons::for_each(list_term, |elem| {
    unsafe { (*new_tuple_p).set_element(index, elem) };
    index += 1;
    Ok(())
  })?;

  Ok(Term::make_boxed(new_tuple_p))
}
