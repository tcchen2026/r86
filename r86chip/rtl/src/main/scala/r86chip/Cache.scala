package r86chip

import chisel3._
import chisel3.util._

// Pipeline-facing ports of the caches. A read is two-phase: an address is
// presented in one clock and answered (hit, word, error) in the next. Nothing
// here stalls the pipeline: a miss is only reported, and the core replays the
// instruction after the line has been filled.
class CacheIO(d: CacheShape) extends Bundle {
  // Instruction side. i_check says the answer is wanted; a wanted miss
  // requests its line by itself.
  val i_addr = Input(UInt(32.W))
  val i_check = Input(Bool())
  val i_hit = Output(Bool())
  val i_word = Output(UInt(32.W))
  val i_error = Output(Bool())
  val i_flush = Input(Bool())
  // Data side. d_fill requests a line; d_take consumes a reported error.
  val d_addr = Input(UInt(32.W))
  val d_hit = Output(Bool())
  val d_way = Output(Bool())
  val d_word = Output(UInt(32.W))
  val d_words = Output(Vec(2, UInt(32.W)))
  val d_error = Output(Bool())
  val d_take = Input(Bool())
  val d_fill = Input(Bool())
  val d_fill_line = Input(UInt(27.W))
  // A committed store into a line known to be present.
  val s_valid = Input(Bool())
  val s_slot = Input(UInt(d.slotBits.W))
  val s_way = Input(Bool())
  val s_data = Input(UInt(32.W))
  val s_strb = Input(UInt(4.W))
  // The core is quiet when no store can commit; busy asks it to become so.
  val quiet = Input(Bool())
  val busy = Output(Bool())
  val ram = new RamPort
  // A rising edge on sync writes every changed line back; synced pulses after.
  val sync = Input(Bool())
  val synced = Output(Bool())
}

// Instruction and data caches in front of one RamPort: two-way set
// associative, 32-byte lines, tags and data in block RAM. The data cache is
// write-back, with one RAM per byte lane so a store needs no read, and one
// "changed" bit per line in its own RAM, set at commit. The instruction cache
// is read-only and is emptied by FENCE.I (i_flush).
//
// The sequencer starts a job only when the core is quiet, so no store can
// commit into a line that is being written back or replaced. No hit is
// reported while it runs.
class Cache(iSetBits: Int, dSetBits: Int) extends Module {
  val iShape = CacheShape(iSetBits)
  val dShape = CacheShape(dSetBits)
  val io = IO(new CacheIO(dShape))
  val ic = Module(new CacheStore(iShape, 1))
  val dc = Module(new CacheStore(dShape, 4))
  val seq = Module(new CacheSequencer(iShape, dShape))
  val changed = Module(new BlockRam(dSetBits + 1, 1))
  val wantI = RegInit(false.B)
  val wantD = RegInit(false.B)
  val iLine = Reg(UInt(27.W))
  val dLine = Reg(UInt(27.W))
  val syncSeen = RegInit(false.B)
  // A failed fill is reported to the next access of that line.
  val errValid = RegInit(false.B)
  val errI = Reg(Bool())
  val errLine = Reg(UInt(27.W))

  val ready = !seq.io.active && !ic.io.sweeping && !dc.io.sweeping
  val start = ready && io.quiet && !io.s_valid
  seq.io.want_d := start && wantD
  seq.io.want_i := start && wantI
  seq.io.want_sync := start && io.sync && !syncSeen
  seq.io.i_line := iLine
  seq.io.d_line := dLine
  seq.io.i_row := ic.io.row
  seq.io.d_row := dc.io.row
  seq.io.d_words := dc.io.words
  seq.io.changed_data := changed.io.read_data(0)
  io.ram <> seq.io.ram
  io.synced := seq.io.synced
  io.busy := !ready || wantD || wantI || (io.sync && !syncSeen)

  ic.io.read_addr := Mux(seq.io.steer, seq.io.read_addr, io.i_addr)
  ic.io.flush := io.i_flush
  ic.io.row_write := seq.io.i_row_write
  ic.io.row_index := seq.io.line(iSetBits - 1, 0)
  ic.io.row_value := seq.io.i_row_value
  ic.io.word_enable := seq.io.word_write_i
  ic.io.word_way := seq.io.word_way
  ic.io.word_slot := Cat(seq.io.line(iSetBits - 1, 0), seq.io.word_beat)
  ic.io.word_data := io.ram.rsp.bits.data

  dc.io.read_addr := Mux(seq.io.steer, seq.io.read_addr, io.d_addr)
  dc.io.flush := false.B
  dc.io.row_write := seq.io.d_row_write
  dc.io.row_index := seq.io.line(dSetBits - 1, 0)
  dc.io.row_value := seq.io.d_row_value
  dc.io.word_enable := Mux(seq.io.word_write_d, 15.U, Mux(io.s_valid, io.s_strb, 0.U))
  dc.io.word_way := Mux(seq.io.word_write_d, seq.io.word_way, io.s_way)
  dc.io.word_slot := Mux(seq.io.word_write_d,
    Cat(seq.io.line(dSetBits - 1, 0), seq.io.word_beat), io.s_slot)
  dc.io.word_data := Mux(seq.io.word_write_d, io.ram.rsp.bits.data, io.s_data)

  changed.io.clk := clock
  changed.io.read_addr := seq.io.changed_read
  changed.io.write_enable := seq.io.changed_write || io.s_valid
  changed.io.write_addr := Mux(seq.io.changed_write, seq.io.changed_addr,
    Cat(io.s_slot(dSetBits + 2, 3), io.s_way))
  changed.io.write_data := !seq.io.changed_write

  val iAddr = ic.io.addr
  val dAddr = dc.io.addr
  val iErr = iAddr(31) || (errValid && errI && errLine === iAddr(31, 5))
  val dErr = errValid && !errI && errLine === dAddr(31, 5)
  io.i_hit := ready && !iAddr(31) && ic.io.hit && !iErr
  io.i_word := ic.io.word
  io.i_error := ready && iErr
  io.d_hit := ready && !dAddr(31) && dc.io.hit && !dErr
  io.d_way := dc.io.way
  io.d_word := dc.io.word
  io.d_words := dc.io.words
  io.d_error := ready && dErr

  when(io.i_check && ready && !io.i_hit && !io.i_error) {
    wantI := true.B
    iLine := iAddr(31, 5)
  }
  when(io.d_fill) {
    wantD := true.B
    dLine := io.d_fill_line
  }
  when(seq.io.take_i) { wantI := false.B }
  when(seq.io.take_d) { wantD := false.B }
  when(seq.io.take_sync) { syncSeen := true.B }
  when(!io.sync) { syncSeen := false.B }
  when(io.i_check && io.i_error && !iAddr(31)) { errValid := false.B }
  when(io.d_take && io.d_error) { errValid := false.B }
  when(seq.io.failed) {
    errValid := true.B
    errI := seq.io.failed_i
    errLine := seq.io.failed_line
  }
}
