//! Module implements opcodes related to map and record operations.
use crate::{
  beam::disp_result::DispatchResult,
  defs::exc_type::ExceptionType,
  emulator::{gen_atoms, heap::THeapOwner, process::Process, runtime_ctx::*},
  fail::{self, RtErr, RtResult},
  term::{boxed, term_builder::tuple_builder::tuple2, Term},
};

// fn module() -> &'static str {
//   "opcodes::op_map: "
// }

// Checks that argument is a map, otherwise jumps to label.
// Structure: is_map(on_false:label, val:src)
define_opcode!(_vm, ctx, curr_p,
  name: OpcodeIsMap, arity: 2,
  run: {
    if !value.is_map() { ctx.jump(fail) }
    Ok(DispatchResult::Normal)
  },
  args: cp_or_nil(fail), load(value),
);

// Creates or updates a map using the assoc operator (=>).
// Structure: put_map_assoc(fail:label, src:map, dst:reg, live:int, pairs:extlist)
// The pairs extlist is stored as a tuple: {K1, V1, K2, V2, ...}
define_opcode!(_vm, ctx, curr_p,
  name: OpcodePutMapAssoc, arity: 5,
  run: { Self::put_map_assoc(ctx, curr_p, fail, src, dst, _live, pairs) },
  args: cp_or_nil(fail), load(src), term(dst), usize(_live), term(pairs),
);

impl OpcodePutMapAssoc {
  #[inline]
  pub fn put_map_assoc(
    ctx: &mut RuntimeContext,
    curr_p: &mut Process,
    _fail: Term,
    src: Term,
    dst: Term,
    _live: usize,
    pairs: Term,
  ) -> RtResult<DispatchResult> {
    let hp = curr_p.get_heap_mut();

    // Get the pairs from the tuple
    let pairs_ptr = pairs.get_tuple_ptr();
    let pairs_count = unsafe { (*pairs_ptr).get_arity() };
    let num_new_pairs = pairs_count / 2;

    // Count existing pairs from source map
    let src_count = if src == Term::empty_map() {
      0
    } else if src.is_map() {
      src.map_size()
    } else {
      return fail::create::badarg();
    };

    // Allocate a new map with enough capacity
    let new_map_p = boxed::Map::create_into(hp, src_count + num_new_pairs)?;

    // Copy existing pairs from source map
    if src != Term::empty_map() && src_count > 0 {
      let src_map_p = src.get_box_ptr::<boxed::Map>();
      let src_data = unsafe { (src_map_p as *const Term).add(
        core::mem::size_of::<boxed::Map>() / core::mem::size_of::<Term>()
      ) };
      for i in 0..src_count {
        let key = unsafe { src_data.add(i * 2).read() };
        let val = unsafe { src_data.add(i * 2 + 1).read() };
        unsafe { boxed::Map::add(new_map_p, key, val)? };
      }
    }

    // Add/update new pairs from the instruction
    for i in 0..num_new_pairs {
      let key = unsafe { (*pairs_ptr).get_element(i * 2) };
      let val = unsafe { (*pairs_ptr).get_element(i * 2 + 1) };
      let key = ctx.load(key, hp);
      let val = ctx.load(val, hp);
      unsafe { boxed::Map::add(new_map_p, key, val)? };
    }

    ctx.store_value(Term::make_boxed(new_map_p), dst, hp)?;
    Ok(DispatchResult::Normal)
  }
}

// Updates an existing map using the exact operator (:=).
// Keys MUST already exist in the map; if not, jump to fail label.
// Structure: put_map_exact(fail:label, src:map, dst:reg, live:int, pairs:extlist)
define_opcode!(_vm, ctx, curr_p,
  name: OpcodePutMapExact, arity: 5,
  run: { Self::put_map_exact(ctx, curr_p, fail, src, dst, _live, pairs) },
  args: cp_or_nil(fail), load(src), term(dst), usize(_live), term(pairs),
);

impl OpcodePutMapExact {
  #[inline]
  pub fn put_map_exact(
    ctx: &mut RuntimeContext,
    curr_p: &mut Process,
    fail: Term,
    src: Term,
    dst: Term,
    _live: usize,
    pairs: Term,
  ) -> RtResult<DispatchResult> {
    let hp = curr_p.get_heap_mut();

    // Source must be a map
    if !src.is_map() {
      if fail == Term::nil() {
        return Err(RtErr::Exception(
          ExceptionType::Error,
          tuple2(hp, gen_atoms::BADMAP, src)?,
        ));
      }
      ctx.jump(fail);
      return Ok(DispatchResult::Normal);
    }

    // Get the pairs from the tuple
    let pairs_ptr = pairs.get_tuple_ptr();
    let pairs_count = unsafe { (*pairs_ptr).get_arity() };
    let num_new_pairs = pairs_count / 2;

    // First verify all keys exist in the source map
    if src != Term::empty_map() {
      let src_map_p = src.get_box_ptr::<boxed::Map>();
      for i in 0..num_new_pairs {
        let key = unsafe { (*pairs_ptr).get_element(i * 2) };
        let key = ctx.load(key, hp);
        let found = unsafe { boxed::Map::get(src_map_p, key)? };
        if found.is_none() {
          if fail == Term::nil() {
            return Err(RtErr::Exception(
              ExceptionType::Error,
              tuple2(hp, gen_atoms::BADKEY, key)?,
            ));
          }
          ctx.jump(fail);
          return Ok(DispatchResult::Normal);
        }
      }
    } else if num_new_pairs > 0 {
      // Empty map can't have exact updates
      if fail == Term::nil() {
        let key = unsafe { (*pairs_ptr).get_element(0) };
        let key = ctx.load(key, hp);
        return Err(RtErr::Exception(
          ExceptionType::Error,
          tuple2(hp, gen_atoms::BADKEY, key)?,
        ));
      }
      ctx.jump(fail);
      return Ok(DispatchResult::Normal);
    }

    // All keys exist, now create a clone and apply updates
    let src_count = src.map_size();
    let new_map_p = boxed::Map::create_into(hp, src_count)?;

    // Copy all existing pairs
    if src != Term::empty_map() && src_count > 0 {
      let src_map_p = src.get_box_ptr::<boxed::Map>();
      let src_data = unsafe { (src_map_p as *const Term).add(
        core::mem::size_of::<boxed::Map>() / core::mem::size_of::<Term>()
      ) };
      for i in 0..src_count {
        let key = unsafe { src_data.add(i * 2).read() };
        let val = unsafe { src_data.add(i * 2 + 1).read() };
        unsafe { boxed::Map::add(new_map_p, key, val)? };
      }
    }

    // Apply the exact updates
    for i in 0..num_new_pairs {
      let key = unsafe { (*pairs_ptr).get_element(i * 2) };
      let val = unsafe { (*pairs_ptr).get_element(i * 2 + 1) };
      let key = ctx.load(key, hp);
      let val = ctx.load(val, hp);
      unsafe { boxed::Map::add(new_map_p, key, val)? };
    }

    ctx.store_value(Term::make_boxed(new_map_p), dst, hp)?;
    Ok(DispatchResult::Normal)
  }
}

// Check that map contains all given keys. Jump to fail if any is missing.
// Structure: has_map_fields(fail:label, src:map, keys:extlist)
// The keys extlist is a tuple: {K1, K2, ...}
define_opcode!(_vm, ctx, curr_p,
  name: OpcodeHasMapFields, arity: 3,
  run: { Self::has_map_fields(ctx, curr_p, fail, src, keys) },
  args: cp_or_nil(fail), load(src), term(keys),
);

impl OpcodeHasMapFields {
  #[inline]
  pub fn has_map_fields(
    ctx: &mut RuntimeContext,
    curr_p: &mut Process,
    fail: Term,
    src: Term,
    keys: Term,
  ) -> RtResult<DispatchResult> {
    let hp = curr_p.get_heap_mut();

    if !src.is_map() {
      ctx.jump(fail);
      return Ok(DispatchResult::Normal);
    }

    if src == Term::empty_map() {
      // Empty map has no fields
      ctx.jump(fail);
      return Ok(DispatchResult::Normal);
    }

    let keys_ptr = keys.get_tuple_ptr();
    let keys_count = unsafe { (*keys_ptr).get_arity() };
    let src_map_p = src.get_box_ptr::<boxed::Map>();

    for i in 0..keys_count {
      let key = unsafe { (*keys_ptr).get_element(i) };
      let key = ctx.load(key, hp);
      let found = unsafe { boxed::Map::get(src_map_p, key)? };
      if found.is_none() {
        ctx.jump(fail);
        return Ok(DispatchResult::Normal);
      }
    }

    Ok(DispatchResult::Normal)
  }
}

// Extract values from map for given keys and store them into destination registers.
// Structure: get_map_elements(fail:label, src:map, pairs:extlist)
// The pairs extlist is a tuple: {K1, Dst1, K2, Dst2, ...}
define_opcode!(_vm, ctx, curr_p,
  name: OpcodeGetMapElements, arity: 3,
  run: { Self::get_map_elements(ctx, curr_p, fail, src, pairs) },
  args: cp_or_nil(fail), load(src), term(pairs),
);

impl OpcodeGetMapElements {
  #[inline]
  pub fn get_map_elements(
    ctx: &mut RuntimeContext,
    curr_p: &mut Process,
    fail: Term,
    src: Term,
    pairs: Term,
  ) -> RtResult<DispatchResult> {
    let hp = curr_p.get_heap_mut();

    if !src.is_map() {
      ctx.jump(fail);
      return Ok(DispatchResult::Normal);
    }

    if src == Term::empty_map() {
      ctx.jump(fail);
      return Ok(DispatchResult::Normal);
    }

    let pairs_ptr = pairs.get_tuple_ptr();
    let pairs_count = unsafe { (*pairs_ptr).get_arity() };
    let num_pairs = pairs_count / 2;
    let src_map_p = src.get_box_ptr::<boxed::Map>();

    // First pass: check all keys exist (we need to not partially store)
    for i in 0..num_pairs {
      let key = unsafe { (*pairs_ptr).get_element(i * 2) };
      let key = ctx.load(key, hp);
      let found = unsafe { boxed::Map::get(src_map_p, key)? };
      if found.is_none() {
        ctx.jump(fail);
        return Ok(DispatchResult::Normal);
      }
    }

    // Second pass: store values into destination registers
    for i in 0..num_pairs {
      let key = unsafe { (*pairs_ptr).get_element(i * 2) };
      let dst = unsafe { (*pairs_ptr).get_element(i * 2 + 1) };
      let key = ctx.load(key, hp);
      let val = unsafe { boxed::Map::get(src_map_p, key)? }.unwrap();
      ctx.store_value(val, dst, hp)?;
    }

    Ok(DispatchResult::Normal)
  }
}

// Clone a tuple (src) into dst, then apply updates at given 1-based indices.
// Structure: update_record(hint:int, size:int, src:reg, dst:reg, updates:extlist)
// The updates extlist is a tuple: {Index1, Val1, Index2, Val2, ...}
// Indices are 1-based.
define_opcode!(_vm, ctx, curr_p,
  name: OpcodeUpdateRecord, arity: 5,
  run: { Self::update_record(ctx, curr_p, _hint, size, src, dst, updates) },
  args: usize(_hint), usize(size), load(src), term(dst), term(updates),
);

impl OpcodeUpdateRecord {
  #[inline]
  pub fn update_record(
    ctx: &mut RuntimeContext,
    curr_p: &mut Process,
    _hint: usize,
    size: usize,
    src: Term,
    dst: Term,
    updates: Term,
  ) -> RtResult<DispatchResult> {
    let hp = curr_p.get_heap_mut();

    // Clone the source tuple
    let src_tuple_p = src.get_tuple_ptr();
    let new_tuple_p = boxed::Tuple::create_into(hp, size)?;

    // Copy all elements from source
    for i in 0..size {
      let val = unsafe { (*src_tuple_p).get_element(i) };
      unsafe { (*new_tuple_p).set_element(i, val) };
    }

    // Apply updates from the ext_list (tuple of index-value pairs)
    let updates_ptr = updates.get_tuple_ptr();
    let updates_count = unsafe { (*updates_ptr).get_arity() };
    let num_updates = updates_count / 2;

    for i in 0..num_updates {
      let index_term = unsafe { (*updates_ptr).get_element(i * 2) };
      let val_raw = unsafe { (*updates_ptr).get_element(i * 2 + 1) };
      let val = ctx.load(val_raw, hp);

      // Index is 1-based in the instruction
      debug_assert!(index_term.is_small());
      let index = index_term.get_small_unsigned() - 1;
      unsafe { (*new_tuple_p).set_element(index, val) };
    }

    ctx.store_value(Term::make_boxed(new_tuple_p), dst, hp)?;
    Ok(DispatchResult::Normal)
  }
}

// Raise a {badrecord, Value} error exception.
// Structure: badrecord(value:src)
define_opcode!(_vm, _ctx, curr_p,
  name: OpcodeBadrecord, arity: 1,
  run: { Self::badrecord(curr_p, val) },
  args: load(val),
);

impl OpcodeBadrecord {
  #[inline]
  pub fn badrecord(
    curr_p: &mut Process,
    val: Term,
  ) -> RtResult<DispatchResult> {
    let hp = curr_p.get_heap_mut();
    Err(RtErr::Exception(
      ExceptionType::Error,
      tuple2(hp, gen_atoms::BADRECORD, val)?,
    ))
  }
}
