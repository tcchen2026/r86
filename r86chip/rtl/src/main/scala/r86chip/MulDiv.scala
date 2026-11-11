package r86chip

import chisel3._
import chisel3.util._

class MulDivIO extends Bundle {
  val start = Input(Bool())
  // Drop the operation in progress; its instruction was squashed.
  val abort = Input(Bool())
  val operation = Input(UInt(3.W))
  val a = Input(UInt(32.W))
  val b = Input(UInt(32.W))
  val done = Output(Bool())
  val result = Output(UInt(32.W))
}

// Fixed 32-round shift/add multiplication and restoring division. No DSPs,
// combinational divide, instruction expansion, or microcode storage.
class MulDiv extends Module {
  val io = IO(new MulDivIO)
  val busy = RegInit(false.B)
  val done = RegInit(false.B)
  val result = RegInit(0.U(32.W))
  val op = Reg(UInt(3.W))
  val count = Reg(UInt(5.W))
  val negative = Reg(Bool())
  val remainderNegative = Reg(Bool())
  val operand = Reg(UInt(32.W))
  val quotient = Reg(UInt(32.W))
  val remainder = Reg(UInt(32.W))
  val product = Reg(UInt(64.W))

  val signA = io.a(31) && (io.operation === 1.U || io.operation === 2.U ||
    io.operation === 4.U || io.operation === 6.U)
  val signB = io.b(31) && (io.operation === 1.U || io.operation === 4.U || io.operation === 6.U)
  val magnitudeA = Mux(signA, 0.U(32.W) - io.a, io.a)
  val magnitudeB = Mux(signB, 0.U(32.W) - io.b, io.b)
  val sum = product(63, 32) +& Mux(product(0), operand, 0.U(32.W))
  val nextProduct = Cat(sum, product(31, 1))
  val shiftedRemainder = Cat(remainder, quotient(31))
  val subtract = shiftedRemainder >= operand
  val nextRemainder = Mux(subtract, shiftedRemainder - operand, shiftedRemainder)(31, 0)
  val nextQuotient = Cat(quotient(30, 0), subtract)

  done := false.B
  when(io.start && !busy) {
    op := io.operation
    count := 0.U
    negative := signA ^ signB
    remainderNegative := signA
    when(io.operation(2) && io.b === 0.U) {
      result := Mux(io.operation(1), io.a, "hffffffff".U)
      done := true.B
    }.otherwise {
      busy := true.B
      when(!io.operation(2)) {
        product := Cat(0.U(32.W), magnitudeB)
        operand := magnitudeA
      }.otherwise {
        quotient := magnitudeA
        remainder := 0.U
        operand := magnitudeB
      }
    }
  }.elsewhen(busy) {
    count := count + 1.U
    when(!op(2)) {
      product := nextProduct
      when(count === 31.U) {
        result := Mux(op === 0.U, nextProduct(31, 0),
          Mux(negative, ~nextProduct(63, 32) + (nextProduct(31, 0) === 0.U),
            nextProduct(63, 32)))
      }
    }.otherwise {
      remainder := nextRemainder
      quotient := nextQuotient
      when(count === 31.U) {
        result := Mux(op(1),
          Mux(remainderNegative, 0.U(32.W) - nextRemainder, nextRemainder),
          Mux(negative, 0.U(32.W) - nextQuotient, nextQuotient))
      }
    }
    when(count === 31.U) {
      busy := false.B
      done := true.B
    }
  }
  when(io.abort) {
    busy := false.B
    done := false.B
  }
  io.done := done
  io.result := result
}
