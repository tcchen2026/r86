package r86chip

import chisel3._

class SystemIO extends Bundle {
  val ram = new RamPort
  val dev_valid = Output(Bool())
  val dev_addr = Output(UInt(32.W))
  val dev_wdata = Output(UInt(32.W))
  val dev_wstrb = Output(UInt(4.W))
  val dev_ready = Input(Bool())
  val dev_rdata = Input(UInt(32.W))
  val dev_error = Input(Bool())
  val sync = Input(Bool())
  val synced = Output(Bool())
  val trap = Output(Bool())
  val trap_pc = Output(UInt(32.W))
  val trap_cause = Output(UInt(4.W))
  val trap_value = Output(UInt(32.W))
  val retire_valid = Output(Bool())
  val retire_pc = Output(UInt(32.W))
  val retire_word = Output(UInt(32.W))
  val retire_bank = Output(Bool())
  val retire_write = Output(Bool())
  val retire_rd = Output(UInt(6.W))
  val retire_value = Output(UInt(32.W))
  val events = Output(UInt(16.W))
}

// Core plus caches: the unit a board top level connects to a RAM back end.
class System(banks: Int, iSetBits: Int, dSetBits: Int) extends Module {
  override def desiredName: String = "r86_system"
  val io = IO(new SystemIO)
  val cache = Module(new Cache(iSetBits, dSetBits))
  val core = Module(new Core(banks, cache.dShape))
  cache.io.i_addr := core.io.i_addr
  cache.io.i_check := core.io.i_check
  cache.io.i_flush := core.io.i_flush
  core.io.i_hit := cache.io.i_hit
  core.io.i_word := cache.io.i_word
  core.io.i_error := cache.io.i_error
  cache.io.d_addr := core.io.d_addr
  cache.io.d_take := core.io.d_take
  cache.io.d_fill := core.io.d_fill
  cache.io.d_fill_line := core.io.d_fill_line
  core.io.d_hit := cache.io.d_hit
  core.io.d_way := cache.io.d_way
  core.io.d_word := cache.io.d_word
  core.io.d_words := cache.io.d_words
  core.io.d_error := cache.io.d_error
  cache.io.s_valid := core.io.s_valid
  cache.io.s_slot := core.io.s_slot
  cache.io.s_way := core.io.s_way
  cache.io.s_data := core.io.s_data
  cache.io.s_strb := core.io.s_strb
  cache.io.quiet := core.io.quiet
  core.io.busy := cache.io.busy
  io.ram <> cache.io.ram
  cache.io.sync := io.sync
  io.synced := cache.io.synced
  io.dev_valid := core.io.dev_valid
  io.dev_addr := core.io.dev_addr
  io.dev_wdata := core.io.dev_wdata
  io.dev_wstrb := core.io.dev_wstrb
  core.io.dev_ready := io.dev_ready
  core.io.dev_rdata := io.dev_rdata
  core.io.dev_error := io.dev_error
  io.trap := core.io.trap
  io.trap_pc := core.io.trap_pc
  io.trap_cause := core.io.trap_cause
  io.trap_value := core.io.trap_value
  io.retire_valid := core.io.retire_valid
  io.retire_pc := core.io.retire_pc
  io.retire_word := core.io.retire_word
  io.retire_bank := core.io.retire_bank
  io.retire_write := core.io.retire_write
  io.retire_rd := core.io.retire_rd
  io.retire_value := core.io.retire_value
  io.events := core.io.events
}

object GenerateSystem extends App {
  require(args.length == 4,
    "usage: runMain r86chip.GenerateSystem BANKS I_SET_BITS D_SET_BITS OUTPUT_DIRECTORY")
  _root_.circt.stage.ChiselStage.emitSystemVerilogFile(
    new System(args(0).toInt, args(1).toInt, args(2).toInt),
    Array("--target-dir", args(3), "--split-verilog"),
    Array("--disable-all-randomization",
      "--lowering-options=disallowLocalVariables,disallowPackedArrays,noAlwaysComb"))
}
