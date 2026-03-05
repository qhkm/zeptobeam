use crate::{
  defs::data_reader::TDataReader,
  emulator::{atom, heap::THeapOwner, process::Process, vm::VM},
  fail::{self, RtResult},
  native_fun::assert_arity,
  term::{
    boxed,
    cons,
    term_builder::{BinaryBuilder, ListBuilder},
    Term,
  },
};

// Converts an atom to Erlang string.
define_nativefun!(_vm, proc, args,
  name: "erlang:atom_to_list/1", struct_name: NfErlangA2List2, arity: 1,
  invoke: { atom_to_list_1(proc, atom_val) },
  args: atom(atom_val),
);

#[inline]
pub fn atom_to_list_1(proc: &mut Process, atom_val: Term) -> RtResult<Term> {
  let atom_p = atom::lookup(atom_val);
  if atom_p.is_null() {
    return fail::create::badarg();
  }
  unsafe {
    let s = cons::rust_str_to_list(&(*atom_p).name, proc.get_heap_mut())?;
    Ok(s)
  }
}

// Converts an integer to Erlang string (list of integers)
define_nativefun!(_vm, proc, args,
  name: "erlang:integer_to_list/1", struct_name: NfErlangInt2List2, arity: 1,
  invoke: { integer_to_list_1(proc, val) },
  args: term(val),
);

#[inline]
pub fn integer_to_list_1(curr_p: &mut Process, val: Term) -> RtResult<Term> {
  if !val.is_integer() {
    return fail::create::badarg();
  }
  unsafe { cons::integer_to_list(val, curr_p.get_heap_mut()) }
}

// Returns list `list` reversed with `tail` appended (any term).
define_nativefun!(_vm, proc, args,
  name: "erlang:list_to_binary/1", struct_name: NfErlangL2b1, arity: 1,
  invoke: { unsafe { list_to_binary_1(proc, list) } },
  args: list(list),
);

#[inline]
unsafe fn list_to_binary_1(proc: &mut Process, list: Term) -> RtResult<Term> {
  let size = cons::get_iolist_size(list);
  if size.bytes() == 0 {
    Ok(Term::empty_binary())
  } else {
    let mut bb = BinaryBuilder::with_size(size, proc.get_heap_mut())?;
    list_to_binary_1_recursive(&mut bb, list)?;
    Ok(bb.make_term())
  }
}

unsafe fn list_to_binary_1_recursive(
  bb: &mut BinaryBuilder,
  list: Term,
) -> RtResult<Term> {
  cons::for_each(list, |elem| {
    if elem.is_small() {
      // Any small integer even larger than 256 counts as 1 byte
      bb.write_byte(elem.get_small_unsigned() as u8);
    } else if elem == Term::empty_binary() {
      // <<>> contributes nothing.
    } else if elem.is_binary() {
      let bin_ptr = boxed::Binary::get_trait_from_term(elem);
      let bit_size = (*bin_ptr).get_bit_size();
      if bit_size.get_last_byte_bits() != 0 {
        return fail::create::badarg();
      }
      let n_bytes = bit_size.get_byte_size_rounded_down().bytes();
      if let Some(byte_reader) = (*bin_ptr).get_byte_reader() {
        for i in 0..n_bytes {
          bb.write_byte(byte_reader.read(i));
        }
      } else {
        let bit_reader = (*bin_ptr).get_bit_reader();
        for i in 0..n_bytes {
          bb.write_byte(bit_reader.read(i));
        }
      }
    } else if elem.is_cons() {
      list_to_binary_1_recursive(bb, elem)?;
    } else {
      return fail::create::badarg();
    }
    Ok(())
  })?;
  Ok(list)
}

// Converts a byte-aligned binary to a list of integers [0..255].
define_nativefun!(_vm, proc, args,
  name: "erlang:binary_to_list/1", struct_name: NfErlangB2l1, arity: 1,
  invoke: { unsafe { binary_to_list_1(proc, bin) } },
  args: binary(bin),
);

#[inline]
unsafe fn binary_to_list_1(proc: &mut Process, bin: Term) -> RtResult<Term> {
  if bin == Term::empty_binary() {
    return Ok(Term::nil());
  }

  let bin_ptr = boxed::Binary::get_trait_from_term(bin);
  let bit_size = (*bin_ptr).get_bit_size();
  if bit_size.get_last_byte_bits() != 0 {
    return fail::create::badarg();
  }
  let n_bytes = bit_size.get_byte_size_rounded_down().bytes();
  if n_bytes == 0 {
    return Ok(Term::nil());
  }

  let mut lb = ListBuilder::new()?;
  if let Some(reader) = (*bin_ptr).get_byte_reader() {
    for i in 0..n_bytes {
      lb.append(Term::make_small_unsigned(reader.read(i) as usize), proc.get_heap_mut())?;
    }
  } else {
    let bit_reader = (*bin_ptr).get_bit_reader();
    for i in 0..n_bytes {
      lb.append(Term::make_small_unsigned(bit_reader.read(i) as usize), proc.get_heap_mut())?;
    }
  }
  Ok(lb.make_term())
}

// ---- erlang:atom_to_binary/1 ----
// Convert atom to UTF-8 binary.
pub fn nativefun_atom_to_binary_1(
  _vm: &mut VM,
  curr_p: &mut Process,
  args: &[Term],
) -> RtResult<Term> {
  assert_arity("erlang:atom_to_binary", 1, args);
  let atom_val = args[0];

  if !atom_val.is_atom() {
    return fail::create::badarg();
  }

  let atom_p = atom::lookup(atom_val);
  if atom_p.is_null() {
    return fail::create::badarg();
  }

  let name = unsafe { &(*atom_p).name };
  let bytes = name.as_bytes();

  if bytes.is_empty() {
    return Ok(Term::empty_binary());
  }

  let hp = curr_p.get_heap_mut();
  let bin_ptr = unsafe { boxed::Binary::create_with_data(bytes, hp)? };
  Ok(unsafe { (*bin_ptr).make_term() })
}

// ---- erlang:binary_to_atom/1 ----
// Convert binary to atom (UTF-8).
pub fn nativefun_binary_to_atom_1(
  _vm: &mut VM,
  _curr_p: &mut Process,
  args: &[Term],
) -> RtResult<Term> {
  assert_arity("erlang:binary_to_atom", 1, args);
  let bin = args[0];

  if bin == Term::empty_binary() {
    return Ok(atom::from_str(""));
  }

  if !bin.is_binary() {
    return fail::create::badarg();
  }

  let bin_ptr = unsafe { boxed::Binary::get_trait_from_term(bin) };
  let bit_size = unsafe { (*bin_ptr).get_bit_size() };
  if bit_size.get_last_byte_bits() != 0 {
    return fail::create::badarg();
  }

  let n_bytes = bit_size.get_byte_size_rounded_down().bytes();
  let mut buf = Vec::with_capacity(n_bytes);

  unsafe {
    if let Some(reader) = (*bin_ptr).get_byte_reader() {
      for i in 0..n_bytes {
        buf.push(reader.read(i));
      }
    } else {
      let bit_reader = (*bin_ptr).get_bit_reader();
      for i in 0..n_bytes {
        buf.push(bit_reader.read(i));
      }
    }
  }

  let s = match std::str::from_utf8(&buf) {
    Ok(s) => s,
    Err(_) => return fail::create::badarg(),
  };

  Ok(atom::from_str(s))
}
