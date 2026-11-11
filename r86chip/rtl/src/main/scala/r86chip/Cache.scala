package r86chip

import chisel3._
import chisel3.util._

class CacheIO extends Bundle {
  val i_valid = Input(Bool())
  val i_addr = Input(UInt(32.W))
  val i_next_addr = Input(UInt(32.W))
  val i_flush = Input(Bool())
  val i_ready = Output(Bool())
  val i_data = Output(UInt(32.W))
  val i_error = Output(Bool())
  val d_valid = Input(Bool())
  val d_addr = Input(UInt(32.W))
  val d_next_addr = Input(UInt(32.W))
  val d_wdata = Input(UInt(32.W))
  val d_wstrb = Input(UInt(4.W))
  val d_ready = Output(Bool())
  val d_rdata = Output(UInt(32.W))
  val d_error = Output(Bool())
  val ram = new RamPort
  // Addresses with bit 31 set are devices: single-beat, never cached, never
  // fetched. Same handshake as the core's data port.
  val dev_valid = Output(Bool())
  val dev_ready = Input(Bool())
  val dev_rdata = Input(UInt(32.W))
  val dev_error = Input(Bool())
  // A rising edge on sync writes every dirty line back; synced pulses after.
  val sync = Input(Bool())
  val synced = Output(Bool())
}

// Instruction and data caches between the core's two ports and one RamPort.
// Both are direct mapped with 32-byte lines and 2^lineBits lines, data and
// tags in block RAM. A hit answers in the request's own clock: the block RAMs
// are read at the core's lookahead addresses, and a request whose address was
// not the one looked up simply waits one clock.
//
// The data cache is write-back. The instruction cache is read-only and is
// emptied by FENCE.I (i_flush). Before an instruction line is filled, a dirty
// data line holding the same address is written back, so fetch after FENCE.I
// always sees completed stores. One fill or write-back runs at a time and
// both caches wait meanwhile.
class Cache(lineBits: Int) extends Module {
  require(lineBits >= 4 && lineBits <= 16)
  val io = IO(new CacheIO)
  val tagBits = 26 - lineBits
  def line(a: UInt): UInt = a(31, 5)
  def index(a: UInt): UInt = a(4 + lineBits, 5)
  def tag(a: UInt): UInt = a(30, 5 + lineBits)
  def slot(a: UInt): UInt = a(4 + lineBits, 2)
  // Tag rows: instruction (tag), data (dirty, tag). Liveness is the epoch.
  val itag = new EpochTable(clock, lineBits, tagBits)
  val dtag = new EpochTable(clock, lineBits, tagBits + 1)
  val idata = Module(new BlockRam(lineBits + 3, 32))
  val ddata = Module(new BlockRam(lineBits + 3, 32))
  idata.io.clk := clock
  ddata.io.clk := clock

  val sIdle :: sSnoop0 :: sSnoop1 :: sWbReq :: sWbRead :: sWbData :: sWbRsp :: sFillReq :: sFill :: sFillTag :: sSettle :: sScan0 :: sScan1 :: sScanNext :: Nil = Enum(14)
  val state = RegInit(sIdle)
  val idle = state === sIdle
  val fillI = Reg(Bool())
  val fillLine = Reg(UInt(27.W))
  val fillError = Reg(Bool())
  val wbIndex = Reg(UInt(lineBits.W))
  val wbTag = Reg(UInt(tagBits.W))
  val afterWb = Reg(chiselTypeOf(state))
  val beat = Reg(UInt(3.W))
  val scanIndex = Reg(UInt(lineBits.W))
  val syncSeen = RegInit(false.B)
  // A failed fill is reported to the request that caused it.
  val errValid = RegInit(false.B)
  val errI = Reg(Bool())
  val errLine = Reg(UInt(27.W))
  // Word addresses the block RAM outputs currently describe.
  val iRead = Reg(UInt(30.W))
  val dRead = Reg(UInt(30.W))
  // A block RAM read that collides with a write is not used: the word and
  // the tag row written on the previous edge are forwarded for one clock.
  val storeValid = RegInit(false.B)
  val storeAddr = Reg(UInt(30.W))
  val storeWord = Reg(UInt(32.W))
  val markValid = RegInit(false.B)
  val markIndex = Reg(UInt(lineBits.W))
  val markRow = Reg(UInt((tagBits + 1).W))

  itag.readIndex := index(io.i_next_addr)
  idata.io.read_addr := slot(io.i_next_addr)
  iRead := io.i_next_addr(31, 2)
  dtag.readIndex := index(io.d_next_addr)
  ddata.io.read_addr := slot(io.d_next_addr)
  dRead := io.d_next_addr(31, 2)
  itag.flush := io.i_flush
  itag.writeIndex := fillLine(lineBits - 1, 0)
  itag.writePayload := fillLine(25, lineBits)
  dtag.writeIndex := wbIndex
  dtag.writePayload := Cat(0.U(1.W), wbTag)
  idata.io.write_enable := false.B
  idata.io.write_addr := Cat(fillLine(lineBits - 1, 0), beat)
  idata.io.write_data := io.ram.rsp.bits.data
  ddata.io.write_enable := false.B
  ddata.io.write_addr := Cat(fillLine(lineBits - 1, 0), beat)
  ddata.io.write_data := io.ram.rsp.bits.data
  io.ram.req.valid := false.B
  io.ram.req.bits.address := Cat(fillLine, 0.U(5.W))
  io.ram.req.bits.write := false.B
  io.ram.req.bits.beats := 7.U
  io.ram.w.valid := false.B
  io.ram.w.bits.data := ddata.io.read_data
  io.ram.w.bits.strobes := 15.U
  io.synced := false.B
  storeValid := false.B
  markValid := false.B

  val ready = idle && !itag.sweeping && !dtag.sweeping
  val iCached = io.i_valid && !io.i_addr(31)
  val iAligned = iRead === io.i_addr(31, 2)
  val iHit = ready && iCached && iAligned && itag.live && itag.payload === tag(io.i_addr)
  val iErr = idle && io.i_valid &&
    (io.i_addr(31) || (errValid && errI && errLine === line(io.i_addr)))
  val iMiss = ready && iCached && iAligned && !iHit && !iErr
  io.i_ready := iHit || iErr
  io.i_data := idata.io.read_data
  io.i_error := iErr

  val dDevice = io.d_valid && io.d_addr(31)
  val dCached = io.d_valid && !io.d_addr(31)
  val dAligned = dRead === io.d_addr(31, 2)
  val marked = markValid && markIndex === index(io.d_addr)
  val dLive = marked || dtag.live
  val dRow = Mux(marked, markRow, dtag.payload)
  val dDirty = dRow(tagBits)
  val dTagHit = dLive && dRow(tagBits - 1, 0) === tag(io.d_addr)
  val dErr = idle && dCached && errValid && !errI && errLine === line(io.d_addr)
  val dHit = ready && dCached && dAligned && dTagHit && !dErr
  val dMiss = ready && dCached && dAligned && !dTagHit && !dErr
  val dWord = Mux(storeValid && storeAddr === io.d_addr(31, 2), storeWord, ddata.io.read_data)
  val merged = Cat((3 to 0 by -1).map(n =>
    Mux(io.d_wstrb(n), io.d_wdata(8 * n + 7, 8 * n), dWord(8 * n + 7, 8 * n))))
  io.dev_valid := dDevice
  io.d_ready := Mux(dDevice, io.dev_ready, dHit || dErr)
  io.d_rdata := Mux(dDevice, io.dev_rdata, dWord)
  io.d_error := Mux(dDevice, io.dev_error, dErr)
  when(iErr && !io.i_addr(31)) { errValid := false.B }
  when(dErr) { errValid := false.B }
  when(dHit && io.d_wstrb =/= 0.U) {
    ddata.io.write_enable := true.B
    ddata.io.write_addr := slot(io.d_addr)
    ddata.io.write_data := merged
    storeValid := true.B
    storeAddr := io.d_addr(31, 2)
    storeWord := merged
    when(!dDirty) {
      dtag.write := true.B
      dtag.writeLive := true.B
      dtag.writeIndex := index(io.d_addr)
      dtag.writePayload := Cat(1.U(1.W), tag(io.d_addr))
      markValid := true.B
      markIndex := index(io.d_addr)
      markRow := Cat(1.U(1.W), tag(io.d_addr))
    }
  }

  when(!io.sync) { syncSeen := false.B }
  val fillTag = fillLine(25, lineBits)
  switch(state) {
    is(sIdle) {
      fillError := false.B
      when(dMiss) {
        // The older access first. Its victim, if dirty, goes back to RAM.
        fillI := false.B
        fillLine := line(io.d_addr)
        wbIndex := index(io.d_addr)
        wbTag := dRow(tagBits - 1, 0)
        afterWb := sFillReq
        state := Mux(dLive && dDirty, sWbReq, sFillReq)
      }.elsewhen(iMiss) {
        fillI := true.B
        fillLine := line(io.i_addr)
        wbIndex := index(io.i_addr)
        state := sSnoop0
      }.elsewhen(ready && io.sync && !syncSeen) {
        // Taken once per rising edge, when no access is waiting.
        syncSeen := true.B
        scanIndex := 0.U
        state := sScan0
      }
    }
    is(sSnoop0) { dtag.readIndex := wbIndex; state := sSnoop1 }
    is(sSnoop1) {
      wbTag := fillTag
      afterWb := sFillReq
      state := Mux(dtag.live && dtag.payload(tagBits) && dtag.payload(tagBits - 1, 0) === fillTag,
        sWbReq, sFillReq)
    }
    is(sWbReq) {
      io.ram.req.valid := true.B
      io.ram.req.bits.address := Cat(0.U(1.W), wbTag, wbIndex, 0.U(5.W))
      io.ram.req.bits.write := true.B
      beat := 0.U
      when(io.ram.req.ready) { state := sWbRead }
    }
    is(sWbRead) { ddata.io.read_addr := Cat(wbIndex, beat); state := sWbData }
    is(sWbData) {
      ddata.io.read_addr := Cat(wbIndex, beat)
      io.ram.w.valid := true.B
      when(io.ram.w.ready) {
        beat := beat + 1.U
        state := Mux(beat === 7.U, sWbRsp, sWbRead)
      }
    }
    is(sWbRsp) {
      when(io.ram.rsp.valid) {
        dtag.write := true.B
        dtag.writeLive := true.B
        state := afterWb
      }
    }
    is(sFillReq) {
      io.ram.req.valid := true.B
      beat := 0.U
      when(io.ram.req.ready) { state := sFill }
    }
    is(sFill) {
      when(io.ram.rsp.valid) {
        idata.io.write_enable := fillI
        ddata.io.write_enable := !fillI
        fillError := fillError || io.ram.rsp.bits.error
        beat := beat + 1.U
        when(beat === 7.U) { state := sFillTag }
      }
    }
    is(sFillTag) {
      // A failed fill leaves no line: its words were overwritten.
      itag.write := fillI
      itag.writeLive := !fillError
      dtag.write := !fillI
      dtag.writeLive := !fillError
      dtag.writeIndex := fillLine(lineBits - 1, 0)
      dtag.writePayload := Cat(0.U(1.W), fillTag)
      when(fillError) {
        errValid := true.B
        errI := fillI
        errLine := fillLine
      }
      state := sSettle
    }
    // One clock for the block RAMs to be read again at the waiting addresses.
    is(sSettle) { state := sIdle }
    is(sScan0) { dtag.readIndex := scanIndex; state := sScan1 }
    is(sScan1) {
      wbIndex := scanIndex
      wbTag := dtag.payload(tagBits - 1, 0)
      afterWb := sScanNext
      state := Mux(dtag.live && dtag.payload(tagBits), sWbReq, sScanNext)
    }
    is(sScanNext) {
      scanIndex := scanIndex + 1.U
      io.synced := scanIndex.andR
      state := Mux(scanIndex.andR, sSettle, sScan0)
    }
  }
}

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
}

// Core plus caches: the unit a board top level connects to a RAM back end.
class System(banks: Int, lineBits: Int) extends Module {
  override def desiredName: String = "r86_system"
  val io = IO(new SystemIO)
  val core = Module(new Core(banks))
  val cache = Module(new Cache(lineBits))
  core.clk := clock
  core.reset := reset.asBool
  cache.io.i_valid := core.i_valid
  cache.io.i_addr := core.i_addr
  cache.io.i_next_addr := core.i_next_addr
  cache.io.i_flush := core.i_flush
  core.i_ready := cache.io.i_ready
  core.i_data := cache.io.i_data
  core.i_error := cache.io.i_error
  cache.io.d_valid := core.d_valid
  cache.io.d_addr := core.d_addr
  cache.io.d_next_addr := core.d_next_addr
  cache.io.d_wdata := core.d_wdata
  cache.io.d_wstrb := core.d_wstrb
  core.d_ready := cache.io.d_ready
  core.d_rdata := cache.io.d_rdata
  core.d_error := cache.io.d_error
  io.ram <> cache.io.ram
  io.dev_valid := cache.io.dev_valid
  io.dev_addr := core.d_addr
  io.dev_wdata := core.d_wdata
  io.dev_wstrb := core.d_wstrb
  cache.io.dev_ready := io.dev_ready
  cache.io.dev_rdata := io.dev_rdata
  cache.io.dev_error := io.dev_error
  cache.io.sync := io.sync
  io.synced := cache.io.synced
  io.trap := core.trap
  io.trap_pc := core.trap_pc
  io.trap_cause := core.trap_cause
  io.trap_value := core.trap_value
  io.retire_valid := core.retire_valid
  io.retire_pc := core.retire_pc
  io.retire_word := core.retire_word
  io.retire_bank := core.retire_bank
  io.retire_write := core.retire_write
  io.retire_rd := core.retire_rd
  io.retire_value := core.retire_value
}

object GenerateSystem extends App {
  require(args.length == 3, "usage: runMain r86chip.GenerateSystem BANKS LINE_BITS OUTPUT_DIRECTORY")
  _root_.circt.stage.ChiselStage.emitSystemVerilogFile(
    new System(args(0).toInt, args(1).toInt), Array("--target-dir", args(2), "--split-verilog"),
    Array("--disable-all-randomization",
      "--lowering-options=disallowLocalVariables,disallowPackedArrays,noAlwaysComb"))
}
