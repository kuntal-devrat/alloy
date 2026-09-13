pub mod compiler;

use crate::bytecode::Program;
use hashbrown::HashMap;

pub type JitLoopFn = unsafe extern "C" fn(slots_ptr: *mut u64, slots_len: u64, max_trips: u64) -> u64;

pub struct JitEngine {
    compiler: compiler::JitCompiler,
    cache: HashMap<(u32, usize), JitLoopFn>, // (program_id, header_pc) -> JitLoopFn
    uncompilable: hashbrown::HashSet<(u32, usize)>,
    disabled: bool,
}

unsafe impl Send for JitEngine {}

impl JitEngine {
    pub fn new() -> Result<Self, String> {
        let disabled = std::env::var("ALLOY_NO_JIT").is_ok();
        let compiler = compiler::JitCompiler::new()?;
        Ok(Self {
            compiler,
            cache: HashMap::new(),
            uncompilable: hashbrown::HashSet::new(),
            disabled,
        })
    }

    pub fn is_disabled(&self) -> bool {
        self.disabled
    }

    pub fn cache_len(&self) -> usize {
        self.cache.len()
    }

    pub fn get_or_compile(
        &mut self,
        program_id: u32,
        program: &Program,
        header_pc: usize,
        backedge_pc: usize,
    ) -> Option<JitLoopFn> {
        if self.disabled {
            return None;
        }
        let key = (program_id, header_pc);
        if self.uncompilable.contains(&key) {
            return None;
        }
        if let Some(&f) = self.cache.get(&key) {
            return Some(f);
        }
        match self.compiler.compile_loop(program, header_pc, backedge_pc) {
            Some(f) => {
                self.cache.insert(key, f);
                Some(f)
            }
            None => {
                self.uncompilable.insert(key);
                None
            }
        }
    }

    pub fn mark_uncompilable(&mut self, program_id: u32, header_pc: usize) {
        let key = (program_id, header_pc);
        self.cache.remove(&key);
        self.uncompilable.insert(key);
    }
}
