use crate::{
  emulator::gen_atoms,
  native_fun::{
    erlang::{
      arithmetic::*, binary::*, compare::*, list::*, predicate::*, process::*, sys::*,
      tuple::*, type_conversions::*,
    },
    fn_entry::NativeFnEntry,
    module::NativeModule,
  },
};

pub mod arithmetic;
pub mod binary;
pub mod compare;
pub mod list;
pub mod predicate;
pub mod process;
pub mod sys;
pub mod tuple;
pub mod type_conversions;

pub fn new() -> NativeModule {
  let mut m = NativeModule::new(gen_atoms::ERLANG);
  let fn_entries: Vec<NativeFnEntry> = vec![
    NativeFnEntry::with_str("*", 2, nativefun_multiply_2),
    NativeFnEntry::with_str("+", 2, nativefun_plus_2),
    NativeFnEntry::with_str("++", 2, NfErlangPlusPlus2::_f),
    NativeFnEntry::with_str("-", 2, nativefun_minus_2),
    NativeFnEntry::with_str("/", 2, nativefun_float_div_2),
    NativeFnEntry::with_str("/=", 2, nativefun_notequal_2),
    NativeFnEntry::with_str("<", 2, nativefun_lessthan_2),
    NativeFnEntry::with_str("=/=", 2, nativefun_notequal_exact_2),
    NativeFnEntry::with_str("=:=", 2, nativefun_equal_exact_2),
    NativeFnEntry::with_str("=<", 2, nativefun_lessequal_2),
    NativeFnEntry::with_str("==", 2, nativefun_equalequal_2),
    NativeFnEntry::with_str(">", 2, nativefun_greaterthan_2),
    NativeFnEntry::with_str(">=", 2, nativefun_greaterequal_2),
    NativeFnEntry::with_str("abs", 1, nativefun_abs_1),
    NativeFnEntry::with_str("atom_to_list", 1, NfErlangA2List2::_f),
    NativeFnEntry::with_str("band", 2, nativefun_band_2),
    NativeFnEntry::with_str("bnot", 1, nativefun_bnot_1),
    NativeFnEntry::with_str("bor", 2, nativefun_bor_2),
    NativeFnEntry::with_str("bsl", 2, nativefun_bsl_2),
    NativeFnEntry::with_str("bsr", 2, nativefun_bsr_2),
    NativeFnEntry::with_str("bxor", 2, nativefun_bxor_2),
    NativeFnEntry::with_str("div", 2, nativefun_div_2),
    NativeFnEntry::with_str("error", 1, NfErlangError1::_f),
    NativeFnEntry::with_str("error", 2, NfErlangError2::_f),
    NativeFnEntry::with_str("float", 1, nativefun_float_1),
    NativeFnEntry::with_str("hd", 1, NfErlangHd1::_f),
    NativeFnEntry::with_str("integer_to_list", 1, NfErlangInt2List2::_f),
    NativeFnEntry::with_str("is_boolean", 1, nativefun_is_boolean_1),
    NativeFnEntry::with_str("is_process_alive", 1, NfErlangIsPAlive1::_f),
    NativeFnEntry::with_str("length", 1, NfErlangLength1::_f),
    NativeFnEntry::with_str("list_to_binary", 1, NfErlangL2b1::_f),
    NativeFnEntry::with_str("load_nif", 2, NfErlangLoadNif2::_f),
    NativeFnEntry::with_str("make_fun", 3, nativefun_make_fun_3),
    NativeFnEntry::with_str("md5", 1, NfErlangMd51::_f),
    NativeFnEntry::with_str("nif_error", 1, NfErlangNifError1::_f),
    NativeFnEntry::with_str("nif_error", 2, NfErlangNifError2::_f),
    NativeFnEntry::with_str("process_flag", 2, NfErlangProcFlag2::_f),
    NativeFnEntry::with_str("process_flag", 3, NfErlangProcFlag3::_f),
    NativeFnEntry::with_str("register", 2, NfErlangRegister2::_f),
    NativeFnEntry::with_str("rem", 2, nativefun_rem_2),
    NativeFnEntry::with_str("registered", 0, NfErlangRegistered0::_f),
    NativeFnEntry::with_str("self", 0, NfErlangSelf0::_f),
    NativeFnEntry::with_str("size", 1, NfErlangSize1::_f),
    NativeFnEntry::with_str("bit_size", 1, NfErlangBitSize1::_f),
    NativeFnEntry::with_str("byte_size", 1, NfErlangByteSize1::_f),
    NativeFnEntry::with_str("binary_to_list", 1, NfErlangB2l1::_f),
    NativeFnEntry::with_str("spawn", 3, NfErlangSpawn3::_f),
    NativeFnEntry::with_str("tl", 1, NfErlangTl1::_f),
    // New BIFs: tuple operations
    NativeFnEntry::with_str("element", 2, nativefun_element_2),
    NativeFnEntry::with_str("setelement", 3, nativefun_setelement_3),
    NativeFnEntry::with_str("tuple_size", 1, nativefun_tuple_size_1),
    NativeFnEntry::with_str("tuple_to_list", 1, nativefun_tuple_to_list_1),
    NativeFnEntry::with_str("list_to_tuple", 1, nativefun_list_to_tuple_1),
    // New BIFs: map operations
    NativeFnEntry::with_str("map_size", 1, nativefun_map_size_1),
    NativeFnEntry::with_str("is_map_key", 2, nativefun_is_map_key_2),
    // New BIFs: exceptions
    NativeFnEntry::with_str("throw", 1, NfErlangThrow1::_f),
    NativeFnEntry::with_str("exit", 1, NfErlangExit1::_f),
    // New BIFs: comparison
    NativeFnEntry::with_str("max", 2, nativefun_max_2),
    NativeFnEntry::with_str("min", 2, nativefun_min_2),
    // New BIFs: type conversions
    NativeFnEntry::with_str("atom_to_binary", 1, nativefun_atom_to_binary_1),
    NativeFnEntry::with_str("binary_to_atom", 1, nativefun_binary_to_atom_1),
  ];
  m.init_with(fn_entries.iter());
  m
}
