pub const IC_SLOTS: usize = 1024;

#[inline(always)]
pub fn ic_slot(pc: usize) -> usize {
    (pc ^ (pc >> 4)) & (IC_SLOTS - 1)
}

#[derive(Clone, Copy)]
pub struct IcEntry {
    pub program: u32,
    pub pc: u32,
    /// `Rc::as_ptr(&shape)` of the object's shape at this site.
    pub shape: u64,
    pub offset: u32,
    /// `Value` word of the property name (for `Value::string` clones of the
    /// same `Rc`, the word is identical — identity comparison).
    pub prop: u64,
}

impl IcEntry {
    pub const EMPTY: IcEntry = IcEntry {
        program: u32::MAX,
        pc: u32::MAX,
        shape: 0,
        offset: 0,
        prop: 0,
    };

    #[inline(always)]
    pub fn matches(&self, program: u32, pc: u32, prop_bits: u64, shape: u64) -> bool {
        self.program == program && self.pc == pc && self.prop == prop_bits && self.shape == shape
    }
}

/// Four-way polymorphic slot: e0, e1, e2, e3. Monomorphic sites hit e0
/// every time; 2-4 shape polymorphic sites hit e1..e3 instead of thrashing
/// back and forth. 5+ shapes fall back to the slow shape lookup (megamorphic).
#[derive(Clone, Copy)]
pub struct IcPoly {
    pub e0: IcEntry,
    pub e1: IcEntry,
    pub e2: IcEntry,
    pub e3: IcEntry,
}

impl IcPoly {
    pub const EMPTY: IcPoly = IcPoly {
        e0: IcEntry::EMPTY,
        e1: IcEntry::EMPTY,
        e2: IcEntry::EMPTY,
        e3: IcEntry::EMPTY,
    };

    #[inline(always)]
    pub fn probe(&self, program: u32, pc: u32, prop_bits: u64, shape: u64) -> Option<u32> {
        if self.e0.matches(program, pc, prop_bits, shape) {
            return Some(self.e0.offset);
        }
        if self.e1.matches(program, pc, prop_bits, shape) {
            return Some(self.e1.offset);
        }
        if self.e2.matches(program, pc, prop_bits, shape) {
            return Some(self.e2.offset);
        }
        if self.e3.matches(program, pc, prop_bits, shape) {
            return Some(self.e3.offset);
        }
        None
    }

    #[inline(always)]
    pub fn update(&mut self, fresh: IcEntry) {
        if self.e0.shape == fresh.shape && self.e0.prop == fresh.prop {
            self.e0 = fresh;
            return;
        }
        if self.e1.shape == fresh.shape && self.e1.prop == fresh.prop {
            let tmp = self.e0;
            self.e0 = fresh;
            self.e1 = tmp;
            return;
        }
        if self.e2.shape == fresh.shape && self.e2.prop == fresh.prop {
            let tmp = self.e1;
            self.e1 = fresh;
            self.e2 = tmp;
            return;
        }
        self.e3 = self.e2;
        self.e2 = self.e1;
        self.e1 = self.e0;
        self.e0 = fresh;
    }
}

/// Call-site cache: last callee seen at this `Call` pc + its function identity.
#[derive(Clone, Copy)]
pub struct CallIcEntry {
    pub callee_bits: u64,
    pub func_ptr: u64,
    pub params: u8,
}

impl CallIcEntry {
    pub const EMPTY: CallIcEntry = CallIcEntry {
        callee_bits: 0,
        func_ptr: 0,
        params: 0,
    };
}
