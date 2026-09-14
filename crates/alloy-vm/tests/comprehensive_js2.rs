use alloy_vm::compiler::Compiler;
use alloy_vm::vm::Vm;
fn run(src: &str) -> Vec<String> {
    let p = Compiler::compile_source(src).unwrap();
    let (mut vm, out) = Vm::with_output(p);
    vm.run();
    let v = out.lock().unwrap().clone();
    v
}
fn lines(src: &str) -> String {
    run(src).join("\n")
}
#[test]
fn math_0() {
    assert_eq!(lines("print(Math.abs(0))"), "0");
}
#[test]
fn math_1() {
    assert_eq!(lines("print(Math.abs(-1))"), "1");
}
#[test]
fn math_2() {
    assert_eq!(lines("print(Math.abs(-2))"), "2");
}
#[test]
fn math_3() {
    assert_eq!(lines("print(Math.abs(-3))"), "3");
}
#[test]
fn math_4() {
    assert_eq!(lines("print(Math.abs(-4))"), "4");
}
#[test]
fn math_5() {
    assert_eq!(lines("print(Math.abs(-5))"), "5");
}
#[test]
fn math_6() {
    assert_eq!(lines("print(Math.abs(-6))"), "6");
}
#[test]
fn math_7() {
    assert_eq!(lines("print(Math.abs(-7))"), "7");
}
#[test]
fn math_8() {
    assert_eq!(lines("print(Math.abs(-8))"), "8");
}
#[test]
fn math_9() {
    assert_eq!(lines("print(Math.abs(-9))"), "9");
}
#[test]
fn math_10() {
    assert_eq!(lines("print(Math.abs(-10))"), "10");
}
#[test]
fn math_11() {
    assert_eq!(lines("print(Math.abs(-11))"), "11");
}
#[test]
fn math_12() {
    assert_eq!(lines("print(Math.abs(-12))"), "12");
}
#[test]
fn math_13() {
    assert_eq!(lines("print(Math.abs(-13))"), "13");
}
#[test]
fn math_14() {
    assert_eq!(lines("print(Math.abs(-14))"), "14");
}
#[test]
fn math_15() {
    assert_eq!(lines("print(Math.abs(-15))"), "15");
}
#[test]
fn math_16() {
    assert_eq!(lines("print(Math.abs(-16))"), "16");
}
#[test]
fn math_17() {
    assert_eq!(lines("print(Math.abs(-17))"), "17");
}
#[test]
fn math_18() {
    assert_eq!(lines("print(Math.abs(-18))"), "18");
}
#[test]
fn math_19() {
    assert_eq!(lines("print(Math.abs(-19))"), "19");
}
#[test]
fn num_0() {
    assert_eq!(lines("print((0+0.5).toString())"), "0.5");
}
#[test]
fn num_1() {
    assert_eq!(lines("print((1+0.5).toString())"), "1.5");
}
#[test]
fn num_2() {
    assert_eq!(lines("print((2+0.5).toString())"), "2.5");
}
#[test]
fn num_3() {
    assert_eq!(lines("print((3+0.5).toString())"), "3.5");
}
#[test]
fn num_4() {
    assert_eq!(lines("print((4+0.5).toString())"), "4.5");
}
#[test]
fn num_5() {
    assert_eq!(lines("print((5+0.5).toString())"), "5.5");
}
#[test]
fn num_6() {
    assert_eq!(lines("print((6+0.5).toString())"), "6.5");
}
#[test]
fn num_7() {
    assert_eq!(lines("print((7+0.5).toString())"), "7.5");
}
#[test]
fn num_8() {
    assert_eq!(lines("print((8+0.5).toString())"), "8.5");
}
#[test]
fn num_9() {
    assert_eq!(lines("print((9+0.5).toString())"), "9.5");
}
#[test]
fn num_10() {
    assert_eq!(lines("print((10+0.5).toString())"), "10.5");
}
#[test]
fn num_11() {
    assert_eq!(lines("print((11+0.5).toString())"), "11.5");
}
#[test]
fn num_12() {
    assert_eq!(lines("print((12+0.5).toString())"), "12.5");
}
#[test]
fn num_13() {
    assert_eq!(lines("print((13+0.5).toString())"), "13.5");
}
#[test]
fn num_14() {
    assert_eq!(lines("print((14+0.5).toString())"), "14.5");
}
#[test]
fn num_15() {
    assert_eq!(lines("print((15+0.5).toString())"), "15.5");
}
#[test]
fn num_16() {
    assert_eq!(lines("print((16+0.5).toString())"), "16.5");
}
#[test]
fn num_17() {
    assert_eq!(lines("print((17+0.5).toString())"), "17.5");
}
#[test]
fn num_18() {
    assert_eq!(lines("print((18+0.5).toString())"), "18.5");
}
#[test]
fn num_19() {
    assert_eq!(lines("print((19+0.5).toString())"), "19.5");
}
#[test]
fn json_0() {
    assert_eq!(
        lines("const o={a:0}; print(JSON.stringify(o))"),
        "{\"a\":0}"
    );
}
#[test]
fn json_1() {
    assert_eq!(
        lines("const o={a:1}; print(JSON.stringify(o))"),
        "{\"a\":1}"
    );
}
#[test]
fn json_2() {
    assert_eq!(
        lines("const o={a:2}; print(JSON.stringify(o))"),
        "{\"a\":2}"
    );
}
#[test]
fn json_3() {
    assert_eq!(
        lines("const o={a:3}; print(JSON.stringify(o))"),
        "{\"a\":3}"
    );
}
#[test]
fn json_4() {
    assert_eq!(
        lines("const o={a:4}; print(JSON.stringify(o))"),
        "{\"a\":4}"
    );
}
#[test]
fn json_5() {
    assert_eq!(
        lines("const o={a:5}; print(JSON.stringify(o))"),
        "{\"a\":5}"
    );
}
#[test]
fn json_6() {
    assert_eq!(
        lines("const o={a:6}; print(JSON.stringify(o))"),
        "{\"a\":6}"
    );
}
#[test]
fn json_7() {
    assert_eq!(
        lines("const o={a:7}; print(JSON.stringify(o))"),
        "{\"a\":7}"
    );
}
#[test]
fn json_8() {
    assert_eq!(
        lines("const o={a:8}; print(JSON.stringify(o))"),
        "{\"a\":8}"
    );
}
#[test]
fn json_9() {
    assert_eq!(
        lines("const o={a:9}; print(JSON.stringify(o))"),
        "{\"a\":9}"
    );
}
#[test]
fn arr_map_0() {
    assert_eq!(
        lines("const a=[1,2,3]; print(a.map(x=>x*0).join(\",\"))"),
        "0,0,0"
    );
}
#[test]
fn arr_map_1() {
    assert_eq!(
        lines("const a=[1,2,3]; print(a.map(x=>x*1).join(\",\"))"),
        "1,2,3"
    );
}
#[test]
fn arr_map_2() {
    assert_eq!(
        lines("const a=[1,2,3]; print(a.map(x=>x*2).join(\",\"))"),
        "2,4,6"
    );
}
#[test]
fn arr_map_3() {
    assert_eq!(
        lines("const a=[1,2,3]; print(a.map(x=>x*3).join(\",\"))"),
        "3,6,9"
    );
}
#[test]
fn arr_map_4() {
    assert_eq!(
        lines("const a=[1,2,3]; print(a.map(x=>x*4).join(\",\"))"),
        "4,8,12"
    );
}
#[test]
fn arr_map_5() {
    assert_eq!(
        lines("const a=[1,2,3]; print(a.map(x=>x*5).join(\",\"))"),
        "5,10,15"
    );
}
#[test]
fn arr_map_6() {
    assert_eq!(
        lines("const a=[1,2,3]; print(a.map(x=>x*6).join(\",\"))"),
        "6,12,18"
    );
}
#[test]
fn arr_map_7() {
    assert_eq!(
        lines("const a=[1,2,3]; print(a.map(x=>x*7).join(\",\"))"),
        "7,14,21"
    );
}
#[test]
fn arr_map_8() {
    assert_eq!(
        lines("const a=[1,2,3]; print(a.map(x=>x*8).join(\",\"))"),
        "8,16,24"
    );
}
#[test]
fn arr_map_9() {
    assert_eq!(
        lines("const a=[1,2,3]; print(a.map(x=>x*9).join(\",\"))"),
        "9,18,27"
    );
}
#[test]
fn arr_map_10() {
    assert_eq!(
        lines("const a=[1,2,3]; print(a.map(x=>x*10).join(\",\"))"),
        "10,20,30"
    );
}
#[test]
fn arr_map_11() {
    assert_eq!(
        lines("const a=[1,2,3]; print(a.map(x=>x*11).join(\",\"))"),
        "11,22,33"
    );
}
#[test]
fn arr_map_12() {
    assert_eq!(
        lines("const a=[1,2,3]; print(a.map(x=>x*12).join(\",\"))"),
        "12,24,36"
    );
}
#[test]
fn arr_map_13() {
    assert_eq!(
        lines("const a=[1,2,3]; print(a.map(x=>x*13).join(\",\"))"),
        "13,26,39"
    );
}
#[test]
fn arr_map_14() {
    assert_eq!(
        lines("const a=[1,2,3]; print(a.map(x=>x*14).join(\",\"))"),
        "14,28,42"
    );
}
#[test]
fn arr_map_15() {
    assert_eq!(
        lines("const a=[1,2,3]; print(a.map(x=>x*15).join(\",\"))"),
        "15,30,45"
    );
}
#[test]
fn arr_map_16() {
    assert_eq!(
        lines("const a=[1,2,3]; print(a.map(x=>x*16).join(\",\"))"),
        "16,32,48"
    );
}
#[test]
fn arr_map_17() {
    assert_eq!(
        lines("const a=[1,2,3]; print(a.map(x=>x*17).join(\",\"))"),
        "17,34,51"
    );
}
#[test]
fn arr_map_18() {
    assert_eq!(
        lines("const a=[1,2,3]; print(a.map(x=>x*18).join(\",\"))"),
        "18,36,54"
    );
}
#[test]
fn arr_map_19() {
    assert_eq!(
        lines("const a=[1,2,3]; print(a.map(x=>x*19).join(\",\"))"),
        "19,38,57"
    );
}
#[test]
fn arr_filter_0() {
    assert_eq!(
        lines("const a=[1,2,3,4,5]; print(a.filter(x=>x>0).length)"),
        "5"
    );
}
#[test]
fn arr_filter_1() {
    assert_eq!(
        lines("const a=[1,2,3,4,5]; print(a.filter(x=>x>1).length)"),
        "4"
    );
}
#[test]
fn arr_filter_2() {
    assert_eq!(
        lines("const a=[1,2,3,4,5]; print(a.filter(x=>x>2).length)"),
        "3"
    );
}
#[test]
fn arr_filter_3() {
    assert_eq!(
        lines("const a=[1,2,3,4,5]; print(a.filter(x=>x>3).length)"),
        "2"
    );
}
#[test]
fn arr_filter_4() {
    assert_eq!(
        lines("const a=[1,2,3,4,5]; print(a.filter(x=>x>4).length)"),
        "1"
    );
}
#[test]
fn arr_filter_5() {
    assert_eq!(
        lines("const a=[1,2,3,4,5]; print(a.filter(x=>x>0).length)"),
        "5"
    );
}
#[test]
fn arr_filter_6() {
    assert_eq!(
        lines("const a=[1,2,3,4,5]; print(a.filter(x=>x>1).length)"),
        "4"
    );
}
#[test]
fn arr_filter_7() {
    assert_eq!(
        lines("const a=[1,2,3,4,5]; print(a.filter(x=>x>2).length)"),
        "3"
    );
}
#[test]
fn arr_filter_8() {
    assert_eq!(
        lines("const a=[1,2,3,4,5]; print(a.filter(x=>x>3).length)"),
        "2"
    );
}
#[test]
fn arr_filter_9() {
    assert_eq!(
        lines("const a=[1,2,3,4,5]; print(a.filter(x=>x>4).length)"),
        "1"
    );
}
#[test]
fn arr_filter_10() {
    assert_eq!(
        lines("const a=[1,2,3,4,5]; print(a.filter(x=>x>0).length)"),
        "5"
    );
}
#[test]
fn arr_filter_11() {
    assert_eq!(
        lines("const a=[1,2,3,4,5]; print(a.filter(x=>x>1).length)"),
        "4"
    );
}
#[test]
fn arr_filter_12() {
    assert_eq!(
        lines("const a=[1,2,3,4,5]; print(a.filter(x=>x>2).length)"),
        "3"
    );
}
#[test]
fn arr_filter_13() {
    assert_eq!(
        lines("const a=[1,2,3,4,5]; print(a.filter(x=>x>3).length)"),
        "2"
    );
}
#[test]
fn arr_filter_14() {
    assert_eq!(
        lines("const a=[1,2,3,4,5]; print(a.filter(x=>x>4).length)"),
        "1"
    );
}
#[test]
fn arr_filter_15() {
    assert_eq!(
        lines("const a=[1,2,3,4,5]; print(a.filter(x=>x>0).length)"),
        "5"
    );
}
#[test]
fn arr_filter_16() {
    assert_eq!(
        lines("const a=[1,2,3,4,5]; print(a.filter(x=>x>1).length)"),
        "4"
    );
}
#[test]
fn arr_filter_17() {
    assert_eq!(
        lines("const a=[1,2,3,4,5]; print(a.filter(x=>x>2).length)"),
        "3"
    );
}
#[test]
fn arr_filter_18() {
    assert_eq!(
        lines("const a=[1,2,3,4,5]; print(a.filter(x=>x>3).length)"),
        "2"
    );
}
#[test]
fn arr_filter_19() {
    assert_eq!(
        lines("const a=[1,2,3,4,5]; print(a.filter(x=>x>4).length)"),
        "1"
    );
}
#[test]
fn str_slice_0() {
    assert_eq!(lines("print(\"hello\".slice(0,3))"), "hel");
}
#[test]
fn str_slice_1() {
    assert_eq!(lines("print(\"hello\".slice(1,4))"), "ell");
}
#[test]
fn str_slice_2() {
    assert_eq!(lines("print(\"hello\".slice(2,5))"), "llo");
}
#[test]
fn str_slice_3() {
    assert_eq!(lines("print(\"hello\".slice(0,3))"), "hel");
}
#[test]
fn str_slice_4() {
    assert_eq!(lines("print(\"hello\".slice(1,4))"), "ell");
}
#[test]
fn str_slice_5() {
    assert_eq!(lines("print(\"hello\".slice(2,5))"), "llo");
}
#[test]
fn str_slice_6() {
    assert_eq!(lines("print(\"hello\".slice(0,3))"), "hel");
}
#[test]
fn str_slice_7() {
    assert_eq!(lines("print(\"hello\".slice(1,4))"), "ell");
}
#[test]
fn str_slice_8() {
    assert_eq!(lines("print(\"hello\".slice(2,5))"), "llo");
}
#[test]
fn str_slice_9() {
    assert_eq!(lines("print(\"hello\".slice(0,3))"), "hel");
}
#[test]
fn str_slice_10() {
    assert_eq!(lines("print(\"hello\".slice(1,4))"), "ell");
}
#[test]
fn str_slice_11() {
    assert_eq!(lines("print(\"hello\".slice(2,5))"), "llo");
}
#[test]
fn str_slice_12() {
    assert_eq!(lines("print(\"hello\".slice(0,3))"), "hel");
}
#[test]
fn str_slice_13() {
    assert_eq!(lines("print(\"hello\".slice(1,4))"), "ell");
}
#[test]
fn str_slice_14() {
    assert_eq!(lines("print(\"hello\".slice(2,5))"), "llo");
}
#[test]
fn keys_0() {
    assert_eq!(
        lines("const o={a:0,b:1}; print(Object.keys(o).length)"),
        "2"
    );
}
#[test]
fn keys_1() {
    assert_eq!(
        lines("const o={a:1,b:2}; print(Object.keys(o).length)"),
        "2"
    );
}
#[test]
fn keys_2() {
    assert_eq!(
        lines("const o={a:2,b:3}; print(Object.keys(o).length)"),
        "2"
    );
}
#[test]
fn keys_3() {
    assert_eq!(
        lines("const o={a:3,b:4}; print(Object.keys(o).length)"),
        "2"
    );
}
#[test]
fn keys_4() {
    assert_eq!(
        lines("const o={a:4,b:5}; print(Object.keys(o).length)"),
        "2"
    );
}
#[test]
fn keys_5() {
    assert_eq!(
        lines("const o={a:5,b:6}; print(Object.keys(o).length)"),
        "2"
    );
}
#[test]
fn keys_6() {
    assert_eq!(
        lines("const o={a:6,b:7}; print(Object.keys(o).length)"),
        "2"
    );
}
#[test]
fn keys_7() {
    assert_eq!(
        lines("const o={a:7,b:8}; print(Object.keys(o).length)"),
        "2"
    );
}
#[test]
fn keys_8() {
    assert_eq!(
        lines("const o={a:8,b:9}; print(Object.keys(o).length)"),
        "2"
    );
}
#[test]
fn keys_9() {
    assert_eq!(
        lines("const o={a:9,b:10}; print(Object.keys(o).length)"),
        "2"
    );
}
#[test]
fn fib10() {
    assert_eq!(
        lines("function fib(n){if(n<=1)return n; return fib(n-1)+fib(n-2)} print(fib(7))"),
        "13"
    );
}
#[test]
fn fact5() {
    assert_eq!(
        lines("function fact(n){if(n<=1)return 1; return n*fact(n-1)} print(fact(5))"),
        "120"
    );
}
#[test]
fn promise_0() {
    assert_eq!(lines("print(typeof Promise.resolve(0))"), "object");
}
#[test]
fn promise_1() {
    assert_eq!(lines("print(typeof Promise.resolve(1))"), "object");
}
#[test]
fn promise_2() {
    assert_eq!(lines("print(typeof Promise.resolve(2))"), "object");
}
#[test]
fn promise_3() {
    assert_eq!(lines("print(typeof Promise.resolve(3))"), "object");
}
#[test]
fn promise_4() {
    assert_eq!(lines("print(typeof Promise.resolve(4))"), "object");
}
#[test]
fn promise_5() {
    assert_eq!(lines("print(typeof Promise.resolve(5))"), "object");
}
#[test]
fn promise_6() {
    assert_eq!(lines("print(typeof Promise.resolve(6))"), "object");
}
#[test]
fn promise_7() {
    assert_eq!(lines("print(typeof Promise.resolve(7))"), "object");
}
#[test]
fn promise_8() {
    assert_eq!(lines("print(typeof Promise.resolve(8))"), "object");
}
#[test]
fn promise_9() {
    assert_eq!(lines("print(typeof Promise.resolve(9))"), "object");
}
#[test]
fn while_0() {
    assert_eq!(
        lines("let i=0; let s=0; while(i<3){s=s+i; i=i+1} print(s)"),
        "3"
    );
}
#[test]
fn while_1() {
    assert_eq!(
        lines("let i=0; let s=0; while(i<4){s=s+i; i=i+1} print(s)"),
        "6"
    );
}
#[test]
fn while_2() {
    assert_eq!(
        lines("let i=0; let s=0; while(i<5){s=s+i; i=i+1} print(s)"),
        "10"
    );
}
#[test]
fn while_3() {
    assert_eq!(
        lines("let i=0; let s=0; while(i<6){s=s+i; i=i+1} print(s)"),
        "15"
    );
}
#[test]
fn while_4() {
    assert_eq!(
        lines("let i=0; let s=0; while(i<7){s=s+i; i=i+1} print(s)"),
        "21"
    );
}
#[test]
fn while_5() {
    assert_eq!(
        lines("let i=0; let s=0; while(i<8){s=s+i; i=i+1} print(s)"),
        "28"
    );
}
#[test]
fn while_6() {
    assert_eq!(
        lines("let i=0; let s=0; while(i<9){s=s+i; i=i+1} print(s)"),
        "36"
    );
}
#[test]
fn while_7() {
    assert_eq!(
        lines("let i=0; let s=0; while(i<10){s=s+i; i=i+1} print(s)"),
        "45"
    );
}
#[test]
fn while_8() {
    assert_eq!(
        lines("let i=0; let s=0; while(i<11){s=s+i; i=i+1} print(s)"),
        "55"
    );
}
#[test]
fn while_9() {
    assert_eq!(
        lines("let i=0; let s=0; while(i<12){s=s+i; i=i+1} print(s)"),
        "66"
    );
}
#[test]
fn while_10() {
    assert_eq!(
        lines("let i=0; let s=0; while(i<13){s=s+i; i=i+1} print(s)"),
        "78"
    );
}
#[test]
fn while_11() {
    assert_eq!(
        lines("let i=0; let s=0; while(i<14){s=s+i; i=i+1} print(s)"),
        "91"
    );
}
#[test]
fn while_12() {
    assert_eq!(
        lines("let i=0; let s=0; while(i<15){s=s+i; i=i+1} print(s)"),
        "105"
    );
}
#[test]
fn while_13() {
    assert_eq!(
        lines("let i=0; let s=0; while(i<16){s=s+i; i=i+1} print(s)"),
        "120"
    );
}
#[test]
fn while_14() {
    assert_eq!(
        lines("let i=0; let s=0; while(i<17){s=s+i; i=i+1} print(s)"),
        "136"
    );
}
#[test]
fn bit_0() {
    assert_eq!(lines("print(0 & 1)"), "0");
}
#[test]
fn bit_or_0() {
    assert_eq!(lines("print(0 | 1)"), "1");
}
#[test]
fn bit_1() {
    assert_eq!(lines("print(1 & 2)"), "0");
}
#[test]
fn bit_or_1() {
    assert_eq!(lines("print(1 | 2)"), "3");
}
#[test]
fn bit_2() {
    assert_eq!(lines("print(2 & 3)"), "2");
}
#[test]
fn bit_or_2() {
    assert_eq!(lines("print(2 | 3)"), "3");
}
#[test]
fn bit_3() {
    assert_eq!(lines("print(3 & 4)"), "0");
}
#[test]
fn bit_or_3() {
    assert_eq!(lines("print(3 | 4)"), "7");
}
#[test]
fn bit_4() {
    assert_eq!(lines("print(4 & 5)"), "4");
}
#[test]
fn bit_or_4() {
    assert_eq!(lines("print(4 | 5)"), "5");
}
#[test]
fn bit_5() {
    assert_eq!(lines("print(5 & 6)"), "4");
}
#[test]
fn bit_or_5() {
    assert_eq!(lines("print(5 | 6)"), "7");
}
#[test]
fn bit_6() {
    assert_eq!(lines("print(6 & 7)"), "6");
}
#[test]
fn bit_or_6() {
    assert_eq!(lines("print(6 | 7)"), "7");
}
#[test]
fn bit_7() {
    assert_eq!(lines("print(7 & 8)"), "0");
}
#[test]
fn bit_or_7() {
    assert_eq!(lines("print(7 | 8)"), "15");
}
#[test]
fn bit_8() {
    assert_eq!(lines("print(8 & 9)"), "8");
}
#[test]
fn bit_or_8() {
    assert_eq!(lines("print(8 | 9)"), "9");
}
#[test]
fn bit_9() {
    assert_eq!(lines("print(9 & 10)"), "8");
}
#[test]
fn bit_or_9() {
    assert_eq!(lines("print(9 | 10)"), "11");
}
#[test]
fn bit_10() {
    assert_eq!(lines("print(10 & 11)"), "10");
}
#[test]
fn bit_or_10() {
    assert_eq!(lines("print(10 | 11)"), "11");
}
#[test]
fn bit_11() {
    assert_eq!(lines("print(11 & 12)"), "8");
}
#[test]
fn bit_or_11() {
    assert_eq!(lines("print(11 | 12)"), "15");
}
#[test]
fn bit_12() {
    assert_eq!(lines("print(12 & 13)"), "12");
}
#[test]
fn bit_or_12() {
    assert_eq!(lines("print(12 | 13)"), "13");
}
#[test]
fn bit_13() {
    assert_eq!(lines("print(13 & 14)"), "12");
}
#[test]
fn bit_or_13() {
    assert_eq!(lines("print(13 | 14)"), "15");
}
#[test]
fn bit_14() {
    assert_eq!(lines("print(14 & 15)"), "14");
}
#[test]
fn bit_or_14() {
    assert_eq!(lines("print(14 | 15)"), "15");
}
#[test]
fn inst_0() {
    assert_eq!(
        lines("class A{} const a=new A(); print(a instanceof A)"),
        "true"
    );
}
#[test]
fn inst_1() {
    assert_eq!(
        lines("class A{} const a=new A(); print(a instanceof A)"),
        "true"
    );
}
#[test]
fn inst_2() {
    assert_eq!(
        lines("class A{} const a=new A(); print(a instanceof A)"),
        "true"
    );
}
#[test]
fn inst_3() {
    assert_eq!(
        lines("class A{} const a=new A(); print(a instanceof A)"),
        "true"
    );
}
#[test]
fn inst_4() {
    assert_eq!(
        lines("class A{} const a=new A(); print(a instanceof A)"),
        "true"
    );
}
#[test]
fn try_0() {
    assert_eq!(lines("try{throw 0}catch(e){print(e)}"), "0");
}
#[test]
fn try_1() {
    assert_eq!(lines("try{throw 1}catch(e){print(e)}"), "1");
}
#[test]
fn try_2() {
    assert_eq!(lines("try{throw 2}catch(e){print(e)}"), "2");
}
#[test]
fn try_3() {
    assert_eq!(lines("try{throw 3}catch(e){print(e)}"), "3");
}
#[test]
fn try_4() {
    assert_eq!(lines("try{throw 4}catch(e){print(e)}"), "4");
}
#[test]
fn try_5() {
    assert_eq!(lines("try{throw 5}catch(e){print(e)}"), "5");
}
#[test]
fn try_6() {
    assert_eq!(lines("try{throw 6}catch(e){print(e)}"), "6");
}
#[test]
fn try_7() {
    assert_eq!(lines("try{throw 7}catch(e){print(e)}"), "7");
}
#[test]
fn try_8() {
    assert_eq!(lines("try{throw 8}catch(e){print(e)}"), "8");
}
#[test]
fn try_9() {
    assert_eq!(lines("try{throw 9}catch(e){print(e)}"), "9");
}
