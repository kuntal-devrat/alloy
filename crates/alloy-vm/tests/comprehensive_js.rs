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
    run(src).join(
        "
",
    )
}
#[test]
fn arith_0() {
    assert_eq!(lines("print(0 + 0)"), "0");
}
#[test]
fn arith_1() {
    assert_eq!(lines("print(1 + 2)"), "3");
}
#[test]
fn arith_2() {
    assert_eq!(lines("print(2 + 4)"), "6");
}
#[test]
fn arith_3() {
    assert_eq!(lines("print(3 + 6)"), "9");
}
#[test]
fn arith_4() {
    assert_eq!(lines("print(4 + 8)"), "12");
}
#[test]
fn arith_5() {
    assert_eq!(lines("print(5 + 10)"), "15");
}
#[test]
fn arith_6() {
    assert_eq!(lines("print(6 + 12)"), "18");
}
#[test]
fn arith_7() {
    assert_eq!(lines("print(7 + 14)"), "21");
}
#[test]
fn arith_8() {
    assert_eq!(lines("print(8 + 16)"), "24");
}
#[test]
fn arith_9() {
    assert_eq!(lines("print(9 + 18)"), "27");
}
#[test]
fn arith_10() {
    assert_eq!(lines("print(10 + 20)"), "30");
}
#[test]
fn arith_11() {
    assert_eq!(lines("print(11 + 22)"), "33");
}
#[test]
fn arith_12() {
    assert_eq!(lines("print(12 + 24)"), "36");
}
#[test]
fn arith_13() {
    assert_eq!(lines("print(13 + 26)"), "39");
}
#[test]
fn arith_14() {
    assert_eq!(lines("print(14 + 28)"), "42");
}
#[test]
fn arith_15() {
    assert_eq!(lines("print(15 + 30)"), "45");
}
#[test]
fn arith_16() {
    assert_eq!(lines("print(16 + 32)"), "48");
}
#[test]
fn arith_17() {
    assert_eq!(lines("print(17 + 34)"), "51");
}
#[test]
fn arith_18() {
    assert_eq!(lines("print(18 + 36)"), "54");
}
#[test]
fn arith_19() {
    assert_eq!(lines("print(19 + 38)"), "57");
}
#[test]
fn arith_20() {
    assert_eq!(lines("print(20 + 40)"), "60");
}
#[test]
fn arith_21() {
    assert_eq!(lines("print(21 + 42)"), "63");
}
#[test]
fn arith_22() {
    assert_eq!(lines("print(22 + 44)"), "66");
}
#[test]
fn arith_23() {
    assert_eq!(lines("print(23 + 46)"), "69");
}
#[test]
fn arith_24() {
    assert_eq!(lines("print(24 + 48)"), "72");
}
#[test]
fn arith_25() {
    assert_eq!(lines("print(25 + 50)"), "75");
}
#[test]
fn arith_26() {
    assert_eq!(lines("print(26 + 52)"), "78");
}
#[test]
fn arith_27() {
    assert_eq!(lines("print(27 + 54)"), "81");
}
#[test]
fn arith_28() {
    assert_eq!(lines("print(28 + 56)"), "84");
}
#[test]
fn arith_29() {
    assert_eq!(lines("print(29 + 58)"), "87");
}
#[test]
fn str_concat_0() {
    assert_eq!(lines("print(\"a0\" + \"b0\")"), "a0b0");
}
#[test]
fn str_concat_1() {
    assert_eq!(lines("print(\"a1\" + \"b1\")"), "a1b1");
}
#[test]
fn str_concat_2() {
    assert_eq!(lines("print(\"a2\" + \"b2\")"), "a2b2");
}
#[test]
fn str_concat_3() {
    assert_eq!(lines("print(\"a3\" + \"b3\")"), "a3b3");
}
#[test]
fn str_concat_4() {
    assert_eq!(lines("print(\"a4\" + \"b4\")"), "a4b4");
}
#[test]
fn str_concat_5() {
    assert_eq!(lines("print(\"a5\" + \"b5\")"), "a5b5");
}
#[test]
fn str_concat_6() {
    assert_eq!(lines("print(\"a6\" + \"b6\")"), "a6b6");
}
#[test]
fn str_concat_7() {
    assert_eq!(lines("print(\"a7\" + \"b7\")"), "a7b7");
}
#[test]
fn str_concat_8() {
    assert_eq!(lines("print(\"a8\" + \"b8\")"), "a8b8");
}
#[test]
fn str_concat_9() {
    assert_eq!(lines("print(\"a9\" + \"b9\")"), "a9b9");
}
#[test]
fn arr_len_0() {
    assert_eq!(lines("const a=[0,1,2]; print(a.length)"), "3");
}
#[test]
fn arr_len_1() {
    assert_eq!(lines("const a=[0,1,2,3]; print(a.length)"), "4");
}
#[test]
fn arr_len_2() {
    assert_eq!(lines("const a=[0,1,2,3,4]; print(a.length)"), "5");
}
#[test]
fn arr_len_3() {
    assert_eq!(lines("const a=[0,1,2,3,4,5]; print(a.length)"), "6");
}
#[test]
fn arr_len_4() {
    assert_eq!(lines("const a=[0,1,2,3,4,5,6]; print(a.length)"), "7");
}
#[test]
fn arr_len_5() {
    assert_eq!(lines("const a=[0,1,2,3,4,5,6,7]; print(a.length)"), "8");
}
#[test]
fn arr_len_6() {
    assert_eq!(lines("const a=[0,1,2,3,4,5,6,7,8]; print(a.length)"), "9");
}
#[test]
fn arr_len_7() {
    assert_eq!(
        lines("const a=[0,1,2,3,4,5,6,7,8,9]; print(a.length)"),
        "10"
    );
}
#[test]
fn arr_len_8() {
    assert_eq!(
        lines("const a=[0,1,2,3,4,5,6,7,8,9,10]; print(a.length)"),
        "11"
    );
}
#[test]
fn arr_len_9() {
    assert_eq!(
        lines("const a=[0,1,2,3,4,5,6,7,8,9,10,11]; print(a.length)"),
        "12"
    );
}
#[test]
fn obj_prop_0() {
    assert_eq!(lines("const o={x:0}; print(o.x)"), "0");
}
#[test]
fn obj_prop_1() {
    assert_eq!(lines("const o={x:1}; print(o.x)"), "1");
}
#[test]
fn obj_prop_2() {
    assert_eq!(lines("const o={x:2}; print(o.x)"), "2");
}
#[test]
fn obj_prop_3() {
    assert_eq!(lines("const o={x:3}; print(o.x)"), "3");
}
#[test]
fn obj_prop_4() {
    assert_eq!(lines("const o={x:4}; print(o.x)"), "4");
}
#[test]
fn obj_prop_5() {
    assert_eq!(lines("const o={x:5}; print(o.x)"), "5");
}
#[test]
fn obj_prop_6() {
    assert_eq!(lines("const o={x:6}; print(o.x)"), "6");
}
#[test]
fn obj_prop_7() {
    assert_eq!(lines("const o={x:7}; print(o.x)"), "7");
}
#[test]
fn obj_prop_8() {
    assert_eq!(lines("const o={x:8}; print(o.x)"), "8");
}
#[test]
fn obj_prop_9() {
    assert_eq!(lines("const o={x:9}; print(o.x)"), "9");
}
#[test]
fn loop_sum_0() {
    assert_eq!(
        lines("let s=0; for(let i=0;i<5;i=i+1){s=s+i} print(s)"),
        "10"
    );
}
#[test]
fn loop_sum_1() {
    assert_eq!(
        lines("let s=0; for(let i=0;i<6;i=i+1){s=s+i} print(s)"),
        "15"
    );
}
#[test]
fn loop_sum_2() {
    assert_eq!(
        lines("let s=0; for(let i=0;i<7;i=i+1){s=s+i} print(s)"),
        "21"
    );
}
#[test]
fn loop_sum_3() {
    assert_eq!(
        lines("let s=0; for(let i=0;i<8;i=i+1){s=s+i} print(s)"),
        "28"
    );
}
#[test]
fn loop_sum_4() {
    assert_eq!(
        lines("let s=0; for(let i=0;i<9;i=i+1){s=s+i} print(s)"),
        "36"
    );
}
#[test]
fn loop_sum_5() {
    assert_eq!(
        lines("let s=0; for(let i=0;i<10;i=i+1){s=s+i} print(s)"),
        "45"
    );
}
#[test]
fn loop_sum_6() {
    assert_eq!(
        lines("let s=0; for(let i=0;i<11;i=i+1){s=s+i} print(s)"),
        "55"
    );
}
#[test]
fn loop_sum_7() {
    assert_eq!(
        lines("let s=0; for(let i=0;i<12;i=i+1){s=s+i} print(s)"),
        "66"
    );
}
#[test]
fn loop_sum_8() {
    assert_eq!(
        lines("let s=0; for(let i=0;i<13;i=i+1){s=s+i} print(s)"),
        "78"
    );
}
#[test]
fn loop_sum_9() {
    assert_eq!(
        lines("let s=0; for(let i=0;i<14;i=i+1){s=s+i} print(s)"),
        "91"
    );
}
#[test]
fn closure_simple() {
    assert_eq!(
        lines("function f(x){return function(y){return x+y}} print(f(5)(3))"),
        "8"
    );
}
#[test]
fn closure_counter() {
    assert_eq!(
        lines(
            "function c(){let n=0; return function(){n=n+1; return n}} const a=c(); print(a(),a())"
        ),
        "1 2"
    );
}
#[test]
fn async_simple() {
    assert_eq!(
        lines("async function g(){return 21} async function f(){print(await g())} f()"),
        "21"
    );
}
#[test]
fn tpl_0() {
    assert_eq!(lines("print(`val 0 = ${0}`)"), "val 0 = 0");
}
#[test]
fn tpl_1() {
    assert_eq!(lines("print(`val 1 = ${2}`)"), "val 1 = 2");
}
#[test]
fn tpl_2() {
    assert_eq!(lines("print(`val 2 = ${4}`)"), "val 2 = 4");
}
#[test]
fn tpl_3() {
    assert_eq!(lines("print(`val 3 = ${6}`)"), "val 3 = 6");
}
#[test]
fn tpl_4() {
    assert_eq!(lines("print(`val 4 = ${8}`)"), "val 4 = 8");
}
#[test]
fn bulk_0() {
    assert_eq!(lines("print(0*0+0)"), "0");
}
#[test]
fn bulk_1() {
    assert_eq!(lines("print(1*3+1)"), "4");
}
#[test]
fn bulk_2() {
    assert_eq!(lines("print(2*6+2)"), "14");
}
#[test]
fn bulk_3() {
    assert_eq!(lines("print(3*9+3)"), "30");
}
#[test]
fn bulk_4() {
    assert_eq!(lines("print(4*1+4)"), "8");
}
#[test]
fn bulk_5() {
    assert_eq!(lines("print(5*4+5)"), "25");
}
#[test]
fn bulk_6() {
    assert_eq!(lines("print(6*7+6)"), "48");
}
#[test]
fn bulk_7() {
    assert_eq!(lines("print(0*10+7)"), "7");
}
#[test]
fn bulk_8() {
    assert_eq!(lines("print(1*2+8)"), "10");
}
#[test]
fn bulk_9() {
    assert_eq!(lines("print(2*5+9)"), "19");
}
#[test]
fn bulk_10() {
    assert_eq!(lines("print(3*8+10)"), "34");
}
#[test]
fn bulk_11() {
    assert_eq!(lines("print(4*0+11)"), "11");
}
#[test]
fn bulk_12() {
    assert_eq!(lines("print(5*3+12)"), "27");
}
#[test]
fn bulk_13() {
    assert_eq!(lines("print(6*6+13)"), "49");
}
#[test]
fn bulk_14() {
    assert_eq!(lines("print(0*9+14)"), "14");
}
#[test]
fn bulk_15() {
    assert_eq!(lines("print(1*1+15)"), "16");
}
#[test]
fn bulk_16() {
    assert_eq!(lines("print(2*4+16)"), "24");
}
#[test]
fn bulk_17() {
    assert_eq!(lines("print(3*7+17)"), "38");
}
#[test]
fn bulk_18() {
    assert_eq!(lines("print(4*10+18)"), "58");
}
#[test]
fn bulk_19() {
    assert_eq!(lines("print(5*2+19)"), "29");
}
#[test]
fn bulk_20() {
    assert_eq!(lines("print(6*5+20)"), "50");
}
#[test]
fn bulk_21() {
    assert_eq!(lines("print(0*8+21)"), "21");
}
#[test]
fn bulk_22() {
    assert_eq!(lines("print(1*0+22)"), "22");
}
#[test]
fn bulk_23() {
    assert_eq!(lines("print(2*3+23)"), "29");
}
#[test]
fn bulk_24() {
    assert_eq!(lines("print(3*6+24)"), "42");
}
#[test]
fn bulk_25() {
    assert_eq!(lines("print(4*9+25)"), "61");
}
#[test]
fn bulk_26() {
    assert_eq!(lines("print(5*1+26)"), "31");
}
#[test]
fn bulk_27() {
    assert_eq!(lines("print(6*4+27)"), "51");
}
#[test]
fn bulk_28() {
    assert_eq!(lines("print(0*7+28)"), "28");
}
#[test]
fn bulk_29() {
    assert_eq!(lines("print(1*10+29)"), "39");
}
#[test]
fn bulk_30() {
    assert_eq!(lines("print(2*2+30)"), "34");
}
#[test]
fn bulk_31() {
    assert_eq!(lines("print(3*5+31)"), "46");
}
#[test]
fn bulk_32() {
    assert_eq!(lines("print(4*8+32)"), "64");
}
#[test]
fn bulk_33() {
    assert_eq!(lines("print(5*0+33)"), "33");
}
#[test]
fn bulk_34() {
    assert_eq!(lines("print(6*3+34)"), "52");
}
#[test]
fn bulk_35() {
    assert_eq!(lines("print(0*6+35)"), "35");
}
#[test]
fn bulk_36() {
    assert_eq!(lines("print(1*9+36)"), "45");
}
#[test]
fn bulk_37() {
    assert_eq!(lines("print(2*1+37)"), "39");
}
#[test]
fn bulk_38() {
    assert_eq!(lines("print(3*4+38)"), "50");
}
#[test]
fn bulk_39() {
    assert_eq!(lines("print(4*7+39)"), "67");
}
#[test]
fn bulk_40() {
    assert_eq!(lines("print(5*10+40)"), "90");
}
#[test]
fn bulk_41() {
    assert_eq!(lines("print(6*2+41)"), "53");
}
#[test]
fn bulk_42() {
    assert_eq!(lines("print(0*5+42)"), "42");
}
#[test]
fn bulk_43() {
    assert_eq!(lines("print(1*8+43)"), "51");
}
#[test]
fn bulk_44() {
    assert_eq!(lines("print(2*0+44)"), "44");
}
#[test]
fn bulk_45() {
    assert_eq!(lines("print(3*3+45)"), "54");
}
#[test]
fn bulk_46() {
    assert_eq!(lines("print(4*6+46)"), "70");
}
#[test]
fn bulk_47() {
    assert_eq!(lines("print(5*9+47)"), "92");
}
#[test]
fn bulk_48() {
    assert_eq!(lines("print(6*1+48)"), "54");
}
#[test]
fn bulk_49() {
    assert_eq!(lines("print(0*4+49)"), "49");
}
#[test]
fn bulk_50() {
    assert_eq!(lines("print(1*7+50)"), "57");
}
#[test]
fn bulk_51() {
    assert_eq!(lines("print(2*10+51)"), "71");
}
#[test]
fn bulk_52() {
    assert_eq!(lines("print(3*2+52)"), "58");
}
#[test]
fn bulk_53() {
    assert_eq!(lines("print(4*5+53)"), "73");
}
#[test]
fn bulk_54() {
    assert_eq!(lines("print(5*8+54)"), "94");
}
#[test]
fn bulk_55() {
    assert_eq!(lines("print(6*0+55)"), "55");
}
#[test]
fn bulk_56() {
    assert_eq!(lines("print(0*3+56)"), "56");
}
#[test]
fn bulk_57() {
    assert_eq!(lines("print(1*6+57)"), "63");
}
#[test]
fn bulk_58() {
    assert_eq!(lines("print(2*9+58)"), "76");
}
#[test]
fn bulk_59() {
    assert_eq!(lines("print(3*1+59)"), "62");
}
#[test]
fn bulk_60() {
    assert_eq!(lines("print(4*4+60)"), "76");
}
#[test]
fn bulk_61() {
    assert_eq!(lines("print(5*7+61)"), "96");
}
#[test]
fn bulk_62() {
    assert_eq!(lines("print(6*10+62)"), "122");
}
#[test]
fn bulk_63() {
    assert_eq!(lines("print(0*2+63)"), "63");
}
#[test]
fn bulk_64() {
    assert_eq!(lines("print(1*5+64)"), "69");
}
#[test]
fn bulk_65() {
    assert_eq!(lines("print(2*8+65)"), "81");
}
#[test]
fn bulk_66() {
    assert_eq!(lines("print(3*0+66)"), "66");
}
#[test]
fn bulk_67() {
    assert_eq!(lines("print(4*3+67)"), "79");
}
#[test]
fn bulk_68() {
    assert_eq!(lines("print(5*6+68)"), "98");
}
#[test]
fn bulk_69() {
    assert_eq!(lines("print(6*9+69)"), "123");
}
#[test]
fn bulk_70() {
    assert_eq!(lines("print(0*1+70)"), "70");
}
#[test]
fn bulk_71() {
    assert_eq!(lines("print(1*4+71)"), "75");
}
#[test]
fn bulk_72() {
    assert_eq!(lines("print(2*7+72)"), "86");
}
#[test]
fn bulk_73() {
    assert_eq!(lines("print(3*10+73)"), "103");
}
#[test]
fn bulk_74() {
    assert_eq!(lines("print(4*2+74)"), "82");
}
#[test]
fn bulk_75() {
    assert_eq!(lines("print(5*5+75)"), "100");
}
#[test]
fn bulk_76() {
    assert_eq!(lines("print(6*8+76)"), "124");
}
#[test]
fn bulk_77() {
    assert_eq!(lines("print(0*0+77)"), "77");
}
#[test]
fn bulk_78() {
    assert_eq!(lines("print(1*3+78)"), "81");
}
#[test]
fn bulk_79() {
    assert_eq!(lines("print(2*6+79)"), "91");
}
#[test]
fn cond_0() {
    assert_eq!(lines("print(0 > 5 ? \"big\" : \"small\")"), "small");
}
#[test]
fn cond_1() {
    assert_eq!(lines("print(1 > 5 ? \"big\" : \"small\")"), "small");
}
#[test]
fn cond_2() {
    assert_eq!(lines("print(2 > 5 ? \"big\" : \"small\")"), "small");
}
#[test]
fn cond_3() {
    assert_eq!(lines("print(3 > 5 ? \"big\" : \"small\")"), "small");
}
#[test]
fn cond_4() {
    assert_eq!(lines("print(4 > 5 ? \"big\" : \"small\")"), "small");
}
#[test]
fn cond_5() {
    assert_eq!(lines("print(5 > 5 ? \"big\" : \"small\")"), "small");
}
#[test]
fn cond_6() {
    assert_eq!(lines("print(6 > 5 ? \"big\" : \"small\")"), "big");
}
#[test]
fn cond_7() {
    assert_eq!(lines("print(7 > 5 ? \"big\" : \"small\")"), "big");
}
#[test]
fn cond_8() {
    assert_eq!(lines("print(8 > 5 ? \"big\" : \"small\")"), "big");
}
#[test]
fn cond_9() {
    assert_eq!(lines("print(9 > 5 ? \"big\" : \"small\")"), "big");
}
#[test]
fn switch_0() {
    assert_eq!(lines("let r=\"\"; switch(0){case 0:r=\"a\";break;case 1:r=\"b\";break;default:r=\"c\"} print(r)"), "a");
}
#[test]
fn switch_1() {
    assert_eq!(lines("let r=\"\"; switch(1){case 0:r=\"a\";break;case 1:r=\"b\";break;default:r=\"c\"} print(r)"), "b");
}
#[test]
fn switch_2() {
    assert_eq!(lines("let r=\"\"; switch(2){case 0:r=\"a\";break;case 1:r=\"b\";break;default:r=\"c\"} print(r)"), "c");
}
#[test]
fn switch_3() {
    assert_eq!(lines("let r=\"\"; switch(0){case 0:r=\"a\";break;case 1:r=\"b\";break;default:r=\"c\"} print(r)"), "a");
}
#[test]
fn switch_4() {
    assert_eq!(lines("let r=\"\"; switch(1){case 0:r=\"a\";break;case 1:r=\"b\";break;default:r=\"c\"} print(r)"), "b");
}
