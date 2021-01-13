use super::{shm_conds, forkcli, shm_branches};
use std::ops::DerefMut;
use std::ptr;
use std::sync::Once;

static START: Once = Once::new();

use libc::{c_char, c_int};

extern "C" {
    fn printf(fmt : *const c_char, ...) -> c_int;
}

#[ctor]
fn fast_init() { 
    START.call_once(|| {
        shm_branches::map_branch_counting_shm();
        forkcli::start_forkcli();
    });
}

#[no_mangle]
pub extern "C" fn __angora_trace_cmp(
    condition: u32,
    cmpid: u32,
    context: u32,
    arg1: u64,
    arg2: u64,
    func : u32,
) -> u32 {
    unsafe {
        printf("fast cmp : %d,%d,%d\n\0".as_ptr() as *const i8, cmpid, condition, func);
        let a : * mut i8 = ptr::null_mut();
        *a = 4;
    }

    let mut conds = shm_conds::SHM_CONDS.lock().expect("SHM mutex poisoned.");
    match conds.deref_mut() {
        &mut Some(ref mut c) => {
            if c.check_match(cmpid, context) {
                return c.update_cmp(condition, arg1, arg2);
            }
        }
        _ => {}
    }
    condition
}

#[no_mangle]
pub extern "C" fn __angora_trace_switch(cmpid: u32, context: u32, condition: u64) -> u64 {
    let mut conds = shm_conds::SHM_CONDS.lock().expect("SHM mutex poisoned.");
    match conds.deref_mut() {
        &mut Some(ref mut c) => {
            if c.check_match(cmpid, context) {
                return c.update_switch(condition);
            }
        }
        _ => {}
    }
    condition
}
