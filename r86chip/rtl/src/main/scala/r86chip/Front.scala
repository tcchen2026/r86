package r86chip

import chisel3._
import chisel3.util._

class FrontWord extends Bundle {
  val pc = UInt(32.W)
  val word = UInt(32.W)
  val error = Bool()
  // Fetch already continued at `target` (a native word address) after this.
  val predicted = Bool()
  val target = UInt(30.W)
  // The target table had a row for this PC, with this counter.
  val hit = Bool()
  val counter = UInt(2.W)
}

class FrontIO extends Bundle {
  // From the back end: start again at flush_target. Registered there.
  val flush = Input(Bool())
  val flush_target = Input(UInt(32.W))
  val stopped = Input(Bool())
  val i_addr = Output(UInt(32.W))
  val i_check = Output(Bool())
  val i_hit = Input(Bool())
  val i_word = Input(UInt(32.W))
  val i_error = Input(Bool())
  // Oldest fetched word; `take` removes it.
  val valid = Output(Bool())
  val head = Output(new FrontWord)
  val take = Input(Bool())
  // Training from the commit stage: write (or, with train_live low, remove)
  // the row of train_pc. Row: target30, kind2, counter2.
  val train = Input(Bool())
  val train_pc = Input(UInt(32.W))
  val train_live = Input(Bool())
  val train_row = Input(UInt(34.W))
  // Top of the guest return stack, for XJT.R.
  val ras_valid = Input(Bool())
  val ras_native = Input(UInt(30.W))
  // Event pulses for the testbench counters: decode-time prediction, rewind.
  val events = Output(UInt(2.W))
}

object Front {
  // Row kinds of the target table.
  val conditional = 0
  val direct = 1
  val indirect = 2
}

// Fetch and prediction. One address is presented to the instruction cache
// per clock and its word arrives in the next (F1). Fetch never waits for that
// answer: it goes on to the next address at once, and a miss rewinds to the
// missed address until the cache holds it. Words queue in a four-entry
// buffer whose head is the decode stage.
//
// Prediction is one table of 1024 rows in block RAM (tag20, target30, kind2,
// counter2), read with the instruction: a row whose branch is expected to be
// taken sends the next fetch to its target, at no cost. Rows for JAL are
// written when decode first sees one (two clocks, as is XJT.R predicted by
// the top of the guest return stack); rows for conditional branches and JALR
// are written by the back end. Every prediction is only a hint that the back
// end checks.
class Front extends Module {
  val io = IO(new FrontIO)
  val pc = RegInit("h1000".U(32.W))
  val f1Valid = RegInit(false.B)
  val npc = RegInit("h1000".U(32.W))
  val rewind = RegInit(false.B)
  val rewindPC = Reg(UInt(32.W))
  val slots = Reg(Vec(4, new FrontWord))
  val first = RegInit(0.U(2.W))
  val count = RegInit(0.U(3.W))
  val table = new EpochTable(clock, 10, 54)

  // Decode-time prediction on the head word.
  val head = slots(first)
  val headValid = count =/= 0.U
  val word = head.word
  val jal = word(6, 0) === "h6f".U
  val ret = word(6, 0) === "h6b".U && word(14, 7) === 0.U && word(31, 20) === 1.U
  val direct = head.pc +
    Cat(Fill(11, word(31)), word(31), word(19, 12), word(20), word(30, 21), 0.U(1.W))
  val predictTarget = Mux(ret, Cat(io.ras_native, 0.U(2.W)), direct)
  val predict = headValid && !head.error && !head.predicted && !io.flush && !io.stopped &&
    head.pc(1, 0) === 0.U && Mux(ret, io.ras_valid, jal && direct(1, 0) === 0.U)

  // F1: the word for `pc`, and the table's row for it.
  val good = f1Valid && !io.flush && !rewind && !predict
  val row = table.payload
  val rowHit = table.live && !table.sweeping && row(53, 34) === pc(31, 12)
  val rowTarget = row(33, 4)
  val known = good && rowHit && (row(3, 2) =/= Front.conditional.U || row(1))
  val arrive = good && (io.i_hit || io.i_error)
  val missed = good && !io.i_hit && !io.i_error
  io.i_check := good
  io.events := Cat(missed, predict)

  val room = Mux(rewind, count <= 3.U, count +& f1Valid.asUInt <= 3.U)
  val launch = !io.stopped && (io.flush || predict || room)
  val address = Mux(io.flush, io.flush_target, Mux(predict, predictTarget,
    Mux(rewind, rewindPC, Mux(known, Cat(rowTarget, 0.U(2.W)), npc))))
  io.i_addr := address
  table.readIndex := address(11, 2)
  f1Valid := launch
  when(launch) {
    pc := address
    npc := address + 4.U
    rewind := false.B
  }.elsewhen(known) { npc := Cat(rowTarget, 0.U(2.W)) }
  when(missed) {
    rewind := true.B
    rewindPC := pc
  }
  when(io.flush || predict) { rewind := false.B }

  // The buffer. A decode-time prediction keeps only the head.
  val last = first + count(1, 0)
  val take = io.take && headValid
  io.valid := headValid && !io.flush
  io.head := head
  when(predict) {
    io.head.predicted := true.B
    io.head.target := predictTarget(31, 2)
  }
  when(io.flush) {
    count := 0.U
  }.elsewhen(predict) {
    slots(first).predicted := true.B
    slots(first).target := predictTarget(31, 2)
    count := Mux(take, 0.U, 1.U)
    when(take) { first := first + 1.U }
  }.otherwise {
    when(arrive) {
      slots(last).pc := pc
      slots(last).word := io.i_word
      slots(last).error := io.i_error
      slots(last).predicted := known
      slots(last).target := rowTarget
      slots(last).hit := rowHit
      slots(last).counter := row(1, 0)
    }
    count := count + arrive.asUInt - take.asUInt
    when(take) { first := first + 1.U }
  }

  // The back end's training has the write port first; a JAL seen at decode
  // is learned when the port is free.
  val learn = predict && !ret
  table.write := (io.train || learn) && !table.sweeping
  table.writeLive := !io.train || io.train_live
  table.writeIndex := Mux(io.train, io.train_pc(11, 2), head.pc(11, 2))
  table.writePayload := Mux(io.train, Cat(io.train_pc(31, 12), io.train_row),
    Cat(head.pc(31, 12), direct(31, 2), Front.direct.U(2.W), 3.U(2.W)))
}
