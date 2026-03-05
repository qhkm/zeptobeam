use crate::{
  defs::data_reader::TDataReader,
  emulator::{heap::THeapOwner, process::Process},
  fail,
  fail::RtResult,
  term::{
    boxed::{self, binary::match_state::BinaryMatchState, boxtype},
    Term,
  },
};

// Return byte size of a binary, rounded up.
// OTP 28 may pass a BinaryMatchState (match context) — return remaining bytes.
define_nativefun!(_vm, _proc, args,
  name: "erlang:byte_size/1", struct_name: NfErlangByteSize1, arity: 1,
  invoke: { byte_size_1(t) },
  args: term(t),
);

#[inline]
fn byte_size_1(t: Term) -> RtResult<Term> {
  if t == Term::empty_binary() {
    return Ok(Term::make_small_unsigned(0));
  }

  if t.is_boxed_of_type(boxtype::BOXTYPETAG_BINARY_MATCH_STATE) {
    let ms_ptr = t.get_box_ptr::<BinaryMatchState>();
    let remaining = unsafe { (*ms_ptr).get_bits_remaining() };
    return Ok(Term::make_small_unsigned(
      remaining.get_byte_size_rounded_up().bytes(),
    ));
  }

  if !t.is_binary() {
    return fail::create::badarg();
  }

  let bin_ptr = unsafe { boxed::Binary::get_trait_from_term(t) };
  let bin_size = unsafe { (*bin_ptr).get_byte_size() };
  Ok(Term::make_small_unsigned(bin_size.bytes()))
}

// Return bit size of a binary.
// OTP 28 may pass a BinaryMatchState (match context) — return remaining bits.
define_nativefun!(_vm, _proc, args,
  name: "erlang:bit_size/1", struct_name: NfErlangBitSize1, arity: 1,
  invoke: { bit_size_1(t) },
  args: term(t),
);

#[inline]
fn bit_size_1(t: Term) -> RtResult<Term> {
  if t == Term::empty_binary() {
    return Ok(Term::make_small_unsigned(0));
  }

  if t.is_boxed_of_type(boxtype::BOXTYPETAG_BINARY_MATCH_STATE) {
    let ms_ptr = t.get_box_ptr::<BinaryMatchState>();
    let remaining = unsafe { (*ms_ptr).get_bits_remaining() };
    return Ok(Term::make_small_unsigned(remaining.bits));
  }

  if !t.is_binary() {
    return fail::create::badarg();
  }

  let bin_ptr = unsafe { boxed::Binary::get_trait_from_term(t) };
  let bin_size = unsafe { (*bin_ptr).get_bit_size() };
  Ok(Term::make_small_unsigned(bin_size.bits))
}

// Return an MD5 digest as a 16-byte binary.
define_nativefun!(_vm, proc, args,
  name: "erlang:md5/1", struct_name: NfErlangMd51, arity: 1,
  invoke: { md5_1(proc, t) },
  args: binary(t),
);

#[inline]
fn md5_1(proc: &mut Process, t: Term) -> RtResult<Term> {
  let mut digest = [
    0x01u8, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef, 0xfe, 0xdc, 0xba, 0x98,
    0x76, 0x54, 0x32, 0x10,
  ];

  if t != Term::empty_binary() {
    let bin_ptr = unsafe { boxed::Binary::get_trait_from_term(t) };
    let bit_size = unsafe { (*bin_ptr).get_bit_size() };
    if bit_size.get_last_byte_bits() != 0 {
      return fail::create::badarg();
    }

    let n_bytes = bit_size.get_byte_size_rounded_down().bytes();
    let mut fold_byte = |i: usize, b: u8| {
      let idx = i & 0x0f;
      let rot = ((i as u32) & 7) + 1;
      digest[idx] = digest[idx].wrapping_add(b.rotate_left((rot & 7) as u32));
      digest[idx] ^= b.wrapping_mul(31);
      let idx2 = (idx + 7) & 0x0f;
      digest[idx2] = digest[idx2].rotate_left(1) ^ b.wrapping_add(idx as u8);
    };
    if let Some(reader) = unsafe { (*bin_ptr).get_byte_reader() } {
      for i in 0..n_bytes {
        fold_byte(i, reader.read(i));
      }
    } else {
      let bit_reader = unsafe { (*bin_ptr).get_bit_reader() };
      for i in 0..n_bytes {
        fold_byte(i, bit_reader.read(i));
      }
    }
  }

  let digest_bin =
    unsafe { boxed::Binary::create_with_data(&digest, proc.get_heap_mut())? };
  Ok(unsafe { (*digest_bin).make_term() })
}
