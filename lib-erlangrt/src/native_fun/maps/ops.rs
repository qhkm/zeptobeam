//! Implements maps module BIFs.
use crate::{
  emulator::{
    gen_atoms,
    heap::THeapOwner,
    process::Process,
    vm::VM,
  },
  fail::{self, RtResult},
  native_fun::assert_arity,
  term::{
    boxed::Map,
    compare::cmp_terms,
    cons,
    term_builder::{tuple_builder::tuple2, ListBuilder},
    Term,
  },
};
use core::cmp::Ordering;

#[allow(dead_code)]
fn module() -> &'static str {
  "native funs module for maps: "
}

// ---- maps:new/0 ----
// Returns an empty map.
pub fn nativefun_maps_new_0(
  _vm: &mut VM,
  _curr_p: &mut Process,
  args: &[Term],
) -> RtResult<Term> {
  assert_arity("maps:new", 0, args);
  Ok(Term::empty_map())
}

// ---- maps:get/2 ----
// Get value by key, raise {badkey, K} if missing.
pub fn nativefun_maps_get_2(
  _vm: &mut VM,
  curr_p: &mut Process,
  args: &[Term],
) -> RtResult<Term> {
  assert_arity("maps:get", 2, args);
  let key = args[0];
  let map = args[1];

  if !map.is_map() {
    return fail::create::badarg();
  }

  if map == Term::empty_map() {
    let hp = curr_p.get_heap_mut();
    return fail::create::generic_tuple2_fail(gen_atoms::BADKEY, key, hp);
  }

  let map_p = map.get_box_ptr::<Map>();
  match unsafe { Map::get(map_p, key) }? {
    Some(val) => Ok(val),
    None => {
      let hp = curr_p.get_heap_mut();
      fail::create::generic_tuple2_fail(gen_atoms::BADKEY, key, hp)
    }
  }
}

// ---- maps:get/3 ----
// Get value by key with default.
pub fn nativefun_maps_get_3(
  _vm: &mut VM,
  _curr_p: &mut Process,
  args: &[Term],
) -> RtResult<Term> {
  assert_arity("maps:get", 3, args);
  let key = args[0];
  let map = args[1];
  let default = args[2];

  if !map.is_map() {
    return fail::create::badarg();
  }

  if map == Term::empty_map() {
    return Ok(default);
  }

  let map_p = map.get_box_ptr::<Map>();
  match unsafe { Map::get(map_p, key) }? {
    Some(val) => Ok(val),
    None => Ok(default),
  }
}

// ---- maps:put/3 ----
// Insert or update a key-value pair. Returns a new map.
pub fn nativefun_maps_put_3(
  _vm: &mut VM,
  curr_p: &mut Process,
  args: &[Term],
) -> RtResult<Term> {
  assert_arity("maps:put", 3, args);
  let key = args[0];
  let value = args[1];
  let map = args[2];

  if !map.is_map() {
    return fail::create::badarg();
  }

  let hp = curr_p.get_heap_mut();

  if map == Term::empty_map() {
    // Create a new 1-element map
    let new_map_p = Map::create_into(hp, 1)?;
    unsafe { Map::add(new_map_p, key, value)? };
    return Ok(Term::make_boxed(new_map_p));
  }

  let old_map_p = map.get_box_ptr::<Map>();
  let old_count = unsafe { (*old_map_p).get_count() };

  // Check if key already exists to determine new size
  let key_exists = unsafe { Map::get(old_map_p, key)? }.is_some();
  let new_count = if key_exists {
    old_count
  } else {
    old_count + 1
  };

  // Create new map and copy all existing pairs, updating the key if found
  let new_map_p = Map::create_into(hp, new_count)?;
  let old_data = unsafe { old_map_p.add(1) as *const Term };
  for i in 0..old_count {
    let k = unsafe { old_data.add(i * 2).read() };
    let v = unsafe { old_data.add(i * 2 + 1).read() };
    unsafe { Map::add(new_map_p, k, v)? };
  }
  // Add/update the new key-value pair
  unsafe { Map::add(new_map_p, key, value)? };

  Ok(Term::make_boxed(new_map_p))
}

// ---- maps:keys/1 ----
// Return list of keys.
pub fn nativefun_maps_keys_1(
  _vm: &mut VM,
  curr_p: &mut Process,
  args: &[Term],
) -> RtResult<Term> {
  assert_arity("maps:keys", 1, args);
  let map = args[0];

  if !map.is_map() {
    return fail::create::badarg();
  }

  if map == Term::empty_map() {
    return Ok(Term::nil());
  }

  let map_p = map.get_box_ptr::<Map>();
  let count = unsafe { (*map_p).get_count() };
  let data = unsafe { map_p.add(1) as *const Term };

  let hp = curr_p.get_heap_mut();
  let mut lb = ListBuilder::new()?;
  for i in 0..count {
    let key = unsafe { data.add(i * 2).read() };
    unsafe { lb.append(key, hp)? };
  }

  if count == 0 {
    Ok(Term::nil())
  } else {
    Ok(lb.make_term())
  }
}

// ---- maps:values/1 ----
// Return list of values.
pub fn nativefun_maps_values_1(
  _vm: &mut VM,
  curr_p: &mut Process,
  args: &[Term],
) -> RtResult<Term> {
  assert_arity("maps:values", 1, args);
  let map = args[0];

  if !map.is_map() {
    return fail::create::badarg();
  }

  if map == Term::empty_map() {
    return Ok(Term::nil());
  }

  let map_p = map.get_box_ptr::<Map>();
  let count = unsafe { (*map_p).get_count() };
  let data = unsafe { map_p.add(1) as *const Term };

  let hp = curr_p.get_heap_mut();
  let mut lb = ListBuilder::new()?;
  for i in 0..count {
    let val = unsafe { data.add(i * 2 + 1).read() };
    unsafe { lb.append(val, hp)? };
  }

  if count == 0 {
    Ok(Term::nil())
  } else {
    Ok(lb.make_term())
  }
}

// ---- maps:is_key/2 ----
// Check if key exists, return true/false.
pub fn nativefun_maps_is_key_2(
  _vm: &mut VM,
  _curr_p: &mut Process,
  args: &[Term],
) -> RtResult<Term> {
  assert_arity("maps:is_key", 2, args);
  let key = args[0];
  let map = args[1];

  if !map.is_map() {
    return fail::create::badarg();
  }

  if map == Term::empty_map() {
    return Ok(gen_atoms::FALSE);
  }

  let map_p = map.get_box_ptr::<Map>();
  let found = unsafe { Map::get(map_p, key)? }.is_some();
  Ok(Term::make_bool(found))
}

// ---- maps:size/1 ----
// Return map size as integer.
pub fn nativefun_maps_size_1(
  _vm: &mut VM,
  _curr_p: &mut Process,
  args: &[Term],
) -> RtResult<Term> {
  assert_arity("maps:size", 1, args);
  let map = args[0];

  if !map.is_map() {
    return fail::create::badarg();
  }

  if map == Term::empty_map() {
    return Ok(Term::make_small_unsigned(0));
  }

  Ok(Term::make_small_unsigned(map.map_size()))
}

// ---- maps:merge/2 ----
// Merge two maps. Values from the second map override the first.
pub fn nativefun_maps_merge_2(
  _vm: &mut VM,
  curr_p: &mut Process,
  args: &[Term],
) -> RtResult<Term> {
  assert_arity("maps:merge", 2, args);
  let map1 = args[0];
  let map2 = args[1];

  if !map1.is_map() || !map2.is_map() {
    return fail::create::badarg();
  }

  if map1 == Term::empty_map() {
    return Ok(map2);
  }
  if map2 == Term::empty_map() {
    return Ok(map1);
  }

  let map1_p = map1.get_box_ptr::<Map>();
  let map2_p = map2.get_box_ptr::<Map>();
  let count1 = unsafe { (*map1_p).get_count() };
  let count2 = unsafe { (*map2_p).get_count() };

  let hp = curr_p.get_heap_mut();
  // Worst case: all keys are unique
  let new_map_p = Map::create_into(hp, count1 + count2)?;

  // Copy all pairs from map1
  let data1 = unsafe { map1_p.add(1) as *const Term };
  for i in 0..count1 {
    let k = unsafe { data1.add(i * 2).read() };
    let v = unsafe { data1.add(i * 2 + 1).read() };
    unsafe { Map::add(new_map_p, k, v)? };
  }

  // Add all pairs from map2 (overriding map1 keys)
  let data2 = unsafe { map2_p.add(1) as *const Term };
  for i in 0..count2 {
    let k = unsafe { data2.add(i * 2).read() };
    let v = unsafe { data2.add(i * 2 + 1).read() };
    unsafe { Map::add(new_map_p, k, v)? };
  }

  Ok(Term::make_boxed(new_map_p))
}

// ---- maps:remove/2 ----
// Remove a key from a map. Returns a new map.
pub fn nativefun_maps_remove_2(
  _vm: &mut VM,
  curr_p: &mut Process,
  args: &[Term],
) -> RtResult<Term> {
  assert_arity("maps:remove", 2, args);
  let key = args[0];
  let map = args[1];

  if !map.is_map() {
    return fail::create::badarg();
  }

  if map == Term::empty_map() {
    return Ok(Term::empty_map());
  }

  let map_p = map.get_box_ptr::<Map>();
  let count = unsafe { (*map_p).get_count() };

  // Check if key exists
  let key_exists = unsafe { Map::get(map_p, key)? }.is_some();
  if !key_exists {
    return Ok(map);
  }

  if count == 1 {
    return Ok(Term::empty_map());
  }

  let hp = curr_p.get_heap_mut();
  let new_map_p = Map::create_into(hp, count - 1)?;
  let data = unsafe { map_p.add(1) as *const Term };
  for i in 0..count {
    let k = unsafe { data.add(i * 2).read() };
    let v = unsafe { data.add(i * 2 + 1).read() };
    // Skip the key we want to remove
    let is_same = cmp_terms(k, key, true)? == Ordering::Equal;
    if !is_same {
      unsafe { Map::add(new_map_p, k, v)? };
    }
  }

  Ok(Term::make_boxed(new_map_p))
}

// ---- maps:to_list/1 ----
// Convert map to list of {K, V} pairs.
pub fn nativefun_maps_to_list_1(
  _vm: &mut VM,
  curr_p: &mut Process,
  args: &[Term],
) -> RtResult<Term> {
  assert_arity("maps:to_list", 1, args);
  let map = args[0];

  if !map.is_map() {
    return fail::create::badarg();
  }

  if map == Term::empty_map() {
    return Ok(Term::nil());
  }

  let map_p = map.get_box_ptr::<Map>();
  let count = unsafe { (*map_p).get_count() };
  let data = unsafe { map_p.add(1) as *const Term };

  let hp = curr_p.get_heap_mut();
  let mut lb = ListBuilder::new()?;
  for i in 0..count {
    let k = unsafe { data.add(i * 2).read() };
    let v = unsafe { data.add(i * 2 + 1).read() };
    let pair = tuple2(hp, k, v)?;
    unsafe { lb.append(pair, hp)? };
  }

  if count == 0 {
    Ok(Term::nil())
  } else {
    Ok(lb.make_term())
  }
}

// ---- maps:from_list/1 ----
// Create map from list of {K, V} pairs.
pub fn nativefun_maps_from_list_1(
  _vm: &mut VM,
  curr_p: &mut Process,
  args: &[Term],
) -> RtResult<Term> {
  assert_arity("maps:from_list", 1, args);
  let list = args[0];

  if !list.is_list() {
    return fail::create::badarg();
  }

  if list == Term::nil() {
    return Ok(Term::empty_map());
  }

  // First pass: count elements
  let count = cons::list_length(list)?;

  let hp = curr_p.get_heap_mut();
  let new_map_p = Map::create_into(hp, count)?;

  // Second pass: add pairs
  cons::for_each(list, |elem| {
    if !elem.is_tuple() {
      return fail::create::badarg();
    }
    let tuple_p = elem.get_tuple_ptr();
    if unsafe { (*tuple_p).get_arity() } != 2 {
      return fail::create::badarg();
    }
    let k = unsafe { (*tuple_p).get_element(0) };
    let v = unsafe { (*tuple_p).get_element(1) };
    unsafe { Map::add(new_map_p, k, v)? };
    Ok(())
  })?;

  Ok(Term::make_boxed(new_map_p))
}

// ---- maps:find/2 ----
// Return {ok, V} or error.
pub fn nativefun_maps_find_2(
  _vm: &mut VM,
  curr_p: &mut Process,
  args: &[Term],
) -> RtResult<Term> {
  assert_arity("maps:find", 2, args);
  let key = args[0];
  let map = args[1];

  if !map.is_map() {
    return fail::create::badarg();
  }

  if map == Term::empty_map() {
    return Ok(gen_atoms::ERROR);
  }

  let map_p = map.get_box_ptr::<Map>();
  match unsafe { Map::get(map_p, key) }? {
    Some(val) => {
      let hp = curr_p.get_heap_mut();
      let result = tuple2(hp, gen_atoms::OK, val)?;
      Ok(result)
    }
    None => Ok(gen_atoms::ERROR),
  }
}
