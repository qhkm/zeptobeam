//! Implements misc and general purpose list operations.
use crate::{
  emulator::{heap::THeapOwner, process::Process},
  fail::{self, RtResult},
  term::{compare, cons, term_builder::ListBuilder, *},
};
use core::cmp::Ordering;

define_nativefun!(_vm, _proc, args,
  name: "lists:member/2", struct_name: NfListsMember2, arity: 2,
  invoke: { member_2(sample, list) },
  args: term(sample), list(list),
);

#[inline]
fn member_2(sample: Term, list: Term) -> RtResult<Term> {
  if list == Term::nil() {
    return Ok(Term::make_bool(false));
  }
  let result = cons::any(list, |elem| {
    if let Ok(cmp_result) = compare::cmp_terms(sample, elem, true) {
      cmp_result == Ordering::Equal
    } else {
      false
    }
  });
  Ok(Term::make_bool(result))
}

// Returns list `list` reversed with `tail` appended (any term).
define_nativefun!(_vm, proc, args,
  name: "lists:reverse/2", struct_name: NfListsReverse2, arity: 2,
  invoke: { unsafe { reverse_2(proc, list, tail) } },
  args: list(list), term(tail),
);

#[inline]
unsafe fn reverse_2(proc: &mut Process, list: Term, tail: Term) -> RtResult<Term> {
  if list == Term::nil() {
    return Ok(tail);
  }

  let mut lb = ListBuilder::new()?;
  let hp = proc.get_heap_mut();

  // Going forward the list, prepend values to the result
  cons::for_each(list, |elem| lb.prepend(elem, hp))?;

  // Last element's tail in the new list is set to `tail` argument
  Ok(lb.make_term_with_tail(tail))
}

// ---- lists:sort/1 ----
// Sort a list using Erlang term ordering.
define_nativefun!(_vm, proc, args,
  name: "lists:sort/1", struct_name: NfListsSort1, arity: 1,
  invoke: { sort_1(proc, list) },
  args: list(list),
);

#[inline]
fn sort_1(proc: &mut Process, list: Term) -> RtResult<Term> {
  if list == Term::nil() {
    return Ok(Term::nil());
  }

  // Collect all elements into a Vec
  let mut elements = Vec::new();
  cons::for_each(list, |elem| {
    elements.push(elem);
    Ok(())
  })?;

  if elements.len() <= 1 {
    return Ok(list);
  }

  // Sort using Erlang term comparison
  // We need to handle comparison errors, so use a flag
  let mut sort_error = false;
  elements.sort_by(|a, b| {
    if sort_error {
      return Ordering::Equal;
    }
    match compare::cmp_terms(*a, *b, true) {
      Ok(ord) => ord,
      Err(_) => {
        sort_error = true;
        Ordering::Equal
      }
    }
  });

  if sort_error {
    return fail::create::badarg();
  }

  // Build result list from sorted elements
  let hp = proc.get_heap_mut();
  let mut lb = ListBuilder::new()?;
  for elem in &elements {
    unsafe { lb.append(*elem, hp)? };
  }
  Ok(lb.make_term())
}

// ---- lists:append/2 ----
// Append two lists (same as ++).
define_nativefun!(_vm, proc, args,
  name: "lists:append/2", struct_name: NfListsAppend2, arity: 2,
  invoke: { append_2(proc, a, b) },
  args: list(a), term(b),
);

#[inline]
fn append_2(proc: &mut Process, a: Term, b: Term) -> RtResult<Term> {
  // Doing [] ++ X -> X
  if a == Term::nil() {
    return Ok(b);
  }

  // Copy the list a and append b as tail
  let hp = proc.get_heap_mut();
  let (l1, tail) = unsafe { cons::copy_list_leave_tail(a, hp) }?;
  unsafe {
    (*tail).set_tl(b);
  }
  Ok(l1)
}

// ---- lists:flatten/1 ----
// Flatten nested lists.
define_nativefun!(_vm, proc, args,
  name: "lists:flatten/1", struct_name: NfListsFlatten1, arity: 1,
  invoke: { flatten_1(proc, list) },
  args: list(list),
);

#[inline]
fn flatten_1(proc: &mut Process, list: Term) -> RtResult<Term> {
  if list == Term::nil() {
    return Ok(Term::nil());
  }

  let hp = proc.get_heap_mut();
  let mut lb = ListBuilder::new()?;
  flatten_into(&mut lb, list, hp)?;

  if lb.head_p.is_null() {
    Ok(Term::nil())
  } else {
    Ok(lb.make_term())
  }
}

fn flatten_into(
  lb: &mut ListBuilder,
  list: Term,
  hp: &mut dyn crate::emulator::heap::THeap,
) -> RtResult<()> {
  if list == Term::nil() {
    return Ok(());
  }
  if !list.is_cons() {
    // Non-list element: just append it
    unsafe { lb.append(list, hp)? };
    return Ok(());
  }

  let mut current = list;
  while current.is_cons() {
    let cons_p = current.get_cons_ptr();
    let hd = unsafe { (*cons_p).hd() };
    current = unsafe { (*cons_p).tl() };

    if hd.is_list() {
      // Recursively flatten nested lists
      flatten_into(lb, hd, hp)?;
    } else {
      unsafe { lb.append(hd, hp)? };
    }
  }
  // Handle improper list tail (not nil)
  if current != Term::nil() {
    unsafe { lb.append(current, hp)? };
  }
  Ok(())
}

// ---- lists:nth/2 ----
// Get Nth element (1-based).
define_nativefun!(_vm, _proc, args,
  name: "lists:nth/2", struct_name: NfListsNth2, arity: 2,
  invoke: { nth_2(n, list) },
  args: usize(n), non_empty_list(list),
);

#[inline]
fn nth_2(n: usize, list: Term) -> RtResult<Term> {
  if n == 0 {
    return fail::create::badarg();
  }

  let mut current = list;
  let mut count = 1;
  while current.is_cons() {
    let cons_p = current.get_cons_ptr();
    let hd = unsafe { (*cons_p).hd() };
    if count == n {
      return Ok(hd);
    }
    current = unsafe { (*cons_p).tl() };
    count += 1;
  }

  // If we got here, n was larger than list length
  fail::create::badarg()
}

// ---- lists:last/1 ----
// Get last element.
define_nativefun!(_vm, _proc, args,
  name: "lists:last/1", struct_name: NfListsLast1, arity: 1,
  invoke: { last_1(list) },
  args: non_empty_list(list),
);

#[inline]
fn last_1(list: Term) -> RtResult<Term> {
  let mut current = list;
  let mut last_elem = Term::nil();

  while current.is_cons() {
    let cons_p = current.get_cons_ptr();
    last_elem = unsafe { (*cons_p).hd() };
    current = unsafe { (*cons_p).tl() };
  }

  Ok(last_elem)
}
