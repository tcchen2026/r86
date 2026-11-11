package r86chip

import chisel3._
import chisel3.util._

class WordAccessIO(d: CacheShape) extends Bundle {
  // One word read (strb 0) or write, held until `done`.
  val valid = Input(Bool())
  val addr = Input(UInt(32.W))
  val wdata = Input(UInt(32.W))
  val wstrb = Input(UInt(4.W))
  val done = Output(Bool())
  val rdata = Output(UInt(32.W))
  val error = Output(Bool())
  val active = Output(Bool())
  // quick: the request comes from registers, so it may take the read port in
  // its first clock, unless a store is being written in that clock (hold).
  val quick = Input(Bool())
  val hold = Input(Bool())
  // Data cache read port, taken while `steer`.
  val steer = Output(Bool())
  val d_addr = Output(UInt(32.W))
  val d_hit = Input(Bool())
  val d_way = Input(Bool())
  val d_word = Input(UInt(32.W))
  val d_error = Input(Bool())
  val d_take = Output(Bool())
  val d_fill = Output(Bool())
  val busy = Input(Bool())
  val s_valid = Output(Bool())
  val s_way = Output(Bool())
  // Devices: native addresses with bit 31 set, single word, never cached.
  val dev_valid = Output(Bool())
  val dev_ready = Input(Bool())
  val dev_rdata = Input(UInt(32.W))
  val dev_error = Input(Bool())
}

// The commit stage's own word-at-a-time memory access, for what the pipeline
// does not do in passing: device words, accesses that span two words, and
// XEA32's reads. It runs only for the oldest instruction, so a device read
// is never speculative. A cached word that is absent is requested and waited
// for here; nothing is replayed.
class WordAccess(d: CacheShape) extends Module {
  val io = IO(new WordAccessIO(d))
  val sIdle :: sPresent :: sCheck :: sWait :: sDevice :: Nil = Enum(5)
  val state = RegInit(sIdle)
  val device = io.addr(31)
  io.active := io.valid
  val now = state === sIdle && io.valid && !device && io.quick && !io.hold
  io.steer := state === sPresent || now
  io.d_addr := Cat(io.addr(31, 2), 0.U(2.W))
  io.d_take := false.B
  io.d_fill := false.B
  io.s_valid := false.B
  io.s_way := io.d_way
  io.dev_valid := state === sDevice
  io.done := false.B
  io.rdata := Mux(device, io.dev_rdata, io.d_word)
  io.error := Mux(device, io.dev_error, io.d_error)
  switch(state) {
    // Otherwise the first clock lets an older store reach the cache.
    is(sIdle) { when(io.valid) { state := Mux(device, sDevice, Mux(now, sCheck, sPresent)) } }
    is(sPresent) { state := sCheck }
    is(sCheck) {
      when(io.d_error) {
        io.d_take := true.B
        io.done := true.B
        state := sIdle
      }.elsewhen(io.d_hit) {
        io.s_valid := io.wstrb =/= 0.U
        io.done := true.B
        state := sIdle
      }.otherwise {
        io.d_fill := true.B
        state := sWait
      }
    }
    is(sWait) { when(!io.busy) { state := sPresent } }
    is(sDevice) {
      when(io.dev_ready) {
        io.done := true.B
        state := sIdle
      }
    }
  }
}
