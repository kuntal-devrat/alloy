use std::sync::{OnceLock, RwLock};

#[derive(Copy, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Debug, Default)]
#[repr(transparent)]
pub struct Atom(pub u32);

impl Atom {
    pub const EMPTY: Atom = Atom(0);
    pub const LENGTH: Atom = Atom(1);
    pub const PROTOTYPE: Atom = Atom(2);
    pub const CONSTRUCTOR: Atom = Atom(3);
    pub const NAME: Atom = Atom(4);
    pub const VALUE: Atom = Atom(5);
    pub const DONE: Atom = Atom(6);
    pub const NEXT: Atom = Atom(7);
    pub const PUSH: Atom = Atom(8);
    pub const POP: Atom = Atom(9);
    pub const SHIFT: Atom = Atom(10);
    pub const SLICE: Atom = Atom(11);
    pub const JOIN: Atom = Atom(12);
    pub const TO_STRING: Atom = Atom(13);
    pub const VALUE_OF: Atom = Atom(14);
    pub const CALL: Atom = Atom(15);
    pub const APPLY: Atom = Atom(16);
    pub const BIND: Atom = Atom(17);
    pub const THEN: Atom = Atom(18);
    pub const CATCH: Atom = Atom(19);
    pub const MESSAGE: Atom = Atom(20);
    pub const STACK: Atom = Atom(21);
    pub const GET: Atom = Atom(22);
    pub const SET: Atom = Atom(23);
    pub const HAS: Atom = Atom(24);
    pub const DELETE_PROPERTY: Atom = Atom(25);
    pub const RETURN: Atom = Atom(26);
    pub const THROW: Atom = Atom(27);

    #[inline(always)]
    pub fn is_empty(self) -> bool {
        self.0 == 0
    }

    #[inline(always)]
    pub fn as_u32(self) -> u32 {
        self.0
    }
}

const STATIC_STRINGS: &[&str] = &[
    "",               // 0: EMPTY
    "length",         // 1: LENGTH
    "prototype",      // 2: PROTOTYPE
    "constructor",    // 3: CONSTRUCTOR
    "name",           // 4: NAME
    "value",          // 5: VALUE
    "done",           // 6: DONE
    "next",           // 7: NEXT
    "push",           // 8: PUSH
    "pop",            // 9: POP
    "shift",          // 10: SHIFT
    "slice",          // 11: SLICE
    "join",           // 12: JOIN
    "toString",       // 13: TO_STRING
    "valueOf",        // 14: VALUE_OF
    "call",           // 15: CALL
    "apply",          // 16: APPLY
    "bind",           // 17: BIND
    "then",           // 18: THEN
    "catch",          // 19: CATCH
    "message",        // 20: MESSAGE
    "stack",          // 21: STACK
    "get",            // 22: GET
    "set",            // 23: SET
    "has",            // 24: HAS
    "deleteProperty", // 25: DELETE_PROPERTY
    "return",         // 26: RETURN
    "throw",          // 27: THROW
];

struct InternerState {
    map: hashbrown::HashMap<String, Atom>,
    strings: Vec<String>,
}

static INTERNER: OnceLock<RwLock<InternerState>> = OnceLock::new();

fn get_interner() -> &'static RwLock<InternerState> {
    INTERNER.get_or_init(|| {
        let mut map = hashbrown::HashMap::with_capacity(STATIC_STRINGS.len() + 128);
        let mut strings = Vec::with_capacity(STATIC_STRINGS.len() + 128);
        for (idx, &s) in STATIC_STRINGS.iter().enumerate() {
            let atom = Atom(idx as u32);
            map.insert(s.to_string(), atom);
            strings.push(s.to_string());
        }
        RwLock::new(InternerState { map, strings })
    })
}

/// Look up or allocate an `Atom` for `s`.
pub fn atom_of(s: &str) -> Atom {
    // Fast path for static strings
    match s {
        "" => return Atom::EMPTY,
        "length" => return Atom::LENGTH,
        "prototype" => return Atom::PROTOTYPE,
        "constructor" => return Atom::CONSTRUCTOR,
        "name" => return Atom::NAME,
        "value" => return Atom::VALUE,
        "done" => return Atom::DONE,
        "next" => return Atom::NEXT,
        "push" => return Atom::PUSH,
        "pop" => return Atom::POP,
        "shift" => return Atom::SHIFT,
        "slice" => return Atom::SLICE,
        "join" => return Atom::JOIN,
        "toString" => return Atom::TO_STRING,
        "valueOf" => return Atom::VALUE_OF,
        "call" => return Atom::CALL,
        "apply" => return Atom::APPLY,
        "bind" => return Atom::BIND,
        "then" => return Atom::THEN,
        "catch" => return Atom::CATCH,
        "message" => return Atom::MESSAGE,
        "stack" => return Atom::STACK,
        "get" => return Atom::GET,
        "set" => return Atom::SET,
        "has" => return Atom::HAS,
        "deleteProperty" => return Atom::DELETE_PROPERTY,
        "return" => return Atom::RETURN,
        "throw" => return Atom::THROW,
        _ => {}
    }

    let interner = get_interner();
    if let Ok(guard) = interner.read() {
        if let Some(&atom) = guard.map.get(s) {
            return atom;
        }
    }

    let mut guard = interner.write().unwrap_or_else(|g| g.into_inner());
    if let Some(&atom) = guard.map.get(s) {
        return atom;
    }
    let atom = Atom(guard.strings.len() as u32);
    let s_owned = s.to_string();
    guard.map.insert(s_owned.clone(), atom);
    guard.strings.push(s_owned);
    atom
}

/// Retrieve a clone of the string corresponding to `atom`.
pub fn str_of(atom: Atom) -> String {
    let idx = atom.0 as usize;
    if idx < STATIC_STRINGS.len() {
        return STATIC_STRINGS[idx].to_string();
    }
    let interner = get_interner();
    let guard = interner.read().unwrap_or_else(|g| g.into_inner());
    if let Some(s) = guard.strings.get(idx) {
        s.clone()
    } else {
        String::new()
    }
}

/// Execute a closure with a borrowed view of `atom`'s string.
pub fn with_atom_str<R>(atom: Atom, f: impl FnOnce(&str) -> R) -> R {
    let idx = atom.0 as usize;
    if idx < STATIC_STRINGS.len() {
        return f(STATIC_STRINGS[idx]);
    }
    let interner = get_interner();
    let guard = interner.read().unwrap_or_else(|g| g.into_inner());
    if let Some(s) = guard.strings.get(idx) {
        f(s.as_str())
    } else {
        f("")
    }
}

impl std::fmt::Display for Atom {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        with_atom_str(*self, |s| f.write_str(s))
    }
}

impl From<&str> for Atom {
    #[inline]
    fn from(s: &str) -> Self {
        atom_of(s)
    }
}

impl From<String> for Atom {
    #[inline]
    fn from(s: String) -> Self {
        atom_of(&s)
    }
}
