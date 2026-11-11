package r86chip

import chisel3._
import chisel3.util._

// The one main-memory interface, independent of the RAM technology. A back
// end (HyperRAM, SPI/QSPI PSRAM, DDR, or block RAM) implements `Flipped`.
//
// A request names a word-aligned address and a burst of `beats + 1` words at
// ascending addresses; a back end may wrap or split it internally but answers
// in request order. One request is outstanding at a time.
//   Read:  the back end returns beats + 1 responses, `last` on the final one.
//   Write: the master supplies beats + 1 `w` words, then the back end returns
//          one response with `last`. A zero strobe leaves its byte unchanged.
// `error` on any response means the address is not backed; the master still
// drains the burst. Latency is unbounded and may differ per request, so slow
// serial RAM and DDR look the same here. Caches belong on the master side.
class RamRequest extends Bundle {
  val address = UInt(32.W)
  val write = Bool()
  val beats = UInt(8.W)
}

class RamWrite extends Bundle {
  val data = UInt(32.W)
  val strobes = UInt(4.W)
}

class RamResponse extends Bundle {
  val data = UInt(32.W)
  val last = Bool()
  val error = Bool()
}

class RamPort extends Bundle {
  val req = Decoupled(new RamRequest)
  val w = Decoupled(new RamWrite)
  val rsp = Flipped(Valid(new RamResponse))
}

class RamBridgeIO extends Bundle {
  val i_valid = Input(Bool())
  val i_addr = Input(UInt(32.W))
  val i_ready = Output(Bool())
  val i_data = Output(UInt(32.W))
  val i_error = Output(Bool())
  val d_valid = Input(Bool())
  val d_addr = Input(UInt(32.W))
  val d_wdata = Input(UInt(32.W))
  val d_wstrb = Input(UInt(4.W))
  val d_ready = Output(Bool())
  val d_rdata = Output(UInt(32.W))
  val d_error = Output(Bool())
  val ram = new RamPort
}

// Core ports to one RamPort, uncached: every fetch and data access becomes a
// single-word request, data first. This is the functional reference for a
// back end and the place a cache will be inserted; it is not fast.
class RamBridge extends Module {
  val io = IO(new RamBridgeIO)
  val idle :: request :: writeData :: response :: Nil = Enum(4)
  val state = RegInit(idle)
  val data = Reg(Bool())
  val write = Reg(Bool())
  // The core holds a request stable until its ready, so the choice made in
  // idle stays valid; a fetch is never withdrawn, and neither is a data access.
  when(state === idle && (io.d_valid || io.i_valid)) {
    data := io.d_valid
    write := io.d_valid && io.d_wstrb =/= 0.U
    state := request
  }
  io.ram.req.valid := state === request
  io.ram.req.bits.address := Mux(data, io.d_addr, io.i_addr)
  io.ram.req.bits.write := write
  io.ram.req.bits.beats := 0.U
  io.ram.w.valid := state === writeData
  io.ram.w.bits.data := io.d_wdata
  io.ram.w.bits.strobes := io.d_wstrb
  when(io.ram.req.fire) { state := Mux(write, writeData, response) }
  when(io.ram.w.fire) { state := response }
  val done = state === response && io.ram.rsp.valid
  when(done) { state := idle }
  io.i_ready := done && !data
  io.i_data := io.ram.rsp.bits.data
  io.i_error := io.ram.rsp.bits.error
  io.d_ready := done && data
  io.d_rdata := io.ram.rsp.bits.data
  io.d_error := io.ram.rsp.bits.error
}

object GenerateBridge extends App {
  require(args.length == 1, "usage: runMain r86chip.GenerateBridge OUTPUT_DIRECTORY")
  _root_.circt.stage.ChiselStage.emitSystemVerilogFile(new RamBridge, Array("--target-dir", args(0)),
    Array("--disable-all-randomization"))
}
