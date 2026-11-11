package r86chip

import chisel3._
import chisel3.util._

// Combinational execution of the original custom-0 word, not an internal
// instruction stream. State/publication live in Core. The Rust ISS rv32.rs
// defines lanes, encodings, carry/borrow and the deterministic undefined AF.
object XFlags {
  def alu(a: UInt, b: UInt, lane: UInt, control: UInt, incoming: UInt): (UInt, UInt) = {
    val kind = control(2, 0)
    val mask = Mux(lane === 0.U, "hffffffff".U(32.W),
      Mux(lane === 1.U, "h0000ffff".U(32.W), "h000000ff".U(32.W)))
    val selected = Mux(lane === 3.U, a >> 8, a) & mask
    val lhs = Mux(kind === 7.U, 0.U(32.W), selected)
    val rhs = Mux(kind === 7.U, selected, Mux(control(4), b >> 8, b) & mask)
    val subtract = kind === 3.U || kind === 5.U || kind === 7.U
    val logic = kind === 1.U || kind === 4.U || kind === 6.U
    val carry = (kind === 2.U || kind === 3.U) && incoming(0)
    // One 33-bit add/subtract path. Subtraction complements all 32 rhs bits,
    // even for narrow operands; its bit-32 carry is inverted to obtain borrow.
    val sum = Cat(0.U(1.W), lhs) + Cat(0.U(1.W), Mux(subtract, ~rhs, rhs)) +
      Mux(subtract, !carry, carry).asUInt
    val raw = MuxLookup(kind, sum(31, 0))(Seq(
      1.U -> (lhs | rhs), 4.U -> (lhs & rhs), 6.U -> (lhs ^ rhs)))
    val result = raw & mask
    def sign(x: UInt): Bool = Mux(lane === 0.U, x(31), Mux(lane === 1.U, x(15), x(7)))
    val rawCarry = Mux(subtract, !sum(32),
      Mux(lane === 0.U, sum(32), Mux(lane === 1.U, sum(16), sum(8))))
    val cf = Mux(control(5), incoming(0), !logic && rawCarry)
    val pf = !result(7, 0).xorR
    val af = !logic && (lhs ^ rhs ^ result)(4)
    val zf = result === 0.U
    val sf = sign(result)
    val of = !logic && sign((lhs ^ result) & Mux(subtract, lhs ^ rhs, ~(lhs ^ rhs)))
    val flags = (cf.asUInt | (pf.asUInt << 2) | (af.asUInt << 4) |
      (zf.asUInt << 6) | (sf.asUInt << 7) | (of.asUInt << 11)).pad(32)
    // XMERGE uses the same source selection and destination merge, without
    // publishing either flag value. Keep its selection off the flag path.
    // Each byte range of the destination takes the result or keeps a.
    val laneResult = Mux(control(6), rhs, raw)
    val merged = Cat(
      Mux(lane === 0.U, laneResult(31, 16), a(31, 16)),
      Mux(lane === 3.U, laneResult(7, 0), Mux(lane === 2.U, a(15, 8), laneResult(15, 8))),
      Mux(lane === 3.U, a(7, 0), laneResult(7, 0)))
    (merged, flags)
  }

  // One funnel shifter for RV SLL/SRL/SRA and the x86 shifts and rotates.
  // kind: 0 SHL, 1 SHR, 2 SAR, 3 ROL, 4 ROR; count is 0..31. Left forms shift
  // the bit-reversed value right. Funnel bit 0 is the last bit shifted out.
  // Status follows the shared interpreter: shifts define CF/PF/ZF/SF/OF and
  // clear AF; rotates replace CF/OF only. The caller ignores status when the
  // count is zero.
  def shift(a: UInt, count: UInt, kind: UInt, incoming: UInt): (UInt, UInt) = {
    val left = kind === 0.U || kind === 3.U
    val rotate = kind === 3.U || kind === 4.U
    val x = Mux(left, Reverse(a), a)
    val fill = Mux(rotate, x, Fill(32, kind === 2.U && a(31)))
    val funnel = (Cat(fill, x, 0.U(1.W)) >> count)(32, 0)
    val result = Mux(left, Reverse(funnel(32, 1)), funnel(32, 1))
    val last = funnel(0)
    val cf = MuxLookup(kind, last)(Seq(3.U -> result(0), 4.U -> result(31)))
    val of = MuxLookup(kind, false.B)(Seq(
      0.U -> (last ^ result(31)),
      1.U -> (count === 1.U && a(31)),
      3.U -> (result(31) ^ result(0)),
      4.U -> (result(31) ^ result(30))))
    val pf = !result(7, 0).xorR
    val zf = result === 0.U
    val shifted = (cf.asUInt | (pf.asUInt << 2) | (zf.asUInt << 6) |
      (result(31).asUInt << 7) | (of.asUInt << 11)).pad(32)
    val rotated = (incoming & "h0d4".U(32.W)) | cf.asUInt | (of.asUInt << 11)
    (result, Mux(rotate, rotated, shifted))
  }

  def condition(cc: UInt, flags: UInt): Bool = {
    val cf = flags(0); val pf = flags(2); val zf = flags(6)
    val sf = flags(7); val of = flags(11)
    val paired = VecInit(Seq(of, cf, zf, cf || zf, sf, pf, sf ^ of, zf || (sf ^ of)))
    paired(cc(3, 1)) ^ cc(0)
  }
}
