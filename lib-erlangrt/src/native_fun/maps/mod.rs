pub mod ops;

use crate::{
  emulator::atom,
  native_fun::{
    fn_entry::NativeFnEntry,
    maps::ops::*,
    module::NativeModule,
  },
};

pub fn new() -> NativeModule {
  let mut m = NativeModule::new(atom::from_str("maps"));
  let fn_entries: Vec<NativeFnEntry> = vec![
    NativeFnEntry::with_str("new", 0, nativefun_maps_new_0),
    NativeFnEntry::with_str("get", 2, nativefun_maps_get_2),
    NativeFnEntry::with_str("get", 3, nativefun_maps_get_3),
    NativeFnEntry::with_str("put", 3, nativefun_maps_put_3),
    NativeFnEntry::with_str("keys", 1, nativefun_maps_keys_1),
    NativeFnEntry::with_str("values", 1, nativefun_maps_values_1),
    NativeFnEntry::with_str("is_key", 2, nativefun_maps_is_key_2),
    NativeFnEntry::with_str("size", 1, nativefun_maps_size_1),
    NativeFnEntry::with_str("merge", 2, nativefun_maps_merge_2),
    NativeFnEntry::with_str("remove", 2, nativefun_maps_remove_2),
    NativeFnEntry::with_str("to_list", 1, nativefun_maps_to_list_1),
    NativeFnEntry::with_str("from_list", 1, nativefun_maps_from_list_1),
    NativeFnEntry::with_str("find", 2, nativefun_maps_find_2),
  ];
  m.init_with(fn_entries.iter());
  m
}
