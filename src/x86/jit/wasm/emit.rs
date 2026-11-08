//! Small binary encoder for the integer Wasm instructions this backend uses.
//! No WAT parser, optimizer, or external compiler is part of guest translation.

use alloc::vec::Vec;

pub const I32: u8 = 0x7f;
// Local entry, eight GPRs, EFLAGS, remaining budget, and an optional arithmetic
// record (lhs, result, kind/width/carry).
pub const ARGUMENTS: u32 = 14;
pub const FLAGS_FUNCTION: u32 = 0;
pub const FIRST_REGION_FUNCTION: u32 = 1;

#[derive(Default)]
pub struct Code(pub Vec<u8>);

impl Code {
    pub fn byte(&mut self, v: u8) {
        self.0.push(v);
    }
    pub fn u32(&mut self, mut v: u32) {
        loop {
            let b = (v & 127) as u8;
            v >>= 7;
            self.byte(b | if v == 0 { 0 } else { 128 });
            if v == 0 {
                break;
            }
        }
    }
    pub fn i32(&mut self, mut v: i32) {
        loop {
            let b = (v & 127) as u8;
            v >>= 7;
            let done = (v == 0 && b & 64 == 0) || (v == -1 && b & 64 != 0);
            self.byte(b | if done { 0 } else { 128 });
            if done {
                break;
            }
        }
    }
    pub fn name(&mut self, s: &str) {
        self.u32(s.len() as u32);
        self.0.extend_from_slice(s.as_bytes());
    }
    pub fn section(&mut self, id: u8, data: &Code) {
        self.byte(id);
        self.u32(data.0.len() as u32);
        self.0.extend_from_slice(&data.0);
    }
    pub fn get(&mut self, local: u32) {
        self.byte(0x20);
        self.u32(local);
    }
    pub fn set(&mut self, local: u32) {
        self.byte(0x21);
        self.u32(local);
    }
    pub fn tee(&mut self, local: u32) {
        self.byte(0x22);
        self.u32(local);
    }
    pub fn constant(&mut self, n: u32) {
        self.byte(0x41);
        self.i32(n as i32);
    }
    pub fn binary(&mut self, op: u8, rhs: u32) {
        self.constant(rhs);
        self.byte(op);
    }
    pub fn load(&mut self, bits: u8, offset: u32) {
        self.byte(match bits {
            8 => 0x2d,
            16 => 0x2f,
            32 => 0x28,
            64 => 0x29,
            _ => unreachable!(),
        });
        self.u32(0);
        self.u32(offset);
    }
    pub fn store(&mut self, bits: u8, offset: u32) {
        self.byte(match bits {
            8 => 0x3a,
            16 => 0x3b,
            32 => 0x36,
            64 => 0x37,
            _ => unreachable!(),
        });
        self.u32(0);
        self.u32(offset);
    }
    pub fn block(&mut self) {
        self.0.extend_from_slice(&[0x02, 0x40]);
    }
    pub fn loop_(&mut self) {
        self.0.extend_from_slice(&[0x03, 0x40]);
    }
    pub fn if_(&mut self) {
        self.0.extend_from_slice(&[0x04, 0x40]);
    }
    pub fn else_(&mut self) {
        self.byte(0x05);
    }
    pub fn end(&mut self) {
        self.byte(0x0b);
    }
    pub fn br(&mut self, depth: u32) {
        self.byte(0x0c);
        self.u32(depth);
    }
    pub fn br_if(&mut self, depth: u32) {
        self.byte(0x0d);
        self.u32(depth);
    }
    pub fn call(&mut self, index: u32) {
        self.byte(0x10);
        self.u32(index);
    }
}

/// Regions pass live integer state in arguments across tail calls; only an
/// actual exit commits Frame. Memory and the function table are the only
/// imports; TLB misses return to the core at an exact instruction boundary.
/// The memory need not be shared: the emulator core is single-threaded.
pub fn module(bodies: Vec<(Code, u32)>, locals: u32, flags: Code, memory_pages: u32) -> Vec<u8> {
    let mut out = Code(b"\0asm\x01\0\0\0".to_vec());
    let mut types = Code::default();
    types.u32(2);
    types.byte(0x60);
    types.u32(ARGUMENTS);
    for _ in 0..ARGUMENTS {
        types.byte(I32);
    }
    types.byte(0);
    types.byte(0x60);
    types.u32(4);
    for _ in 0..4 {
        types.byte(I32);
    }
    types.0.extend_from_slice(&[1, I32]);
    out.section(1, &types);
    let mut imports = Code::default();
    imports.u32(2);
    imports.name("r");
    imports.name("memory");
    imports.byte(2);
    imports.byte(0);
    imports.u32(memory_pages);
    imports.name("r");
    imports.name("table");
    imports.byte(1);
    imports.byte(0x70);
    imports.byte(0);
    imports.u32(0);
    out.section(2, &imports);
    let mut functions = Code::default();
    functions.u32(bodies.len() as u32 + 1);
    functions.u32(1); // shared arithmetic-record materializer
    for _ in &bodies {
        functions.u32(0);
    }
    out.section(3, &functions);
    let mut exports = Code::default();
    exports.u32(bodies.len() as u32);
    for i in 0..bodies.len() {
        exports.name(&if i == 0 {
            alloc::string::String::from("run")
        } else {
            alloc::format!("run{i}")
        });
        exports.byte(0);
        exports.u32(i as u32 + FIRST_REGION_FUNCTION);
    }
    out.section(7, &exports);
    let mut code = Code::default();
    code.u32(bodies.len() as u32 + 1);
    let mut helper = Code::default();
    helper.0.extend_from_slice(&[1, 4, I32]); // four i32 locals
    helper.0.extend(flags.0);
    helper.end();
    code.u32(helper.0.len() as u32);
    code.0.extend(helper.0);
    let mut names = Code::default();
    names.u32(bodies.len() as u32 + 1);
    names.u32(FLAGS_FUNCTION);
    names.name("r86_flags");
    for (index, (body, pc)) in bodies.into_iter().enumerate() {
        names.u32(index as u32 + FIRST_REGION_FUNCTION);
        names.name(&alloc::format!("x86_{pc:08x}"));
        let mut function = Code::default();
        function.u32(2);
        function.u32(locals);
        function.byte(I32);
        function.u32(3);
        function.byte(0x7e); // i64 product/division temporaries
        function.0.extend(body.0);
        function.end();
        code.u32(function.0.len() as u32);
        code.0.extend(function.0);
    }
    out.section(10, &code);
    let mut debug = Code::default();
    debug.name("name");
    debug.section(1, &names);
    out.section(0, &debug);
    out.0
}
