package r86chip

import chisel3._
import chisel3.util._

class RamIO(addr: Int, width: Int) extends Bundle {
  val clk = Input(Clock())
  val read_addr = Input(UInt(addr.W))
  val read_data = Output(UInt(width.W))
  val write_enable = Input(Bool())
  val write_addr = Input(UInt(addr.W))
  val write_data = Input(UInt(width.W))
}

// Technology boundaries implemented in r86_ram.v.
class BlockRam(addr: Int, width: Int) extends ExtModule(Map(
  "ADDR" -> IntParam(addr), "WIDTH" -> IntParam(width)
)) {
  override def desiredName: String = "r86_blockram"
  val io = FlatIO(new RamIO(addr, width))
}

class LutRam(addr: Int, width: Int) extends ExtModule(Map(
  "ADDR" -> IntParam(addr), "WIDTH" -> IntParam(width)
)) {
  override def desiredName: String = "r86_lutram"
  val io = FlatIO(new RamIO(addr, width))
}

// Software-filled direct-mapped table in block RAM; state inside Core, not a
// module. `live` and `payload` describe the row whose index was presented on
// the previous clock edge. A row is live when its stored epoch equals the
// current one. Flush advances the epoch. Every row is zeroed after reset and
// whenever the epoch wraps, and the current epoch is never zero, so a dead or
// swept row cannot match. Users must not read, write or flush while `sweeping`.
class EpochTable(clock: Clock, indexBits: Int, payloadBits: Int) {
  private val ram = Module(new BlockRam(indexBits, 8 + payloadBits))
  private val epoch = RegInit(1.U(8.W))
  private val row = RegInit(0.U(indexBits.W))
  val sweeping = RegInit(true.B)
  val readIndex = Wire(UInt(indexBits.W))
  val write = WireDefault(false.B)
  val writeLive = WireDefault(false.B)
  val writeIndex = Wire(UInt(indexBits.W))
  val writePayload = Wire(UInt(payloadBits.W))
  val flush = WireDefault(false.B)
  ram.io.clk := clock
  ram.io.read_addr := readIndex
  ram.io.write_enable := sweeping || write
  ram.io.write_addr := Mux(sweeping, row, writeIndex)
  ram.io.write_data := Mux(sweeping || !writeLive, 0.U, Cat(epoch, writePayload))
  val live = ram.io.read_data(payloadBits + 7, payloadBits) === epoch
  val payload = ram.io.read_data(payloadBits - 1, 0)
  when(sweeping) {
    row := row + 1.U
    when(row.andR) { sweeping := false.B }
  }
  when(flush) {
    when(epoch === 255.U) {
      epoch := 1.U
      sweeping := true.B
    }.otherwise { epoch := epoch + 1.U }
  }
}
