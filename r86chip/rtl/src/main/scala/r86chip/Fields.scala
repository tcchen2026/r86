package r86chip

import chisel3._
import chisel3.util._

// Fields and memory-word classification of one instruction word, wherever in
// the pipeline it currently is. Pure wiring; rv32.rs defines the encodings.
class Fields(w: UInt) {
  val opcode = w(6, 0)
  val f3 = w(14, 12)
  val f7 = w(31, 25)
  val rd = w(11, 7)
  val rs1 = w(19, 15)
  val rs2 = w(24, 20)
  val immI = Cat(Fill(20, w(31)), w(31, 20))
  val immS = Cat(Fill(20, w(31)), w(31, 25), w(11, 7))
  val immB = Cat(Fill(19, w(31)), w(31), w(7), w(30, 25), w(11, 8), 0.U(1.W))
  val immJ = Cat(Fill(11, w(31)), w(31), w(19, 12), w(20), w(30, 21), 0.U(1.W))
  val immU = Cat(w(31, 12), 0.U(12.W))
  def is(code: Int): Bool = opcode === code.U

  // Guest memory words. 0x5b: offset in rs1, segment in funct7 (or rs2).
  // 0x07/0x27 load/store through DS and 0x47/0x77 through SS with the RV I/S
  // displacement; they may be unaligned inside a page. Displacement loads:
  // funct3 0/1 sign-extended byte/word, 2 dword, 4/5 zero-extended, 3/6/7
  // dword/byte/word with write intent. Displacement stores with funct3 bit 2
  // also write the offset back to rs1. XPUSHRET is 0x77 funct3 7.
  val guestOld = is(0x5b)
  val guestLoadD = is(0x07) || is(0x47)
  val guestStoreD = is(0x27) || is(0x77)
  val guestMemory = guestOld || guestLoadD || guestStoreD
  val nativeMemory = is(0x03) || is(0x23)
  val memory = guestMemory || nativeMemory
  val store = is(0x23) || (guestOld && f3(2)) || guestStoreD
  val guestFetch = guestOld && f7(4)
  val guestRmw = Mux(guestOld, f7(3), guestLoadD && (f3 === 3.U || f3(2, 1) === 3.U))
  val guestSigned = guestLoadD && f3(2, 1) === 0.U
  val guestUpdate = guestStoreD && f3(2)
  val pushRet = is(0x77) && f3 === 7.U
  val guestSpan = !guestOld || (guestFetch && f7(5))
  val guestWidth = Mux(guestLoadD,
    Mux(f3(2) || !f3(1), Cat(0.U(1.W), f3(0)), 2.U(2.W)), Mux(pushRet, 2.U(2.W), f3(1, 0)))
  val guestRegisterSegment = guestOld && f7(6)
  def guestSegment(b: UInt): UInt = Mux(guestOld, Mux(guestRegisterSegment, b(2, 0), f7(2, 0)),
    Mux(opcode(6), 2.U(3.W), 3.U(3.W)))
  val guestRequired = Mux(store, 2.U(3.W),
    Mux(guestFetch, 4.U(3.W), Mux(guestRmw, 3.U(3.W), 1.U(3.W))))
  val displacement = Mux(guestOld, 0.U(32.W),
    Mux(pushRet, "hfffffffc".U(32.W), Mux(opcode(5), immS, immI)))
  val xtlook = is(0x2b) && f3 === 0.U
  val xjt = is(0x6b) && f3 === 0.U
  val ea = is(0x7b) && f3 === 0.U
  val muldiv = is(0x33) && f7 === 1.U
  val zba = is(0x33) && f7 === "h10".U
  val xalui = is(0x1b)
  val incdec = xalui && f3 === 7.U
  // Width of the access in the low two bits; native words use funct3.
  val accessWidth = Mux(guestMemory, guestWidth, f3(1, 0))
  val probeFetch = Mux(guestMemory, guestFetch, w(23, 22) === 2.U)
  val probeRequired = Mux(guestMemory, guestRequired,
    MuxLookup(w(23, 22), 1.U(3.W))(Seq(
      0.U -> 1.U(3.W), 1.U -> 2.U(3.W), 2.U -> 4.U(3.W), 3.U -> 3.U(3.W))))
  val probeWidth = Mux(guestMemory, guestWidth, w(21, 20))

  // Register written when the word commits normally (XEA32 only on a hit).
  val writes = Seq(0x37, 0x17, 0x6f, 0x67, 0x13, 0x33, 0x03, 0x1b, 0x3b).map(is).reduce(_ || _) ||
    (is(0x0b) && f3 =/= 4.U) || (is(0x2b) && (f3 === 0.U || (f3 === 7.U && f7 === 1.U))) ||
    (guestMemory && (!store || guestUpdate)) || (is(0x6b) && f3 === 6.U) ||
    (is(0x7b) && (f3 === 6.U || f3 === 7.U || f3 <= 1.U))
  val dest = Mux(guestUpdate, rs1, rd)
  val crossWrite = is(0x7b) && f3 === 7.U

  // Results the address stage can produce: constants, the link value, and
  // add/subtract/logic on full registers (RV, Zba, and the dword forms of
  // XALU/XALUI whose value does not depend on status). Status, where the
  // word defines any, is still computed at commit.
  val rvLogic = (is(0x13) || (is(0x33) && f7 === 0.U)) && (f3 === 0.U || f3 === 4.U || f3 >= 6.U)
  val rvSub = is(0x33) && f7 === "h20".U && f3 === 0.U
  val zbaOk = zba && (f3 === 2.U || f3 === 4.U || f3 === 6.U)
  val xKind = Mux(xalui, f3, f7(2, 0))
  val xPlain = (xalui || (is(0x0b) && f3 === 0.U && !f7(6) && !f7(4))) &&
    (xKind === 0.U || xKind === 1.U || xKind >= 4.U) && (xalui || xKind =/= 7.U)
  val earlyConstant = is(0x37) || is(0x17) || is(0x6f) || is(0x67)
  val early = earlyConstant || rvLogic || rvSub || zbaOk || xPlain
  val earlyUsesB = is(0x33) || is(0x0b)
  // 0 add, 1 or, 2 and, 3 xor, 4 subtract.
  val earlyKind = Mux(rvSub, 4.U(3.W), Mux(zbaOk, 0.U(3.W),
    Mux(xPlain, MuxLookup(xKind, 0.U(3.W))(Seq(1.U -> 1.U(3.W), 4.U -> 2.U(3.W), 5.U -> 4.U(3.W), 6.U -> 3.U(3.W))),
      MuxLookup(f3, 0.U(3.W))(Seq(4.U -> 3.U(3.W), 6.U -> 1.U(3.W), 7.U -> 2.U(3.W))))))
  // Operands whose value is needed in the address stage itself.
  val needA = memory || xtlook || xjt || is(0x67)
  val needB = guestRegisterSegment
}
