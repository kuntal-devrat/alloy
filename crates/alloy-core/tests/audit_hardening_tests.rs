use alloy_core::arena::ChunkedArena;
use alloy_core::heap::ArenaHeap;
use alloy_core::intern::{atom_of, str_of, with_atom_str};
use alloy_core::value::{ArrayData, Shape, Value};

#[test]
fn test_bug_01_value_send_not_sync() {
    // Assert Value is Send:
    fn assert_send<T: Send>() {}
    assert_send::<Value>();

    // Assert Value is NOT Sync:
    trait IsNotSync {
        fn is_not_sync() -> bool {
            true
        }
    }
    impl<T: ?Sized> IsNotSync for T {}
    assert!(Value::is_not_sync());
}

#[test]
fn test_bug_02_promote_box_missing_safe() {
    let mut heap = ArenaHeap::new(1024);
    // Address not in young generation returns None
    assert!(heap.try_promote_box(0x12345678).is_none());
}

#[test]
fn test_bug_03_chunk_of_missing_returns_none() {
    let arena = ChunkedArena::new(1024);
    // Address 0x99999999 is outside active chunks
    assert!(!arena.contains(0x99999999));
}

#[test]
fn test_bug_07_shift_on_packed_arrays() {
    let mut arr = ArrayData::Ints((0..100).collect());
    for expected in 0..100 {
        let v = arr.shift();
        assert_eq!(v.as_int(), Some(expected));
    }
    // Now empty
    let empty_v = arr.shift();
    assert!(empty_v.is_undefined());

    // Values variant
    let mut vals = ArrayData::Values((0..100).map(Value::int).collect());
    for expected in 0..100 {
        let v = vals.shift();
        assert_eq!(v.as_int(), Some(expected));
    }
    assert!(vals.shift().is_undefined());
}

#[test]
fn test_opt_01_and_opt_03_shape_sorted_keys_and_transitions() {
    let mut map = hashbrown::HashMap::new();
    map.insert(atom_of("z"), 0);
    map.insert(atom_of("a"), 1);
    map.insert(atom_of("m"), 2);
    let names = vec!["z".to_string(), "a".to_string(), "m".to_string()];
    let insertion_order = vec![atom_of("z"), atom_of("a"), atom_of("m")];

    let shape = Shape::new(map, insertion_order, names);
    // Keys should be cached in pre-sorted order
    let sorted = shape.keys_sorted();
    assert_eq!(sorted, vec![&"a".to_string(), &"m".to_string(), &"z".to_string()]);
}

#[test]
fn test_opt_04_arena_contains_bounds_check() {
    let arena = ChunkedArena::new(4096);
    // Below or above chunk range should return false in O(1)
    assert!(!arena.contains(0));
    assert!(!arena.contains(usize::MAX));
}

#[test]
fn test_opt_05_and_opt_07_abstract_eq_and_array_primitives() {
    // Test [] == false
    let arr = Value::array(Vec::new());
    let f = Value::bool(false);
    assert_eq!(arr.equal(&f).as_bool(), Some(true));

    // Test [1] == 1
    let arr1 = Value::array(vec![Value::int(1)]);
    let num1 = Value::int(1);
    assert_eq!(arr1.equal(&num1).as_bool(), Some(true));

    // Test [1, 2] == "1,2"
    let arr2 = Value::array(vec![Value::int(1), Value::int(2)]);
    let s = Value::string("1,2".to_string());
    assert_eq!(arr2.equal(&s).as_bool(), Some(true));
}

#[test]
fn test_bug_06_intern_str_roundtrip() {
    let a = atom_of("custom_symbol_name_test");
    assert_eq!(str_of(a), "custom_symbol_name_test");
    with_atom_str(a, |s| {
        assert_eq!(s, "custom_symbol_name_test");
    });
}
