package r86chip

import chisel3._

class RegFileIO extends Bundle {
  val clk = Input(Clock())
  val read_enable = Input(Bool())
  val read_a = Input(UInt(6.W))
  val read_b = Input(UInt(6.W))
  val value_a = Output(UInt(32.W))
  val value_b = Output(UInt(32.W))
  val write_enable = Input(Bool())
  val write_addr = Input(UInt(6.W))
  val write_value = Input(UInt(32.W))
}

// Synchronous 2R1W storage boundary, implemented in r86_regfile.v. Both read
// outputs hold during stalls; same-edge writes bypass using the full bank tag.
class RegFile(banks: Int) extends ExtModule(Map(
  "BANKS" -> IntParam(banks), "RAM_STYLE" -> StringParam("block")
)) {
  override def desiredName: String = "r86_regfile"
  val io = FlatIO(new RegFileIO)
}
