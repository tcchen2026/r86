// Independent LLVM assembler oracle for src/x86/jit/a64/tests.rs.
// Regenerate the .hex fixture from the .text section with:
// clang --target=aarch64-linux-gnu -c tests/fixtures/jit_arm64.s -o /tmp/jit_arm64.o
// llvm-objcopy --dump-section .text=/tmp/jit_arm64.bin /tmp/jit_arm64.o
// python3 -c 'import struct,pathlib; p=pathlib.Path("/tmp/jit_arm64.bin"); print("\n".join(f"{w:08x}" for w, in struct.iter_unpack("<I",p.read_bytes())))'
.text
    b .+4
    b .-8
    movz w0, #0x5678
    movk w0, #0x1234, lsl #16
    movz x1, #0xdef0
    movk x1, #0x9abc, lsl #16
    movk x1, #0x5678, lsl #32
    movk x1, #0x1234, lsl #48
    movz w2, #0x1234, lsl #16
    movz x3, #0
    mov w4, w5
    mov x6, x7
    adds w8, w9, w10
    sub x11, x12, x13
    add w0, w1, w2, lsl #3
    add x3, x4, x5, lsl #5
    cmp x12, x9, lsr #32
    add x0, x14, x16, lsr #32
    add x5, x5, x5, lsr #0
    add x6, xzr, x7, lsr #63
    smull x2, w0, w1
    umull x4, w3, w5
    udiv x2, x0, x1
    sdiv x4, x3, x5
    msub x6, x2, x1, x0
    rbit w3, w5
    clz w4, w7
    adcs w0, w1, w2
    adc x3, x4, x5
    sbc w6, w7, w8
    sbcs x9, x10, x11
    sub sp, sp, #96
    adds w14, w15, #4095
    and w0, w1, w2
    orr x3, x4, x5
    eor w6, w7, w8
    ands x9, x10, x11
    eor w15, w15, #0x20000000
    eor x1, x2, #1
    eor x3, x4, #0x8000000000000000
    and w12, w13, #0xfffff000
    and x14, x15, #0xfffffffffffff000
    ubfx w12, w13, #8, #8
    asr w14, w15, #8
    lsl x16, x17, #5
    bfi w0, w1, #8, #8
    cset w2, cc
    lsrv x3, x4, x5
    lslv w6, w7, w8
    asrv x9, x10, x11
    rorv w12, w13, w14
    mrs x3, nzcv
    msr nzcv, x4
    ldrb w5, [x6, #12]
    strb w7, [x8, #4095]
    ldrb w5, [x6, x13]
    strb w7, [x8, x13]
    ldrh w5, [x6, #24]
    strh w7, [x8, #8190]
    ldrh w5, [x6, x13]
    strh w7, [x8, x13]
    ldr w5, [x6, #48]
    str w7, [x8, #16380]
    ldr w5, [x6, x13]
    str w7, [x8, x13]
    ldr x5, [x6, #96]
    str x7, [x8, #32760]
    ldr x5, [x6, x13]
    str x7, [x8, x13]
    ldrsw x3, [x4, x0, lsl #2]
    stp x19, x20, [sp]
    ldp x29, x30, [sp, #80]
    stp w9, w10, [x28, #-8]
    stp x2, x6, [x29], #16
    ldp x0, x1, [x29, #-16]!
    sturb w7, [x8, #255]
    sturh w7, [x8, #2]
    stur w7, [x8, #-256]
    stur x7, [x8, #6]
back:
    b.eq forward
    cbz w5, back
    cbnz x6, forward
    tbz w7, #0, back
    tbnz w8, #5, forward
    tbz w15, #31, forward
    tbnz x16, #63, back
    b back
    bl back
    bl forward
forward:
    br x16
    blr x17
    ret
    ret x1
