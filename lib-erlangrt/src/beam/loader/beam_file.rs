use std::{
  io::{Cursor, Read},
  path::PathBuf,
};

use bytes::Bytes;
use compress::zlib;

use crate::{
  beam::{
    gen_op,
    loader::{
      compact_term::CompactTermReader,
      load_time_structs::{LtExport, LtFun, LtImport},
    },
  },
  defs,
  emulator::heap::{Designation, Heap},
  fail::{RtErr, RtResult},
  rt_util::{
    bin_reader::{BinaryReader, ReadError},
    ext_term_format as etf,
  },
  term::Term,
};

fn module() -> &'static str {
  "beam/file: "
}

pub struct BeamFile {
  /// Raw atoms loaded from BEAM module as strings
  pub atoms: Vec<String>,
  pub imports: Vec<LtImport>,
  pub exports: Vec<LtExport>,
  pub locals: Vec<LtExport>,
  pub lambdas: Vec<LtFun>,
  /// Temporary storage for loaded code, will be parsed in stage 2
  pub code: Vec<u8>,

  /// Literal table decoded into friendly terms (does not use process heap).
  pub lit_tab: Vec<Term>,

  /// A place to allocate larger lterms (literal heap)
  pub lit_heap: Heap,

  /// Proplist of module attributes as loaded from "Attr" section
  mod_attrs: Term,

  /// Compiler flags as loaded from "Attr" section
  compiler_info: Term,
}

impl BeamFile {
  fn new() -> Self {
    Self {
      atoms: Vec::new(),
      imports: Vec::new(),
      exports: Vec::new(),
      locals: Vec::new(),
      lambdas: Vec::new(),
      code: Vec::new(),

      lit_tab: Vec::new(),
      lit_heap: Heap::new(Designation::ModuleLiterals),
      mod_attrs: Term::nil(),
      compiler_info: Term::nil(),
    }
  }

  /// Loading the module. Validate the header and iterate over sections,
  /// then call `load_stage2()` to apply changes to the VM, and then finalize
  /// it by calling `load_finalize()` which will return you a module object.
  pub fn read_chunks(fname: &PathBuf) -> RtResult<BeamFile> {
    let mut beam_file = Self::new();

    // Prebuffered BEAM file should be released as soon as the initial phase
    // is done.
    let mut r = BinaryReader::from_file(fname);

    // Parse header and check file FOR1 signature
    let hdr1 = Bytes::from(&b"FOR1"[..]);
    r.ensure_bytes(&hdr1)?;

    let _beam_sz = r.read_u32be();

    // Check BEAM signature
    let hdr2 = Bytes::from(&b"BEAM"[..]);
    r.ensure_bytes(&hdr2)?;

    loop {
      // EOF may strike here when we finished reading
      let chunk_h = match r.read_str_latin1(4) {
        Ok(s) => s,
        // EOF is not an error
        Err(ReadError::PrematureEOF) => break,
        Err(e) => return Err(RtErr::ReadError(e)),
      };
      let chunk_sz = r.read_u32be();
      let pos_begin = r.pos();

      // println!("Chunk {}", chunk_h);
      match chunk_h.as_ref() {
        "Atom" => beam_file.load_atoms_latin1(&mut r)?,
        // Attr/CInf are not used by runtime execution paths yet.
        // Skip decoding for broad OTP compatibility.
        "Attr" => r.skip(chunk_sz as usize),
        "AtU8" => beam_file.load_atoms_utf8(&mut r)?,
        "CInf" => r.skip(chunk_sz as usize),
        "Code" => beam_file.load_code(&mut r, chunk_sz as defs::Word)?,
        "ExpT" => beam_file.exports = beam_file.load_exports(&mut r),
        "FunT" => beam_file.load_fun_table(&mut r),
        "ImpT" => beam_file.load_imports(&mut r),
        "Line" => beam_file.load_line_info(&mut r)?,
        "LitT" => beam_file.load_literals(&mut r, chunk_sz as defs::Word),
        // LocT same format as ExpT, but for local functions
        "LocT" => beam_file.locals = beam_file.load_exports(&mut r),

        "Dbgi" | // skip debug info
        "Docs" | // skip documentation (OTP 27+)
        "Meta" | // skip metadata (OTP 25+)
        "Type" | // skip type info (OTP 25+)
        "StrT" | // skip strings TODO load strings?
        "Abst" => r.skip(chunk_sz as usize), // skip abstract code

        other => {
          let msg = format!("{}Unexpected chunk: {}", module(), other);
          return Err(RtErr::CodeLoadingFailed(msg));
        }
      }

      // The next chunk is aligned at 4 bytes
      let aligned_sz = 4 * ((chunk_sz + 3) / 4);
      r.seek(pos_begin + aligned_sz as usize);
    }

    Ok(beam_file)
  }

  /// Approaching AtU8 section, populate atoms table in the Loader state.
  /// OTP 22 format: u32/big count { u8 length, "atomname" }
  /// OTP 25+ format: i32/big negative_count { compact_length, "atomname" }
  ///   A negative count signals the new compact-encoded atom lengths.
  fn load_atoms_utf8(&mut self, r: &mut BinaryReader) -> RtResult<()> {
    let raw_count = r.read_u32be() as i32;
    let (n_atoms, use_compact) = if raw_count < 0 {
      ((-raw_count) as u32, true)
    } else {
      (raw_count as u32, false)
    };

    self.atoms.reserve(n_atoms as usize);
    for i in 0..n_atoms {
      let atom_len = if use_compact {
        Self::read_compact_atom_length(r)?
      } else {
        r.read_u8() as defs::Word
      };
      let atom_text = r
        .read_str_utf8(atom_len)
        .map_err(|err| {
          let msg = format!(
            "{}AtU8 atom parse failed at index {}: {}",
            module(),
            i,
            err
          );
          RtErr::CodeLoadingFailed(msg)
        })?;
      self.atoms.push(atom_text);
    }
    Ok(())
  }

  /// Read a compact-encoded atom name length (used in OTP 25+ BEAM files).
  /// Uses the BEAM compact term encoding for tag 0 (literal):
  ///   - bit3=0: value = byte >> 4 (0-15, single byte)
  ///   - bit3=1, bit4=0: value = (byte >> 5) << 8 | next_byte (two bytes)
  fn read_compact_atom_length(r: &mut BinaryReader) -> RtResult<defs::Word> {
    let b = r.read_u8();
    if b & 0x08 == 0 {
      // Single byte: length in upper 4 bits
      Ok((b >> 4) as defs::Word)
    } else if b & 0x10 == 0 {
      // Two bytes: upper bits + next byte
      let hi = (b >> 5) as defs::Word;
      let lo = r.read_u8() as defs::Word;
      Ok((hi << 8) | lo)
    } else {
      let msg = format!("{}Unsupported compact atom length encoding: 0x{:02x}", module(), b);
      Err(RtErr::CodeLoadingFailed(msg))
    }
  }

  /// Approaching Atom section, populate atoms table in the Loader state.
  /// The format is: "Atom"|"AtU8", u32/big count { u8 length, "atomname" }.
  /// Same as `load_atoms_utf8` but interprets strings per-character as latin-1
  fn load_atoms_latin1(&mut self, r: &mut BinaryReader) -> RtResult<()> {
    let n_atoms = r.read_u32be();
    self.atoms.reserve(n_atoms as usize);
    for i in 0..n_atoms {
      let atom_bytes = r.read_u8();
      let atom_text = r.read_str_latin1(atom_bytes as defs::Word).map_err(|err| {
        let msg = format!(
          "{}Atom section parse failed at index {}: {}",
          module(),
          i,
          err
        );
        RtErr::CodeLoadingFailed(msg)
      })?;
      self.atoms.push(atom_text);
    }
    Ok(())
  }

  /// Read Attr section: two terms (module attributes and compiler info) encoded
  /// as external term format.
  fn load_attributes(&mut self, r: &mut BinaryReader) -> RtResult<()> {
    self.mod_attrs = etf::decode(r, &mut self.lit_heap)?;
    Ok(())
  }

  fn load_compiler_info(&mut self, r: &mut BinaryReader) -> RtResult<()> {
    self.compiler_info = etf::decode(r, &mut self.lit_heap)?;
    Ok(())
  }

  /// Load the `Code` section
  fn load_code(&mut self, r: &mut BinaryReader, chunk_sz: defs::Word) -> RtResult<()> {
    let _code_ver = r.read_u32be();
    let _min_opcode = r.read_u32be();
    let max_opcode = r.read_u32be();
    let _n_labels = r.read_u32be();
    let _n_funs = r.read_u32be();
    // println!("Code section version {}, opcodes {}-{}, labels: {}, funs: {}",
    //  code_ver, min_opcode, max_opcode, n_labels, n_funs);

    if max_opcode > gen_op::OPCODE_MAX.get() as u32 {
      let msg = "BEAM file comes from a newer and unsupported OTP version".to_string();
      return Err(RtErr::CodeLoadingFailed(msg));
    }

    self.code = r.read_bytes(chunk_sz - 20).unwrap();
    Ok(())
  }

  /// Read the imports table.
  /// Format is u32/big count { modindex: u32, funindex: u32, arity: u32 }
  fn load_imports(&mut self, r: &mut BinaryReader) {
    let n_imports = r.read_u32be();
    self.imports.reserve(n_imports as usize);
    for _i in 0..n_imports {
      let imp = LtImport {
        mod_atom_i: r.read_u32be() as usize,
        fun_atom_i: r.read_u32be() as usize,
        arity: r.read_u32be() as defs::Arity,
      };
      self.imports.push(imp);
    }
  }

  /// Read the exports or local functions table (same format).
  /// Format is u32/big count { funindex: u32, arity: u32, label: u32 }
  fn load_exports(&mut self, r: &mut BinaryReader) -> Vec<LtExport> {
    let n_exports = r.read_u32be();
    let mut exports = Vec::new();
    exports.reserve(n_exports as usize);
    for _i in 0..n_exports {
      let exp = LtExport {
        fun_atom_i: r.read_u32be() as usize,
        arity: r.read_u32be() as defs::Arity,
        label: r.read_u32be() as usize,
      };
      exports.push(exp);
    }
    exports
  }

  fn load_fun_table(&mut self, r: &mut BinaryReader) {
    let n_funs = r.read_u32be();
    self.lambdas.reserve(n_funs as usize);
    for _i in 0..n_funs {
      let fun_atom = r.read_u32be() as usize;
      let arity = r.read_u32be() as usize;
      let code_pos = r.read_u32be() as usize;
      let index = r.read_u32be() as usize;
      let nfrozen = r.read_u32be() as usize;
      let ouniq = r.read_u32be() as usize;
      self.lambdas.push(LtFun {
        fun_atom_i: fun_atom,
        arity: arity as defs::Arity,
        code_pos,
        index,
        nfrozen,
        ouniq,
      })
    }
  }

  fn load_line_info(&mut self, reader: &mut BinaryReader) -> RtResult<()> {
    let _version = reader.read_u32be(); // must match emulator version 0
    let _flags = reader.read_u32be();
    let _n_line_instr = reader.read_u32be();
    let n_line_refs = reader.read_u32be();
    let n_filenames = reader.read_u32be();
    let mut _fname_index = 0u32;

    let mut ct_reader = CompactTermReader::new(&mut self.lit_heap);
    for _i in 0..n_line_refs {
      let val = ct_reader.read(reader)?;

      #[allow(clippy::if_same_then_else)]
      if val.is_small() {
        // self.linerefs.push((_fname_index, w));
      } else if val.is_atom() || val.is_loadtime() {
        // _fname_index = a as u32 (loadtime atoms from OTP 28 compact term encoding)
      } else {
        panic!("{}Unexpected data in line info section: {}", module(), val)
      }
    }

    for _i in 0..n_filenames {
      let name_size = reader.read_u16be();
      let _fstr = reader.read_str_utf8(name_size as defs::Word);
    }
    Ok(())
  }

  /// Given the `r`, reader positioned on the contents of "LitT" chunk,
  /// decompress it and feed into `self.decode_literals/1`.
  /// OTP 25+: when uncomp_sz == 0 the data is stored uncompressed.
  fn load_literals(&mut self, r: &mut BinaryReader, chunk_sz: defs::Word) {
    let uncomp_sz = r.read_u32be();
    let raw_data = r.read_bytes(chunk_sz - 4).unwrap();

    let literal_data = if uncomp_sz == 0 {
      // OTP 25+: literals are stored uncompressed
      raw_data
    } else {
      // OTP 22 and earlier: zlib-compressed literals
      let mut inflated = Vec::<u8>::new();
      inflated.reserve(uncomp_sz as usize);
      let iocursor = Cursor::new(&raw_data);
      zlib::Decoder::new(iocursor)
        .read_to_end(&mut inflated)
        .unwrap();
      assert_eq!(
        inflated.len(),
        uncomp_sz as usize,
        "{}LitT inflate failed",
        module()
      );
      inflated
    };

    self.decode_literals(literal_data);
  }

  /// Given `inflated`, the byte contents of literal table, read the u32/big
  /// `count` and for every encoded term skip u32 and parse the external term
  /// format. Boxed values will go into the `self.lit_heap`.
  fn decode_literals(&mut self, inflated: Vec<u8>) {
    // dump_vec(&inflated);

    // Decode literals into literal heap here
    let mut r = BinaryReader::from_bytes(inflated);
    let count = r.read_u32be();
    self.lit_tab.reserve(count as usize);

    for _i in 0..count {
      // size should match actual consumed ETF bytes so can skip it here
      let _size = r.read_u32be();

      // TODO: Instead of unwrap return error and possibly log/return error location too?
      let literal = etf::decode(&mut r, &mut self.lit_heap).unwrap();

      self.lit_tab.push(literal);
    }
  }
}

#[cfg(test)]
mod tests {
  use crate::{
    beam::loader::beam_file::BeamFile, fail::RtErr, rt_util::bin_reader::BinaryReader,
  };

  #[test]
  fn load_atoms_utf8_returns_error_instead_of_panicking_on_invalid_utf8() {
    let mut r = BinaryReader::from_bytes(vec![
      0, 0, 0, 1,    // one atom
      1,    // length
      0xff, // invalid UTF-8 byte
    ]);
    let mut bf = BeamFile::new();

    let err = bf.load_atoms_utf8(&mut r).unwrap_err();
    match err {
      RtErr::CodeLoadingFailed(msg) => {
        assert!(msg.contains("AtU8 atom parse failed"));
      }
      other => panic!("unexpected error: {:?}", other),
    }
  }

  // --- OTP 28 compact atom length tests ---

  #[test]
  fn read_compact_atom_length_single_byte_small() {
    // bit3=0 → value = byte >> 4. E.g. byte=0x50 → length=5
    let mut r = BinaryReader::from_bytes(vec![0x50]);
    let len = BeamFile::read_compact_atom_length(&mut r).unwrap();
    assert_eq!(len, 5);
  }

  #[test]
  fn read_compact_atom_length_single_byte_zero() {
    // byte=0x00 → length=0
    let mut r = BinaryReader::from_bytes(vec![0x00]);
    let len = BeamFile::read_compact_atom_length(&mut r).unwrap();
    assert_eq!(len, 0);
  }

  #[test]
  fn read_compact_atom_length_single_byte_max_15() {
    // byte=0xF0 → length=15
    let mut r = BinaryReader::from_bytes(vec![0xF0]);
    let len = BeamFile::read_compact_atom_length(&mut r).unwrap();
    assert_eq!(len, 15);
  }

  #[test]
  fn read_compact_atom_length_two_byte_encoding() {
    // bit3=1, bit4=0 → hi = byte >> 5, lo = next_byte
    // byte=0x08 (bits: 0000_1000), hi=0, next=42 → length=42
    let mut r = BinaryReader::from_bytes(vec![0x08, 42]);
    let len = BeamFile::read_compact_atom_length(&mut r).unwrap();
    assert_eq!(len, 42);
  }

  #[test]
  fn read_compact_atom_length_two_byte_large() {
    // byte=0x68 (bits: 0110_1000), hi=3, next=0xFF → length=3*256+255=1023
    let mut r = BinaryReader::from_bytes(vec![0x68, 0xFF]);
    let len = BeamFile::read_compact_atom_length(&mut r).unwrap();
    assert_eq!(len, 3 * 256 + 255);
  }

  #[test]
  fn read_compact_atom_length_unsupported_encoding() {
    // bit3=1, bit4=1 → unsupported
    let mut r = BinaryReader::from_bytes(vec![0x18]);
    let err = BeamFile::read_compact_atom_length(&mut r).unwrap_err();
    match err {
      RtErr::CodeLoadingFailed(msg) => {
        assert!(msg.contains("Unsupported compact atom length"));
      }
      other => panic!("unexpected error: {:?}", other),
    }
  }

  #[test]
  fn load_atoms_utf8_otp22_positive_count() {
    // Positive count=2, classic u8 length encoding
    let mut r = BinaryReader::from_bytes(vec![
      0, 0, 0, 2, // count=2 (positive, OTP 22 style)
      5, b'h', b'e', b'l', b'l', b'o',
      3, b'f', b'o', b'o',
    ]);
    let mut bf = BeamFile::new();
    bf.load_atoms_utf8(&mut r).unwrap();
    assert_eq!(bf.atoms.len(), 2);
    assert_eq!(bf.atoms[0], "hello");
    assert_eq!(bf.atoms[1], "foo");
  }

  #[test]
  fn load_atoms_utf8_otp28_negative_count_compact() {
    // Negative count=-2 (as u32: 0xFFFFFFFE), compact atom lengths
    // Atom "lists" → len=5, compact byte: 5 << 4 = 0x50
    // Atom "ok"    → len=2, compact byte: 2 << 4 = 0x20
    let mut r = BinaryReader::from_bytes(vec![
      0xFF, 0xFF, 0xFF, 0xFE, // count=-2 as i32
      0x50, b'l', b'i', b's', b't', b's', // compact len=5, "lists"
      0x20, b'o', b'k',                     // compact len=2, "ok"
    ]);
    let mut bf = BeamFile::new();
    bf.load_atoms_utf8(&mut r).unwrap();
    assert_eq!(bf.atoms.len(), 2);
    assert_eq!(bf.atoms[0], "lists");
    assert_eq!(bf.atoms[1], "ok");
  }

  #[test]
  fn load_literals_uncompressed_otp25() {
    // uncomp_sz=0 means uncompressed (OTP 25+)
    // Literal data: count=1, then one ETF literal: size=4, <<131, 97, 42>> (small_integer 42)
    let literal_data = vec![
      0, 0, 0, 1,    // count=1
      0, 0, 0, 3,    // size=3
      131, 97, 42,   // ETF: version=131, SMALL_INTEGER_EXT=97, value=42
    ];
    let chunk_sz = 4 + literal_data.len(); // uncomp_sz(4) + raw_data
    let mut data = vec![0, 0, 0, 0]; // uncomp_sz=0
    data.extend_from_slice(&literal_data);

    let mut r = BinaryReader::from_bytes(data);
    let mut bf = BeamFile::new();
    bf.load_literals(&mut r, chunk_sz);
    assert_eq!(bf.lit_tab.len(), 1);
    // The decoded literal should be small integer 42
    assert!(bf.lit_tab[0].is_small());
    assert_eq!(bf.lit_tab[0].get_small_signed(), 42);
  }

  // --- Edge cases ---

  #[test]
  fn read_compact_atom_length_all_single_byte_values() {
    // Single-byte encoding covers lengths 0..15 (bit3=0, value = byte >> 4)
    for expected_len in 0..=15u8 {
      let byte = expected_len << 4; // bit3 is 0
      let mut r = BinaryReader::from_bytes(vec![byte]);
      let len = BeamFile::read_compact_atom_length(&mut r).unwrap();
      assert_eq!(len, expected_len as usize, "byte=0x{:02X}", byte);
    }
  }

  #[test]
  fn read_compact_atom_length_two_byte_zero_hi() {
    // Two-byte: hi=0, lo=0 → length=0
    let mut r = BinaryReader::from_bytes(vec![0x08, 0x00]);
    let len = BeamFile::read_compact_atom_length(&mut r).unwrap();
    assert_eq!(len, 0);
  }

  #[test]
  fn read_compact_atom_length_two_byte_max_hi() {
    // hi bits come from bits 5-7 of first byte: max hi = 0b111 = 7
    // byte = 0b111_01_000 = 0xE8, lo = 0xFF → length = 7*256 + 255 = 2047
    let mut r = BinaryReader::from_bytes(vec![0xE8, 0xFF]);
    let len = BeamFile::read_compact_atom_length(&mut r).unwrap();
    assert_eq!(len, 7 * 256 + 255);
  }

  #[test]
  fn load_atoms_utf8_empty_atom_name() {
    // OTP 22 style: one atom with length 0 (empty atom '')
    let mut r = BinaryReader::from_bytes(vec![
      0, 0, 0, 1, // count=1
      0,           // length=0 (empty string)
    ]);
    let mut bf = BeamFile::new();
    bf.load_atoms_utf8(&mut r).unwrap();
    assert_eq!(bf.atoms.len(), 1);
    assert_eq!(bf.atoms[0], "");
  }

  #[test]
  fn load_atoms_utf8_otp28_single_atom() {
    // Negative count=-1, compact encoding, atom "x" (len=1, byte=0x10)
    let mut r = BinaryReader::from_bytes(vec![
      0xFF, 0xFF, 0xFF, 0xFF, // count=-1 as u32
      0x10, b'x',             // compact len=1, "x"
    ]);
    let mut bf = BeamFile::new();
    bf.load_atoms_utf8(&mut r).unwrap();
    assert_eq!(bf.atoms.len(), 1);
    assert_eq!(bf.atoms[0], "x");
  }

  #[test]
  fn load_atoms_utf8_otp28_zero_atoms() {
    // Edge: negative count=-0 is just 0, positive path, 0 atoms
    let mut r = BinaryReader::from_bytes(vec![0, 0, 0, 0]);
    let mut bf = BeamFile::new();
    bf.load_atoms_utf8(&mut r).unwrap();
    assert_eq!(bf.atoms.len(), 0);
  }

  #[test]
  fn load_atoms_latin1_basic() {
    let mut r = BinaryReader::from_bytes(vec![
      0, 0, 0, 2, // count=2
      3, b'f', b'o', b'o',
      3, b'b', b'a', b'r',
    ]);
    let mut bf = BeamFile::new();
    bf.load_atoms_latin1(&mut r).unwrap();
    assert_eq!(bf.atoms, vec!["foo", "bar"]);
  }

  #[test]
  fn load_atoms_latin1_empty() {
    let mut r = BinaryReader::from_bytes(vec![0, 0, 0, 0]);
    let mut bf = BeamFile::new();
    bf.load_atoms_latin1(&mut r).unwrap();
    assert_eq!(bf.atoms.len(), 0);
  }

  #[test]
  fn load_atoms_utf8_multibyte_utf8_char() {
    // Atom with a 2-byte UTF-8 character: "ö" is 0xC3 0xB6
    let mut r = BinaryReader::from_bytes(vec![
      0, 0, 0, 1, // count=1 (positive, OTP 22)
      2,           // length=2 bytes
      0xC3, 0xB6,  // "ö" in UTF-8
    ]);
    let mut bf = BeamFile::new();
    bf.load_atoms_utf8(&mut r).unwrap();
    assert_eq!(bf.atoms.len(), 1);
    assert_eq!(bf.atoms[0], "ö");
  }

  #[test]
  fn load_literals_uncompressed_multiple() {
    // Two uncompressed literals: 42 and 100
    let literal_data = vec![
      0, 0, 0, 2,    // count=2
      0, 0, 0, 3,    // size=3
      131, 97, 42,   // ETF small_integer 42
      0, 0, 0, 3,    // size=3
      131, 97, 100,  // ETF small_integer 100
    ];
    let chunk_sz = 4 + literal_data.len();
    let mut data = vec![0, 0, 0, 0]; // uncomp_sz=0
    data.extend_from_slice(&literal_data);

    let mut r = BinaryReader::from_bytes(data);
    let mut bf = BeamFile::new();
    bf.load_literals(&mut r, chunk_sz);
    assert_eq!(bf.lit_tab.len(), 2);
    assert_eq!(bf.lit_tab[0].get_small_signed(), 42);
    assert_eq!(bf.lit_tab[1].get_small_signed(), 100);
  }

  #[test]
  fn load_literals_uncompressed_zero_count() {
    // Zero literals
    let literal_data = vec![0, 0, 0, 0]; // count=0
    let chunk_sz = 4 + literal_data.len();
    let mut data = vec![0, 0, 0, 0]; // uncomp_sz=0
    data.extend_from_slice(&literal_data);

    let mut r = BinaryReader::from_bytes(data);
    let mut bf = BeamFile::new();
    bf.load_literals(&mut r, chunk_sz);
    assert_eq!(bf.lit_tab.len(), 0);
  }
}
