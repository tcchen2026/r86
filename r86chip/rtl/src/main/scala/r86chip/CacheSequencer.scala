package r86chip

import chisel3._
import chisel3.util._

class CacheSequencerIO(i: CacheShape, d: CacheShape) extends Bundle {
  // Requests are levels held by the owner until `taken`.
  val want_i = Input(Bool())
  val want_d = Input(Bool())
  val want_sync = Input(Bool())
  val i_line = Input(UInt(27.W))
  val d_line = Input(UInt(27.W))
  val take_i = Output(Bool())
  val take_d = Output(Bool())
  val take_sync = Output(Bool())
  val synced = Output(Bool())
  val active = Output(Bool())
  // The storage read ports follow read_addr (not the owner's addresses).
  val steer = Output(Bool())
  // Result of a fill whose RAM answered with an error.
  val failed = Output(Bool())
  val failed_i = Output(Bool())
  val failed_line = Output(UInt(27.W))
  // Storage access while active.
  val read_addr = Output(UInt(32.W))
  val i_row = Input(UInt(i.rowBits.W))
  val d_row = Input(UInt(d.rowBits.W))
  val d_words = Input(Vec(2, UInt(32.W)))
  val i_row_write = Output(Bool())
  val i_row_value = Output(UInt(i.rowBits.W))
  val d_row_write = Output(Bool())
  val d_row_value = Output(UInt(d.rowBits.W))
  val word_write_i = Output(Bool())
  val word_write_d = Output(Bool())
  val word_way = Output(Bool())
  val word_beat = Output(UInt(3.W))
  val line = Output(UInt(27.W))
  val changed_read = Output(UInt((d.setBits + 1).W))
  val changed_data = Input(Bool())
  val changed_write = Output(Bool())
  val changed_addr = Output(UInt((d.setBits + 1).W))
  val ram = new RamPort
}

// Fills lines and writes changed data lines back, one job at a time: a data
// line, an instruction line, or a pass over every data set (sync). The owner
// starts a job only when no store can commit meanwhile. Before an instruction
// line is filled, a changed data line of the same address is written back.
// The way to replace is an absent one if there is one, otherwise they
// alternate.
class CacheSequencer(i: CacheShape, d: CacheShape) extends Module {
  val io = IO(new CacheSequencerIO(i, d))
  val sIdle :: sLook0 :: sLook1 :: sLook2 :: sChoose :: sBackReq :: sBackRead :: sBackData :: sBackRsp :: sFillReq :: sFill :: sFillRow :: sSettle :: sScanStep :: Nil = Enum(14)
  val state = RegInit(sIdle)
  val forI = Reg(Bool())
  val scanning = RegInit(false.B)
  val line = Reg(UInt(27.W))
  val way = Reg(Bool())
  val bad = Reg(Bool())
  val iHold = Reg(UInt(i.rowBits.W))
  val dHold = Reg(UInt(d.rowBits.W))
  val changed = Reg(UInt(2.W))
  val backWay = Reg(Bool())
  val beat = Reg(UInt(3.W))
  val turn = RegInit(false.B)

  val dIndex = line(d.setBits - 1, 0)
  val dTag = line(25, d.setBits)
  val iIndex = line(i.setBits - 1, 0)
  val iTag = line(25, i.setBits)
  val backField = Mux(backWay, d.way(dHold, 1), d.way(dHold, 0))

  io.take_i := false.B
  io.take_d := false.B
  io.take_sync := false.B
  io.synced := false.B
  io.active := state =/= sIdle
  io.steer := state =/= sIdle && state =/= sSettle
  io.failed := false.B
  io.failed_i := forI
  io.failed_line := line
  io.read_addr := Cat(line, 0.U(5.W))
  io.i_row_write := false.B
  io.i_row_value := i.put(iHold, way, Cat(!bad, iTag))
  io.d_row_write := false.B
  io.d_row_value := d.put(dHold, way, Cat(!bad, dTag))
  io.word_write_i := false.B
  io.word_write_d := false.B
  io.word_way := way
  io.word_beat := beat
  io.line := line
  io.changed_read := Cat(dIndex, 0.U(1.W))
  io.changed_write := false.B
  io.changed_addr := Cat(dIndex, backWay)
  io.ram.req.valid := false.B
  io.ram.req.bits.address := Cat(line, 0.U(5.W))
  io.ram.req.bits.write := false.B
  io.ram.req.bits.beats := 7.U
  io.ram.w.valid := false.B
  io.ram.w.bits.data := Mux(backWay, io.d_words(1), io.d_words(0))
  io.ram.w.bits.strobes := 15.U

  switch(state) {
    is(sIdle) {
      bad := false.B
      // The data request is the older one in program order.
      when(io.want_d) {
        io.take_d := true.B
        forI := false.B
        scanning := false.B
        line := io.d_line
        state := sLook0
      }.elsewhen(io.want_i) {
        io.take_i := true.B
        forI := true.B
        scanning := false.B
        line := io.i_line
        state := sLook0
      }.elsewhen(io.want_sync) {
        io.take_sync := true.B
        forI := false.B
        scanning := true.B
        line := 0.U
        state := sLook0
      }
    }
    // Rows of the line's two sets, then the data set's two changed bits.
    is(sLook0) { state := sLook1 }
    is(sLook1) {
      iHold := io.i_row
      dHold := io.d_row
      changed := Cat(0.U(1.W), io.changed_data)
      io.changed_read := Cat(dIndex, 1.U(1.W))
      state := sLook2
    }
    is(sLook2) {
      changed := Cat(io.changed_data, changed(0))
      state := sChoose
    }
    is(sChoose) {
      val dHas = (0 until 2).map(w => d.holds(dHold, w, dTag))
      val iHas = (0 until 2).map(w => i.holds(iHold, w, iTag))
      val dLive = (0 until 2).map(w => d.present(d.way(dHold, w)))
      when(scanning) {
        // Write back each changed way of this set, lower way first.
        backWay := !(dLive(0) && changed(0))
        state := Mux((dLive(0) && changed(0)) || (dLive(1) && changed(1)), sBackReq, sScanStep)
      }.elsewhen(forI) {
        way := Mux(!i.present(i.way(iHold, 0)), false.B, Mux(!i.present(i.way(iHold, 1)), true.B, turn))
        backWay := dHas(1)
        state := Mux(iHas(0) || iHas(1), sSettle,
          Mux((dHas(0) && changed(0)) || (dHas(1) && changed(1)), sBackReq, sFillReq))
      }.otherwise {
        val w = Mux(!dLive(0), false.B, Mux(!dLive(1), true.B, turn))
        way := w
        backWay := w
        state := Mux(dHas(0) || dHas(1), sSettle,
          Mux(Mux(w, dLive(1) && changed(1), dLive(0) && changed(0)), sBackReq, sFillReq))
      }
    }
    is(sBackReq) {
      io.ram.req.valid := true.B
      io.ram.req.bits.address := Cat(0.U(1.W), d.tagOf(backField), dIndex, 0.U(5.W))
      io.ram.req.bits.write := true.B
      beat := 0.U
      when(io.ram.req.ready) { state := sBackRead }
    }
    is(sBackRead) {
      io.read_addr := Cat(line(26, d.setBits), dIndex, beat, 0.U(2.W))
      state := sBackData
    }
    is(sBackData) {
      io.read_addr := Cat(line(26, d.setBits), dIndex, beat, 0.U(2.W))
      io.ram.w.valid := true.B
      when(io.ram.w.ready) {
        beat := beat + 1.U
        state := Mux(beat === 7.U, sBackRsp, sBackRead)
      }
    }
    is(sBackRsp) {
      when(io.ram.rsp.valid) {
        io.changed_write := true.B
        changed := changed & Mux(backWay, 1.U(2.W), 2.U(2.W))
        // A scan returns to look for the other way; a fill goes on.
        state := Mux(scanning, sChoose, sFillReq)
      }
    }
    is(sFillReq) {
      io.ram.req.valid := true.B
      beat := 0.U
      when(io.ram.req.ready) { state := sFill }
    }
    is(sFill) {
      when(io.ram.rsp.valid) {
        io.word_write_i := forI
        io.word_write_d := !forI
        bad := bad || io.ram.rsp.bits.error
        beat := beat + 1.U
        when(beat === 7.U) { state := sFillRow }
      }
    }
    is(sFillRow) {
      // A failed fill leaves the way absent: its words were overwritten.
      io.i_row_write := forI
      io.d_row_write := !forI
      io.changed_write := !forI
      io.changed_addr := Cat(dIndex, way)
      io.failed := bad
      turn := !turn
      state := sSettle
    }
    // One clock for the block RAMs to be read at the owner's addresses again.
    is(sSettle) { state := sIdle }
    is(sScanStep) {
      line := line + 1.U
      io.synced := dIndex.andR
      state := Mux(dIndex.andR, sSettle, sLook0)
    }
  }
}
