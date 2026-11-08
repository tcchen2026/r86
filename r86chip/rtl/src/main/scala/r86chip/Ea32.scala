package r86chip

import chisel3._
import chisel3.util._

// Wires/state inside Core, not another execution engine or an instruction ROM.
// XEA32's bounded sequence is SIB, displacement (one/two beats), base, index.
// RF operands and published proofs are held by the one execute slot throughout.
case class Ea32Signals(request: Bool, address: UInt, done: Bool, hit: Bool,
  fault: Bool, badAddress: UInt, offset: UInt, info: UInt)

object Ea32 {
  def apply(active: Bool, finish: Bool, modrm: UInt, start: UInt,
    configured: Bool, registers: UInt, contextOk: Bool,
    csBase: UInt, csLimit: UInt, csExecute: Bool,
    table: Vec[UInt], valid: UInt,
    ready: Bool, data: UInt, error: Bool): Ea32Signals = {

    val initial = 0.U(3.W)
    val afterSib = 1.U(3.W)
    val dispSecond = 2.U(3.W)
    val readBase = 3.U(3.W)
    val readIndex = 4.U(3.W)
    val phase = RegInit(initial)
    val sib = Reg(UInt(8.W))
    val firstWord = Reg(UInt(32.W))
    val displacement = Reg(UInt(32.W))
    val accumulator = Reg(UInt(32.W))

    val mode = modrm(7, 6)
    val hasSib = modrm(2, 0) === 4.U
    val base = Mux(hasSib, sib(2, 0), modrm(2, 0))
    val index = sib(5, 3)
    val scale = sib(7, 6)
    val hasBase = mode =/= 0.U || base =/= 5.U
    val hasIndex = hasSib && index =/= 4.U
    val dispBytes = Mux(mode === 1.U, 1.U(3.W),
      Mux(mode === 2.U || !hasBase, 4.U(3.W), 0.U(3.W)))
    val needSib = phase === initial && hasSib
    val choose = (phase === initial || phase === afterSib) && !needSib
    val needDisp = (choose && dispBytes =/= 0.U) || phase === dispSecond
    val needBase = (choose && dispBytes === 0.U && hasBase) || phase === readBase
    val needIndex = phase === readIndex
    val codeRead = needSib || needDisp

    val codeOffset = start + Mux(needSib, 0.U(32.W), hasSib.asUInt)
    val codeBytes = Mux(needSib, 1.U(3.W), dispBytes)
    val linear = csBase + codeOffset
    val firstLinear = csBase + start
    val lastOffset = codeOffset +& (codeBytes - 1.U)
    val slot = table(linear(13, 12))
    val codeOk = csExecute && codeOffset >= start && lastOffset <= csLimit &&
      linear(31, 12) === firstLinear(31, 12) &&
      (linear(11, 0) +& codeBytes) <= 4096.U &&
      valid(linear(13, 12)) && slot(38, 21) === linear(31, 14) && slot(0)
    val codeNative = Cat(slot(20, 1), linear(11, 0))
    val gprNative = registers + Cat(Mux(needIndex, index, base), 0.U(2.W))
    val address = Mux(codeRead,
      Cat(codeNative(31, 12), Mux(phase === dispSecond,
        codeNative(11, 2) + 1.U, codeNative(11, 2)), 0.U(2.W)), gprNative)
    val allowed = configured && contextOk
    val rejected = !allowed || (codeRead && !codeOk)
    val request = active && !rejected && (codeRead || needBase || needIndex)
    val transfer = request && ready
    val fault = transfer && error
    val succeed = transfer && !error

    // A second displacement beat always reports the first byte's native
    // address. Unknown payload registers are selected only after their fill.
    val badAddress = Mux(codeRead, codeNative, gprNative)
    val shifted = (Cat(data, Mux(phase === dispSecond, firstWord, data)) >>
      Cat(codeNative(1, 0), 0.U(3.W)))(31, 0)
    val dispValue = Mux(dispBytes === 1.U,
      Cat(Fill(24, shifted(7)), shifted(7, 0)), shifted)
    val splitDisp = dispBytes === 4.U && codeNative(1, 0) =/= 0.U
    val dispComplete = needDisp && (phase === dispSecond || !splitDisp)
    val finalRead = (dispComplete && !hasBase && !hasIndex) ||
      (needBase && !hasIndex) || needIndex
    val hit = succeed && finalRead
    val done = active && (rejected || fault || hit)
    val baseSum = data + Mux(dispBytes === 0.U, 0.U, displacement)
    val indexSum = accumulator + (data << scale)(31, 0)
    val offset = Mux(needIndex, indexSum, Mux(needBase, baseSum, dispValue))
    val length = dispBytes + hasSib.asUInt
    val defaultSs = hasBase && (base === 4.U || base === 5.U)
    val info = Mux(hit, Cat(0.U(28.W), length, defaultSs), "hffffffff".U)

    when(succeed) {
      when(needSib) {
        sib := shifted(7, 0)
        phase := afterSib
      }.elsewhen(needDisp) {
        when(!dispComplete) {
          firstWord := data
          phase := dispSecond
        }.otherwise {
          displacement := dispValue
          accumulator := dispValue
          phase := Mux(hasBase, readBase, readIndex)
        }
      }.elsewhen(needBase) {
        accumulator := baseSum
        phase := readIndex
      }
    }
    // Clear on the retiring/faulting edge, including when another XEA32 is
    // issued immediately. Merely waiting for inactive would reuse old state.
    when(finish || !active) { phase := initial }
    Ea32Signals(request, address, done, hit, fault, badAddress, offset, info)
  }
}
