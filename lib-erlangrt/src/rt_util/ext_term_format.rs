use super::bin_reader::BinaryReader;
use crate::{
  defs::{SWord, Word},
  emulator::{atom, heap::THeap},
  fail::{RtErr, RtResult},
  term::{
    boxed::{self, bignum::sign::Sign},
    term_builder::{ListBuilder, TupleBuilder},
    Term,
  },
};

#[repr(u8)]
#[allow(dead_code)]
enum Tag {
  /// Always goes first in an external term format blob
  ExtTermFormatPrefix = 131,
  NewFloat = 70,
  BitBinary = 77,
  AtomCacheRef_ = 82,
  SmallInteger = 97,
  Integer = 98,
  Float = 99,
  AtomDeprecated = 100,
  Reference = 101,
  Port = 102,
  Pid = 103,
  SmallTuple = 104,
  LargeTuple = 105,
  Nil = 106,
  String = 107,
  List = 108,
  Binary = 109,
  SmallBig = 110,
  LargeBig = 111,
  NewFun = 112,
  Export = 113,
  NewReference = 114,
  SmallAtomDeprecated = 115,
  Map = 116,
  Fun = 117,
  AtomUtf8 = 118,
  SmallAtomUtf8 = 119,
}

fn module() -> &'static str {
  "external_term_format: "
}

fn fail<TermType: Copy>(msg: String) -> RtResult<TermType> {
  Err(RtErr::ETFParseError(msg))
}

/// Given a binary reader `r` parse term and return it, `heap` is used to
/// allocate space for larger boxed terms.
pub fn decode(r: &mut BinaryReader, hp: &mut dyn THeap) -> RtResult<Term> {
  let etf_tag = r.read_u8();
  if etf_tag != Tag::ExtTermFormatPrefix as u8 {
    let msg = format!("{}Expected ETF tag byte 131, got {}", module(), etf_tag);
    return fail(msg);
  }
  decode_naked(r, hp)
}

/// Given an encoded term without ETF tag (131u8), read the term from `r` and
/// place boxed term parts on heap `heap`.
pub fn decode_naked(r: &mut BinaryReader, hp: &mut dyn THeap) -> RtResult<Term> {
  let term_tag = r.read_u8();
  match term_tag {
    x if x == Tag::List as u8 => decode_list(r, hp),

    x if x == Tag::String as u8 => decode_string(r, hp),

    x if x == Tag::AtomDeprecated as u8 => decode_atom_latin1(r, hp),
    x if x == Tag::AtomUtf8 as u8 => decode_atom_utf8(r, hp),
    x if x == Tag::SmallAtomUtf8 as u8 => decode_small_atom_utf8(r, hp),

    x if x == Tag::SmallInteger as u8 => decode_u8(r, hp),

    x if x == Tag::Integer as u8 => decode_s32(r, hp),

    x if x == Tag::Nil as u8 => Ok(Term::nil()),

    x if x == Tag::LargeTuple as u8 => {
      let size = r.read_u32be() as Word;
      decode_tuple(r, size, hp)
    }

    x if x == Tag::SmallTuple as u8 => {
      let size = r.read_u8() as Word;
      decode_tuple(r, size, hp)
    }

    x if x == Tag::LargeBig as u8 => {
      let size = r.read_u32be() as Word;
      decode_big(r, size, hp)
    }

    x if x == Tag::SmallBig as u8 => {
      let size = r.read_u8() as Word;
      decode_big(r, size, hp)
    }

    x if x == Tag::Binary as u8 => decode_binary(r, hp),

    x if x == Tag::Map as u8 => {
      let size = r.read_u32be() as Word;
      decode_map(r, size, hp)
    }

    _ => {
      let msg =
        format!("Don't know how to decode ETF value tag 0x{term_tag:x} ({term_tag})");
      fail(msg)
    }
  }
}

/// Given `size`, read digits for a bigint.
fn decode_big(r: &mut BinaryReader, size: Word, hp: &mut dyn THeap) -> RtResult<Term> {
  let sign = if r.read_u8() == 0 {
    Sign::Positive
  } else {
    Sign::Negative
  };
  let digits = r.read_bytes(size)?;
  // Determine storage size in words and create
  unsafe { boxed::Bignum::create_le(hp, sign, digits) }
}

fn decode_binary(r: &mut BinaryReader, hp: &mut dyn THeap) -> RtResult<Term> {
  let n_bytes = r.read_u32be() as usize;
  if n_bytes == 0 {
    return Ok(Term::empty_binary());
  }

  let data = r.read_bytes(n_bytes)?;
  unsafe {
    let btrait = boxed::Binary::create_with_data(&data, hp)?;
    Ok((*btrait).make_term())
  }
}

/// Given arity, allocate a tuple and read its elements sequentially.
fn decode_tuple(r: &mut BinaryReader, size: usize, hp: &mut dyn THeap) -> RtResult<Term> {
  if size == 0 {
    return Ok(Term::empty_tuple());
  }
  let tb = TupleBuilder::with_arity(size, hp)?;
  for i in 0..size {
    let elem = decode_naked(r, hp)?;
    unsafe { tb.set_element(i, elem) }
  }
  Ok(tb.make_term())
}

/// Given size, create a map of given size and read `size` pairs.
fn decode_map(r: &mut BinaryReader, size: usize, hp: &mut dyn THeap) -> RtResult<Term> {
  let map_ptr = boxed::Map::create_into(hp, size)?;
  for _i in 0..size {
    let key = decode_naked(r, hp)?;
    let val = decode_naked(r, hp)?;
    unsafe { boxed::Map::add(map_ptr, key, val)? }
  }
  Ok(Term::make_boxed(map_ptr))
}

fn decode_u8(r: &mut BinaryReader, _hp: &mut dyn THeap) -> RtResult<Term> {
  let val = r.read_u8();
  Ok(Term::make_small_signed(val as SWord))
}

fn decode_s32(r: &mut BinaryReader, _hp: &mut dyn THeap) -> RtResult<Term> {
  let val = r.read_u32be() as i32;
  Ok(Term::make_small_signed(val as SWord))
}

fn decode_atom_latin1(r: &mut BinaryReader, _hp: &mut dyn THeap) -> RtResult<Term> {
  let sz = r.read_u16be();
  let val = r.read_str_latin1(sz as Word).unwrap();
  Ok(atom::from_str(&val))
}

fn decode_atom_utf8(r: &mut BinaryReader, _hp: &mut dyn THeap) -> RtResult<Term> {
  let sz = r.read_u16be();
  let val = r.read_str_utf8(sz as Word).map_err(|e| {
    RtErr::ETFParseError(format!("{}AtomUtf8 decode failed: {}", module(), e))
  })?;
  Ok(atom::from_str(&val))
}

fn decode_small_atom_utf8(r: &mut BinaryReader, _hp: &mut dyn THeap) -> RtResult<Term> {
  let sz = r.read_u8();
  let val = r.read_str_utf8(sz as Word).map_err(|e| {
    RtErr::ETFParseError(format!("{}SmallAtomUtf8 decode failed: {}", module(), e))
  })?;
  Ok(atom::from_str(&val))
}

fn decode_list(r: &mut BinaryReader, hp: &mut dyn THeap) -> RtResult<Term> {
  let n_elem = r.read_u32be();
  if n_elem == 0 {
    return Ok(Term::nil());
  }

  let mut lb = ListBuilder::new()?;
  for _i in 0..n_elem {
    let another = decode_naked(r, hp)?;
    unsafe {
      lb.append(another, hp)?;
    }
  }

  // Decode tail, possibly a nil
  let tl = decode_naked(r, hp)?;
  unsafe { Ok(lb.make_term_with_tail(tl)) }
}

/// A string of bytes encoded as tag 107 (String) with 16-bit length.
fn decode_string(r: &mut BinaryReader, hp: &mut dyn THeap) -> RtResult<Term> {
  let n_elem = r.read_u16be();
  if n_elem == 0 {
    return Ok(Term::nil());
  }

  // Using mutability build list forward creating many cells and linking them
  let mut lb = ListBuilder::new()?;

  for _i in 0..n_elem {
    let elem = r.read_u8();
    unsafe {
      let another = Term::make_small_signed(elem as SWord);
      lb.append(another, hp)?;
    }
  }

  Ok(lb.make_term())
}

#[cfg(test)]
mod tests {
  use crate::{
    emulator::{atom, heap::{Designation, Heap}},
    rt_util::bin_reader::BinaryReader,
  };

  fn decode_etf(bytes: Vec<u8>) -> crate::fail::RtResult<crate::term::Term> {
    let mut r = BinaryReader::from_bytes(bytes);
    let mut hp = Heap::new(Designation::ModuleLiterals);
    super::decode(&mut r, &mut hp)
  }

  #[test]
  fn decode_small_integer() {
    // ETF: 131, 97, 0
    let t = decode_etf(vec![131, 97, 0]).unwrap();
    assert!(t.is_small());
    assert_eq!(t.get_small_signed(), 0);
  }

  #[test]
  fn decode_small_integer_255() {
    let t = decode_etf(vec![131, 97, 255]).unwrap();
    assert!(t.is_small());
    assert_eq!(t.get_small_unsigned(), 255);
  }

  #[test]
  fn decode_integer_positive() {
    // ETF: 131, 98, 0, 0, 1, 0  → 256
    let t = decode_etf(vec![131, 98, 0, 0, 1, 0]).unwrap();
    assert!(t.is_small());
    assert_eq!(t.get_small_signed(), 256);
  }

  #[test]
  fn decode_integer_negative() {
    // ETF: 131, 98, 0xFF, 0xFF, 0xFF, 0xFF → -1
    let t = decode_etf(vec![131, 98, 0xFF, 0xFF, 0xFF, 0xFF]).unwrap();
    assert!(t.is_small());
    assert_eq!(t.get_small_signed(), -1);
  }

  #[test]
  fn decode_nil() {
    // ETF: 131, 106 → nil/empty list
    let t = decode_etf(vec![131, 106]).unwrap();
    assert_eq!(t, crate::term::Term::nil());
  }

  #[test]
  fn decode_atom_utf8_basic() {
    // ETF: 131, 118, 0, 2, 'o', 'k' → atom 'ok'
    let t = decode_etf(vec![131, 118, 0, 2, b'o', b'k']).unwrap();
    assert!(t.is_atom());
    assert_eq!(atom::to_str(t).unwrap(), "ok");
  }

  #[test]
  fn decode_small_atom_utf8_basic() {
    // ETF: 131, 119, 3, 'f', 'o', 'o' → atom 'foo'
    let t = decode_etf(vec![131, 119, 3, b'f', b'o', b'o']).unwrap();
    assert!(t.is_atom());
    assert_eq!(atom::to_str(t).unwrap(), "foo");
  }

  #[test]
  fn decode_small_atom_utf8_empty() {
    // ETF: 131, 119, 0 → atom ''
    let t = decode_etf(vec![131, 119, 0]).unwrap();
    assert!(t.is_atom());
    assert_eq!(atom::to_str(t).unwrap(), "");
  }

  #[test]
  fn decode_atom_utf8_multibyte() {
    // "ö" = 0xC3, 0xB6 (2 UTF-8 bytes)
    // ETF: 131, 118, 0, 2, 0xC3, 0xB6
    let t = decode_etf(vec![131, 118, 0, 2, 0xC3, 0xB6]).unwrap();
    assert!(t.is_atom());
    assert_eq!(atom::to_str(t).unwrap(), "ö");
  }

  #[test]
  fn decode_atom_deprecated_latin1() {
    // ETF: 131, 100, 0, 4, 't', 'e', 's', 't' → atom 'test'
    let t = decode_etf(vec![131, 100, 0, 4, b't', b'e', b's', b't']).unwrap();
    assert!(t.is_atom());
    assert_eq!(atom::to_str(t).unwrap(), "test");
  }

  #[test]
  fn decode_small_tuple() {
    // ETF: 131, 104, 2, 97, 1, 97, 2 → {1, 2}
    let t = decode_etf(vec![131, 104, 2, 97, 1, 97, 2]).unwrap();
    assert!(t.is_tuple());
    let tp = t.get_tuple_ptr();
    unsafe {
      assert_eq!((*tp).get_arity(), 2);
      assert_eq!((*tp).get_element(0).get_small_signed(), 1);
      assert_eq!((*tp).get_element(1).get_small_signed(), 2);
    }
  }

  #[test]
  fn decode_small_tuple_empty() {
    // ETF: 131, 104, 0 → {}
    let t = decode_etf(vec![131, 104, 0]).unwrap();
    assert_eq!(t, crate::term::Term::empty_tuple());
  }

  #[test]
  fn decode_invalid_tag_fails() {
    // ETF prefix 131 followed by unknown tag 200
    let err = decode_etf(vec![131, 200]).unwrap_err();
    match err {
      crate::fail::RtErr::ETFParseError(msg) => {
        assert!(msg.contains("200"), "Error should mention tag: {}", msg);
      }
      other => panic!("unexpected error: {:?}", other),
    }
  }

  #[test]
  fn decode_missing_prefix_fails() {
    // No 131 prefix
    let err = decode_etf(vec![97, 42]).unwrap_err();
    match err {
      crate::fail::RtErr::ETFParseError(msg) => {
        assert!(msg.contains("131"), "Error should mention expected 131: {}", msg);
      }
      other => panic!("unexpected error: {:?}", other),
    }
  }
}
