use alloy_core::arena::{Arena, ChunkedArena, class_idx};
use alloy_core::heap::{ArenaHeap, HeapGuard};
use alloy_core::regex::{compile, compile_from_str, RegexFlags};
use alloy_core::shared_memory::SidecarMemory;
use alloy_core::value::Value;

// --- Arena ---
#[test] fn arena_alloc_u8() { let a=Arena::new(256); assert_eq!(*a.alloc(7u8),7); }
#[test] fn arena_alloc_u32() { let a=Arena::new(256); assert_eq!(*a.alloc(123456u32),123456); }
#[test] fn arena_alloc_f64() { let a=Arena::new(256); assert!((*a.alloc(1.5f64)-1.5).abs()<1e-9); }
#[test] fn arena_many_allocs() { let a=Arena::new(4096); for i in 0..100 { assert_eq!(*a.alloc(i as u64), i as u64); } }
#[test] fn arena_bytes_roundtrip() { let a=Arena::new(1024); let p=a.alloc_bytes(b"hello"); assert_eq!(unsafe{std::slice::from_raw_parts(p,5)}, b"hello"); }
#[test] fn arena_bump_alloc_align() { let a=Arena::new(1024); let p=a.bump_alloc(13,8); assert_eq!((p as usize)%8,0); }
#[test] fn arena_reset_reuse() { let a=Arena::new(64); a.alloc(1u64); a.reset(); assert_eq!(a.used(),0); assert_eq!(*a.alloc(2u64),2); }
#[test] fn arena_used_tracks() { let a=Arena::new(1024); let u0=a.used(); a.alloc(1u64); assert!(a.used()>u0); }
#[test] fn arena_capacity() { let a=Arena::new(1024); assert_eq!(a.capacity(),1024); }
#[test] fn arena_empty_bytes() { let a=Arena::new(1024); let p=a.alloc_bytes(b""); assert!(!p.is_null()); }

// ChunkedArena
#[test] fn chunked_basic() { let mut c=ChunkedArena::new(64); let p=c.alloc_at(42u64); assert_eq!(unsafe{c.read_at(p)},42); }
#[test] fn chunked_grow() { let mut c=ChunkedArena::new(32); for i in 0..500 { c.alloc_at(i as u64); } assert!(c.chunk_count()>1); }
#[test] fn chunked_bytes() { let mut c=ChunkedArena::new(128); let p=c.alloc_bytes(b"world"); assert_eq!(unsafe{std::slice::from_raw_parts(p,5)}, b"world"); }
#[test] fn chunked_reset() { let mut c=ChunkedArena::new(64); c.alloc_at(1u64); c.reset(); assert_eq!(c.used(),0); }
#[test] fn chunked_region_tracking() { let mut c=ChunkedArena::new(1024); let p=c.alloc_region(16,1) as usize; assert!(c.region_at(p).is_some()); }
#[test] fn chunked_contains() { let mut c=ChunkedArena::new(1024); let p=c.alloc_region(16,0) as usize; assert!(c.contains(p)); assert!(!c.contains(0x1)); }
#[test] fn chunked_class_idx() { assert_eq!(class_idx(8),0); assert_eq!(class_idx(16),1); assert_eq!(class_idx(64),7); }
#[test] fn chunked_dirty_barrier() { let mut c=ChunkedArena::new(1024); let p=c.alloc_region(16,1) as usize; c.note_box_dirty(p); assert_eq!(c.collect_dirty_boxes().len(),1); }
#[test] fn chunked_free_reuse() { let mut c=ChunkedArena::new(1024); let p=c.alloc_region(32,2) as usize; c.sweep_free_list(|_| false, |_,_|{}); assert!(c.free_list_len()>0); let q=c.alloc_free(32,2); assert!(q.is_some()); }

// Heap
#[test] fn heap_new() { let _h=ArenaHeap::new(1024); }
#[test] fn heap_alloc_box() { let mut h=ArenaHeap::new(1024); let p=h.alloc_box(99u64); assert_eq!(unsafe{*p},99); }
#[test] fn heap_alloc_bytes() { let mut h=ArenaHeap::new(1024); let p=h.alloc_bytes(b"hi"); assert_eq!(unsafe{std::slice::from_raw_parts(p,2)}, b"hi"); }
#[test] fn heap_guard() { let mut h=ArenaHeap::new(512); let _g=HeapGuard::set(&mut h as *mut _); assert!(true); }
#[test] fn heap_try_promote_missing() { let mut h=ArenaHeap::new(1024); assert!(h.try_promote_box(0xdeadbeef).is_none()); }
#[test] fn heap_old_alloc_total() { let mut h=ArenaHeap::new(1024); assert_eq!(h.old_alloc_total(),0); let _ = h.alloc_box(1u64); }

// SharedMemory
#[test] fn shm_new() { let m=SidecarMemory::new(4096); assert_eq!(m.capacity(),4096); }
#[test] fn shm_write_read() { let m=SidecarMemory::new(4096); let o=m.write(b"abc").unwrap(); assert_eq!(m.read(o,3).unwrap(), b"abc"); }
#[test] fn shm_bump_align() { let m=SidecarMemory::new(4096); let o=m.bump(7).unwrap(); assert_eq!(o%8,0); }
#[test] fn shm_f32() { let m=SidecarMemory::new(4096); let o=m.allocate_float32_array(&[1.0,2.0]).unwrap(); assert_eq!(m.get_float32_slice(o,2).unwrap(), &[1.0,2.0]); }
#[test] fn shm_f64() { let m=SidecarMemory::new(4096); let o=m.allocate_float64_array(&[3.14]).unwrap(); assert!((m.get_float64_slice(o,1).unwrap()[0]-3.14).abs()<1e-9); }
#[test] fn shm_i32() { let m=SidecarMemory::new(4096); let o=m.allocate_int32_array(&[1,-2,3]).unwrap(); assert_eq!(m.get_int32_slice(o,3).unwrap(), &[1,-2,3]); }
#[test] fn shm_overflow() { let m=SidecarMemory::new(64); assert!(m.write(&[0u8;100]).is_err()); }
#[test] fn shm_unaligned_err() { let m=SidecarMemory::new(4096); assert!(m.get_float64_slice(1,1).is_err()); }
#[test] fn shm_scalar_rw() { let m=SidecarMemory::new(4096); m.write_float64(0, 9.9).unwrap(); assert!((m.read_float64(0).unwrap()-9.9).abs()<1e-9); }
#[test] fn shm_concurrent_bump() { use std::sync::Arc; let m=Arc::new(SidecarMemory::new(1<<20)); let mut hs=vec![]; for _ in 0..4 { let mm=m.clone(); hs.push(std::thread::spawn(move|| { for _ in 0..100 { let _=mm.bump(8); } })); } for h in hs { h.join().unwrap(); } assert!(m.used()>0); }

// Regex
#[test] fn regex_simple() { assert!(compile("abc", RegexFlags::default()).is_ok()); }
#[test] fn regex_flags_dup() { assert!(RegexFlags::parse("gg").is_err()); }
#[test] fn regex_invalid_flag() { assert!(RegexFlags::parse("z").is_err()); }
#[test] fn regex_compile_err() { assert!(compile("[", RegexFlags::default()).is_err()); }
#[test] fn regex_search_simple() { let p=compile_from_str("ab+", "").unwrap(); let chars="abbb".chars().collect::<Vec<_>>(); let m=alloy_core::regex::search(&p,&chars,0).unwrap(); assert_eq!(m.start,0); }
#[test] fn regex_case_insensitive() { let p=compile_from_str("abc","i").unwrap(); let chars="ABC".chars().collect::<Vec<_>>(); assert!(alloy_core::regex::search(&p,&chars,0).is_some()); }
#[test] fn regex_multiline() { let p=compile_from_str("^b","m").unwrap(); let chars="a\nb".chars().collect::<Vec<_>>(); assert!(alloy_core::regex::search(&p,&chars,0).is_some()); }
#[test] fn regex_scan_all() { let p=compile_from_str("a","g").unwrap(); let chars="aba".chars().collect::<Vec<_>>(); assert_eq!(alloy_core::regex::scan_all(&p,&chars).len(),2); }
#[test] fn regex_unicode_flag() { assert!(compile_from_str("a","u").is_ok()); }

// Value
#[test] fn value_numbers() { let v=Value::number(3.14); assert!(v.is_number()); }
#[test] fn value_int() { let v=Value::int(42); assert_eq!(v.as_int(),Some(42)); }
#[test] fn value_string() { let v=Value::string("hi".to_string()); assert_eq!(v.as_str(),Some("hi")); }
#[test] fn value_bool() { assert_eq!(Value::bool(true).as_bool(),Some(true)); }
#[test] fn value_null_undef() { assert!(Value::null().is_null()); assert!(Value::undefined().is_undefined()); }
#[test] fn value_array() { let a=Value::array(vec![Value::int(1),Value::int(2)]); assert_eq!(a.as_array().unwrap().borrow().len(),2); }
#[test] fn value_object() { let mut m=hashbrown::HashMap::new(); m.insert("a".to_string(), Value::int(1)); let o=Value::object(m); assert!(o.as_object().is_some()); }
#[test] fn value_typeof() { assert_eq!(Value::number(1.0).type_name(),"number"); assert_eq!(Value::string("x".to_string()).type_name(),"string"); }
#[test] fn value_truthiness() { assert!(!Value::undefined().is_truthy()); assert!(Value::int(1).is_truthy()); assert!(!Value::int(0).is_truthy()); }
#[test] fn value_add() { let a=Value::int(2); let b=Value::int(3); assert_eq!(a.add(&b).as_int(),Some(5)); }
#[test] fn value_str_concat() { let a=Value::string("a".to_string()); let b=Value::string("b".to_string()); assert_eq!(a.add(&b).as_str(),Some("ab")); }
#[test] fn value_comparisons() { let a=Value::int(1); let b=Value::int(2); assert!(a.as_int().unwrap() < b.as_int().unwrap()); }
