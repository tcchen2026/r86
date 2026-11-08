package r86chip

import chisel3._
import chisel3.util._

// B1 native slice; see design/RTL.md. One outstanding fetch, one instruction
// buffer, synchronous RF read, one execute/writeback slot. BANKS is the only
// experiment parameter; this is intentionally not a configurable CPU framework.
class Core(banks: Int) extends RawModule {
  require(banks == 1 || banks == 2)
  override def desiredName: String = "r86_core"
  val clk = IO(Input(Clock()))
  val reset = IO(Input(Bool()))
  val i_valid = IO(Output(Bool()))
  val i_addr = IO(Output(UInt(32.W)))
  val i_ready = IO(Input(Bool()))
  val i_data = IO(Input(UInt(32.W)))
  val i_error = IO(Input(Bool()))
  val d_valid = IO(Output(Bool()))
  val d_addr = IO(Output(UInt(32.W)))
  val d_wdata = IO(Output(UInt(32.W)))
  val d_wstrb = IO(Output(UInt(4.W)))
  val d_ready = IO(Input(Bool()))
  val d_rdata = IO(Input(UInt(32.W)))
  val d_error = IO(Input(Bool()))
  val trap = IO(Output(Bool()))
  val trap_pc = IO(Output(UInt(32.W)))
  val trap_cause = IO(Output(UInt(4.W)))
  val trap_value = IO(Output(UInt(32.W)))
  val retire_valid = IO(Output(Bool()))
  val retire_pc = IO(Output(UInt(32.W)))
  val retire_word = IO(Output(UInt(32.W)))
  val retire_bank = IO(Output(Bool()))
  val retire_write = IO(Output(Bool()))
  val retire_rd = IO(Output(UInt(6.W)))
  val retire_value = IO(Output(UInt(32.W)))
  val retire_ea_hit = IO(Output(Bool()))

  withClockAndReset(clk, reset) {
    val bank = RegInit(false.B)
    val runtimeReturn = RegInit(0.U(32.W))
    val guestReturn = RegInit(0.U(32.W))
    val exitReason = RegInit(0.U(12.W))
    // Only six bits of each sparse x86 status word can be nonzero. Synthesis
    // removes the constant bits; these are not extra general-register ports.
    val guestFlags = RegInit(0.U(32.W))
    val pendingFlags = RegInit(0.U(32.W))
    // Shared guest lookup metadata, not an MMU on the native RV bus. Only the
    // valid bits reset; unknown payloads cannot escape an invalid slot.
    // Separate software proofs: 16 data rows (tag16, page20, R/W2) and
    // four fetch rows (tag18, page20, X1). Native banks share both views.
    val guestTlb = Reg(Vec(16, UInt(38.W)))
    val guestTlbValid = RegInit(0.U(16.W))
    val guestITlb = Reg(Vec(4, UInt(39.W)))
    val guestITlbValid = RegInit(0.U(4.W))
    val guestBasePage = RegInit(0.U(20.W))
    val guestLimitPage = RegInit(0.U(20.W))
    // Software publishes qualified x86 assumptions. The same register bank and
    // native bus run the miss handler; ordinary RV loads never use this state.
    val guestContextValid = RegInit(false.B)
    val guestControl = RegInit(0.U(6.W))
    val guestIdentity = RegInit(0.U(32.W))
    val segmentBases = Reg(Vec(6, UInt(32.W)))
    val segmentLimits = Reg(Vec(6, UInt(32.W)))
    val segmentRights = RegInit(VecInit(Seq.fill(6)(0.U(3.W))))
    val serviceVector = RegInit(0.U(32.W))
    val serviceActive = RegInit(false.B)
    val serviceRecord = RegInit(VecInit(Seq.fill(4)(0.U(32.W))))
    val eaRegisters = Reg(UInt(32.W))
    val eaConfigured = RegInit(false.B)
    val eaInfo = RegInit("hffffffff".U(32.W))
    val fetchPC = RegInit("h1000".U(32.W))
    val fetchValid = RegInit(false.B)
    val fetchAddress = RegInit("h1000".U(32.W))
    val fetchKilled = RegInit(false.B)
    val idValid = RegInit(false.B)
    val idError = Reg(Bool())
    val idPC = Reg(UInt(32.W))
    val idWord = Reg(UInt(32.W))
    val exValid = RegInit(false.B)
    val exError = Reg(Bool())
    val exBank = Reg(Bool())
    val exPC = Reg(UInt(32.W))
    val exWord = Reg(UInt(32.W))
    val mStarted = RegInit(false.B)
    // A fetch .U may cross one native word, never a guest page. Hold its
    // first bus response until the second succeeds; no intermediate writeback.
    val loadSecond = RegInit(false.B)
    val loadFirst = Reg(UInt(32.W))
    val stopped = RegInit(false.B)
    val stoppedPC = RegInit(0.U(32.W))
    val stoppedCause = RegInit(0.U(4.W))
    val stoppedValue = RegInit(0.U(32.W))

    val rf = Module(new RegFile(banks))
    rf.io.clk := clk
    val a = rf.io.value_a
    val b = rf.io.value_b
    val opcode = exWord(6, 0)
    val f3 = exWord(14, 12)
    val f7 = exWord(31, 25)
    val rd = exWord(11, 7)
    val rs1 = exWord(19, 15)
    val rs2 = exWord(24, 20)
    val immI = Cat(Fill(20, exWord(31)), exWord(31, 20))
    val immS = Cat(Fill(20, exWord(31)), exWord(31, 25), exWord(11, 7))
    val immB = Cat(Fill(19, exWord(31)), exWord(31), exWord(7), exWord(30, 25), exWord(11, 8), 0.U(1.W))
    val immJ = Cat(Fill(11, exWord(31)), exWord(31), exWord(19, 12), exWord(20), exWord(30, 21), 0.U(1.W))
    val immU = Cat(exWord(31, 12), 0.U(12.W))
    val operandB = Mux(opcode === "h13".U, immI, b)
    // RV32 Zba selects fixed wiring before the existing ADD datapath. No
    // additional variable shifter, adder, register port or execution sequence.
    val zba = opcode === "h33".U && f7 === "h10".U
    val addA = MuxLookup(Mux(zba, f3, 0.U), a)(Seq(
      2.U -> Cat(a(30, 0), 0.U(1.W)),
      4.U -> Cat(a(29, 0), 0.U(2.W)),
      6.U -> Cat(a(28, 0), 0.U(3.W))))
    val addResult = addA + operandB
    val branchTarget = exPC + immB
    val xCondition = XFlags.condition(rs1, guestFlags)

    val done = WireDefault(true.B)
    val decodeFault = WireDefault(false.B)
    val decodeCause = WireDefault(2.U(4.W))
    val decodeFaultValue = WireDefault(exWord)
    val writeRD = WireDefault(false.B)
    val writeBank = WireDefault(exBank)
    val value = WireDefault(0.U(32.W))
    val redirect = WireDefault(false.B)
    val target = WireDefault(exPC + 4.U)
    val memoryOp = WireDefault(false.B)
    val muldivOp = WireDefault(false.B)
    val enterBank = WireDefault(false.B)
    val leaveBank = WireDefault(false.B)
    val flagsAlu = WireDefault(false.B)
    val flagsLoad = WireDefault(false.B)
    val flagsCommit = WireDefault(false.B)
    val tlbFill = WireDefault(false.B)
    val tlbInvalidate = WireDefault(false.B)
    val tlbFlush = WireDefault(false.B)
    val tlbRegion = WireDefault(false.B)
    val contextWrite = WireDefault(false.B)
    val segmentWrite = WireDefault(false.B)
    val vectorWrite = WireDefault(false.B)
    val serviceEnter = WireDefault(false.B)
    val serviceReturn = WireDefault(false.B)
    val guestMemory = opcode === "h5b".U
    val store = opcode === "h23".U || (guestMemory && f3(2))
    val guestUnalignedFetch = guestMemory && !store && f7(4) && f7(5)
    val guestRegisterSegment = f7(6)
    val guestSegment = Mux(guestRegisterSegment, b(2, 0), f7(2, 0))
    val guestRequired = Mux(store, 2.U(3.W),
      Mux(f7(4), 4.U(3.W), Mux(f7(3), 3.U(3.W), 1.U(3.W))))
    // Dynamic selectors use the full old rs2 value: 0x103 is illegal, not DS.
    // Stores already need rs2 for data and cannot use the register-segment form.
    val guestEncodingValid = f3(1, 0) =/= 3.U && guestSegment < 6.U &&
      // guestSegment<6 already checks the low three bits. Reduce the remaining
      // bits instead of inferring a second, full-width magnitude comparator.
      (!guestRegisterSegment || (!b(31, 3).orR && f7(2, 0) === 0.U)) &&
      Mux(store, rd === 0.U && !f7(3) && !f7(4) && !guestRegisterSegment,
        (guestRegisterSegment || rs2 === 0.U) && (!f7(5) || f7(4))) &&
      (!f7(4) || (guestSegment === 1.U && !f7(3)))
    val guestLinear = segmentBases(guestSegment) + a
    // Include the carry: fffffffe+3 must not pass a ffffffff segment limit.
    val guestLast = a +& MuxLookup(f3(1, 0), 0.U(32.W))(Seq(
      1.U -> 1.U(32.W), 2.U -> 3.U(32.W)))
    val guestSegmentOk = (segmentRights(guestSegment) & guestRequired) === guestRequired &&
      guestLast <= segmentLimits(guestSegment)
    val tlbAddress = Mux(guestMemory, guestLinear, a)
    val tlbIndex = tlbAddress(15, 12)
    val tlbSlot = guestTlb(tlbIndex)
    val tlbMask = UIntToOH(tlbIndex, 16)
    val itlbIndex = tlbAddress(13, 12)
    val itlbSlot = guestITlb(itlbIndex)
    val itlbMask = UIntToOH(itlbIndex, 4)
    val tlbFetch = Mux(guestMemory, f7(4), exWord(23, 22) === 2.U)
    val tlbPage = Mux(tlbFetch, itlbSlot(20, 1), tlbSlot(21, 2))
    val tlbRequired = Mux(guestMemory, guestRequired,
      MuxLookup(exWord(23, 22), 1.U(3.W))(Seq(
        0.U -> 1.U(3.W), 1.U -> 2.U(3.W), 2.U -> 4.U(3.W), 3.U -> 3.U(3.W))))
    val tlbWidth = Mux(guestMemory, f3(1, 0), exWord(21, 20))
    val naturallyAligned = tlbWidth === 0.U ||
      (tlbWidth === 1.U && !tlbAddress(0)) ||
      (tlbWidth === 2.U && tlbAddress(1, 0) === 0.U)
    val fetchSamePage = tlbWidth === 0.U ||
      (tlbWidth === 1.U && tlbAddress(11, 0) =/= "hfff".U) ||
      (tlbWidth === 2.U && tlbAddress(11, 0) <= "hffc".U)
    val tlbAligned = Mux(guestUnalignedFetch, fetchSamePage, naturallyAligned)
    val dataHit = guestTlbValid(tlbIndex) && tlbSlot(37, 22) === tlbAddress(31, 16) &&
      (tlbSlot(1, 0) & tlbRequired(1, 0)) === tlbRequired(1, 0)
    val fetchHit = guestITlbValid(itlbIndex) && itlbSlot(38, 21) === tlbAddress(31, 14) && itlbSlot(0)
    val tlbHit = Mux(tlbFetch, fetchHit, dataHit) && tlbAligned
    val guestReason = Mux(!guestContextValid || !guestControl(0) || guestControl(3), 1.U(3.W),
      Mux(!guestSegmentOk, 2.U(3.W), Mux(!tlbAligned, 3.U(3.W), Mux(!tlbHit, 4.U(3.W), 0.U(3.W)))))
    val address = Mux(guestMemory, Cat(tlbPage, guestLinear(11, 0)),
      a + Mux(opcode === "h23".U, immS, immI))
    // On a single beat only the low word's requested lanes are used. On the
    // second beat select the saved low word and concatenate the new high word.
    val readBytes = (Cat(d_rdata, Mux(loadSecond, loadFirst, d_rdata)) >>
      Cat(address(1, 0), 0.U(3.W)))(31, 0)
    val lane = readBytes(15, 0)
    val guestSplitWord = guestUnalignedFetch &&
      ((f3(1, 0) === 2.U && address(1, 0) =/= 0.U) ||
       (f3(1, 0) === 1.U && address(1, 0) === 3.U))
    // Request qualification never depends on ready/error: no bus feedback loop.
    val validMemoryOp = Mux(guestMemory, guestEncodingValid && guestReason === 0.U,
      f3 <= 2.U || (!store && (f3 === 4.U || f3 === 5.U)))
    val misalignedMemory = !guestUnalignedFetch &&
      ((f3(1, 0) === 1.U && address(0)) ||
       (f3(1, 0) === 2.U && address(1, 0) =/= 0.U))
    val (guestResult, guestStatus) = XFlags.alu(a, b, f3, f7, guestFlags)
    val strobes = WireDefault(0.U(4.W))
    val md = Module(new MulDiv)
    md.io.a := a
    md.io.b := b
    md.io.operation := f3
    val eaOperation = opcode === "h7b".U && f3 === 0.U
    val eaEncodingValid = f7 === 0.U && !a(31, 8).orR && a(7, 6) =/= 3.U
    val eaFinish = WireDefault(false.B)
    val ea = Ea32(exValid && eaOperation && eaEncodingValid && !exError &&
      exPC(1, 0) === 0.U && !stopped && !reset, eaFinish, a, b,
      eaConfigured, eaRegisters, guestContextValid && guestControl(0) && !guestControl(3),
      segmentBases(1), segmentLimits(1), segmentRights(1)(2), guestITlb, guestITlbValid,
      d_ready, d_rdata, d_error)
    val eaConfigure = WireDefault(false.B)

    // Directly decode the original 32-bit word. Decode failures, target checks,
    // and fetch errors have separate wires so priority cannot form feedback.
    switch(opcode) {
      is("h37".U) { writeRD := true.B; value := immU }
      is("h17".U) { writeRD := true.B; value := exPC + immU }
      is("h6f".U) {
        writeRD := true.B; value := exPC + 4.U
        redirect := true.B; target := exPC + immJ
      }
      is("h67".U) {
        writeRD := true.B; value := exPC + 4.U
        redirect := true.B; target := (a + immI) & "hfffffffe".U
        decodeFault := f3 =/= 0.U
      }
      is("h63".U) {
        target := branchTarget
        decodeFault := f3 === 2.U || f3 === 3.U
        switch(f3) {
          is(0.U) { redirect := a === b }
          is(1.U) { redirect := a =/= b }
          is(4.U) { redirect := a.asSInt < b.asSInt }
          is(5.U) { redirect := a.asSInt >= b.asSInt }
          is(6.U) { redirect := a < b }
          is(7.U) { redirect := a >= b }
        }
      }
      is("h13".U, "h33".U) {
        writeRD := true.B
        when(opcode === "h33".U && f7 === 1.U) {
          muldivOp := true.B
          done := mStarted && md.io.done
          value := md.io.result
        }.elsewhen(zba) {
          value := addResult
          decodeFault := f3 =/= 2.U && f3 =/= 4.U && f3 =/= 6.U
        }.otherwise {
          switch(f3) {
            is(0.U) { value := Mux(opcode === "h33".U && f7 === "h20".U, a - b, addResult) }
            is(1.U) { value := a << operandB(4, 0) }
            is(2.U) { value := (a.asSInt < operandB.asSInt).asUInt }
            is(3.U) { value := (a < operandB).asUInt }
            is(4.U) { value := a ^ operandB }
            is(5.U) { value := Mux(exWord(30), (a.asSInt >> operandB(4, 0)).asUInt, a >> operandB(4, 0)) }
            is(6.U) { value := a | operandB }
            is(7.U) { value := a & operandB }
          }
          when(opcode === "h33".U) {
            decodeFault := f7 =/= 0.U && !(f7 === "h20".U && (f3 === 0.U || f3 === 5.U))
          }.otherwise {
            decodeFault := (f3 === 1.U && f7 =/= 0.U) ||
              (f3 === 5.U && f7 =/= 0.U && f7 =/= "h20".U)
          }
        }
      }
      is("h03".U, "h23".U) {
        memoryOp := true.B
        writeRD := !store
        switch(f3) {
          is(0.U) { strobes := 1.U(4.W) << address(1, 0); value := Cat(Fill(24, lane(7)), lane(7, 0)) }
          is(1.U) { strobes := 3.U(4.W) << address(1, 0); value := Cat(Fill(16, lane(15)), lane) }
          is(2.U) { strobes := 15.U; value := d_rdata }
          is(4.U) { value := lane(7, 0) }
          is(5.U) { value := lane }
        }
        when(!validMemoryOp) { decodeFault := true.B }
          .elsewhen(misalignedMemory) {
            decodeFault := true.B
            decodeCause := Mux(store, 6.U, 4.U)
            decodeFaultValue := address
          }.otherwise {
            done := d_ready
            when(d_ready && d_error) {
              decodeFault := true.B
              decodeCause := Mux(store, 7.U, 5.U)
              decodeFaultValue := address
            }
          }
      }
      is("h0f".U) {
        redirect := f3 === 1.U // FENCE.I refetches after older stores complete.
        decodeFault := f3 =/= 0.U && f3 =/= 1.U
      }
      is("h73".U) {
        decodeFault := true.B
        when(exWord === "h00000073".U) { decodeCause := 11.U; decodeFaultValue := 0.U }
        when(exWord === "h00100073".U) { decodeCause := 3.U; decodeFaultValue := exPC }
      }
      is("h0b".U) {
        when(f3 < 4.U) {
          val kind = f7(2, 0)
          decodeFault := (f7(4) && f3 < 2.U) || Mux(f7(6),
            f7(5) || f7(3, 0).orR,
            (f7(5) && kind =/= 0.U && kind =/= 5.U) ||
              (kind === 7.U && (rs2 =/= 0.U || f7(5, 4) =/= 0.U)))
          writeRD := true.B; value := guestResult; flagsAlu := !f7(6)
        }.otherwise {
          decodeFault := exWord(31, 20) =/= 0.U ||
            (f3 === 4.U && rd =/= 0.U) || (f3 === 7.U && rs1(4))
          switch(f3) {
            is(4.U) { flagsLoad := true.B }
            is(5.U) { writeRD := true.B; value := (a & "hfffff72a".U) | guestFlags }
            is(6.U) { writeRD := true.B; value := a; flagsCommit := true.B }
            is(7.U) { writeRD := true.B; value := xCondition.asUInt }
          }
        }
      }
      is("h2b".U) {
        switch(f3) {
          is(0.U) {
            decodeFault := exWord(31, 24) =/= 0.U || exWord(21, 20) === 3.U
            writeRD := true.B
            value := Mux(tlbHit, Cat(tlbPage, a(11, 0)), "hffffffff".U)
          }
          is(1.U) {
            decodeFault := rd =/= 0.U || f7 > 1.U || b(11, 3).orR ||
              Mux(f7(0), b(1, 0).orR, b(2))
            tlbFill := true.B
          }
          is(2.U) {
            decodeFault := rd =/= 0.U || exWord(31, 20) =/= 0.U
            tlbInvalidate := true.B
          }
          is(3.U) {
            decodeFault := rd =/= 0.U || rs1 =/= 0.U || exWord(31, 20) =/= 0.U
            tlbFlush := true.B
          }
          is(4.U) {
            decodeFault := rd =/= 0.U || f7 =/= 0.U ||
              a(11, 0) =/= 0.U || b(11, 0) =/= 0.U || a > b
            tlbRegion := true.B
          }
          is(5.U) {
            decodeFault := rd =/= 0.U || f7 =/= 0.U || a(31, 6) =/= 0.U
            contextWrite := true.B
          }
          is(6.U) {
            decodeFault := rd >= 6.U || f7 >= 8.U
            segmentWrite := true.B
          }
          is(7.U) {
            decodeFault := true.B
            when(f7 === 0.U) {
              decodeFault := rd =/= 0.U || rs2 =/= 0.U || a(1, 0) =/= 0.U
              vectorWrite := true.B
            }.elsewhen(f7 === 1.U) {
              decodeFault := rs2 =/= 0.U || rs1 >= 8.U
              writeRD := true.B
              value := MuxLookup(rs1, 0.U(32.W))((0 until 4).map(n => n.U -> serviceRecord(n)) ++ Seq(
                4.U -> guestControl, 5.U -> guestIdentity, 6.U -> serviceVector, 7.U -> serviceActive.asUInt))
            }.elsewhen(f7 === 2.U) {
              decodeFault := rd =/= 0.U || rs2 =/= 0.U || !serviceActive
              redirect := true.B
              target := Mux(rs1 === 0.U, serviceRecord(0), a)
              serviceReturn := true.B
            }
          }
        }
      }
      is("h5b".U) {
        when(!guestEncodingValid) { decodeFault := true.B }
          .elsewhen(guestReason =/= 0.U) {
            when(serviceVector === 0.U || serviceActive) {
              decodeFault := true.B; decodeCause := 12.U; decodeFaultValue := a
            }.otherwise {
              serviceEnter := true.B; redirect := true.B; target := serviceVector
            }
          }.otherwise {
            memoryOp := true.B; writeRD := !store; flagsCommit := store && f7(5)
            switch(f3(1, 0)) {
              is(0.U) { strobes := 1.U(4.W) << address(1, 0); value := lane(7, 0) }
              is(1.U) { strobes := 3.U(4.W) << address(1, 0); value := lane }
              is(2.U) { strobes := 15.U; value := readBytes }
            }
            done := d_ready && (!guestSplitWord || loadSecond || d_error)
            when(d_ready && d_error) {
              decodeFault := true.B; decodeCause := Mux(store, 7.U, 5.U); decodeFaultValue := address
            }
          }
      }
      is("h7b".U) {
        if (banks == 1) { decodeFault := true.B }
        else {
          decodeFault := f3 < 4.U
          switch(f3) {
            is(4.U) {
              decodeFault := exBank || rd =/= 0.U || exWord(31, 20) =/= 0.U
              enterBank := true.B; redirect := true.B; target := a
            }
            is(5.U) {
              decodeFault := !exBank || rd =/= 0.U || rs1 =/= 0.U
              leaveBank := true.B; redirect := true.B; target := runtimeReturn
            }
            is(6.U) {
              decodeFault := exBank || rs2 =/= 0.U || f7 > 1.U || (f7 === 1.U && rs1 > 2.U)
              writeRD := true.B
              value := a
              when(f7 === 1.U) {
                value := MuxLookup(rs1, 0.U)(Seq(
                  0.U -> exitReason, 1.U -> guestReturn, 2.U -> "h42310008".U))
              }
            }
            is(7.U) {
              decodeFault := exBank || f7 =/= 0.U || rs2 =/= 0.U
              writeRD := true.B; writeBank := true.B; value := a
            }
          }
        }
        // Effective-address primitives use the same meaning in either RF bank
        // and are present in the one-bank core too. They do not switch banks.
        when(f3 === 0.U) {
          decodeFault := !eaEncodingValid || ea.fault
          done := ea.done
          writeRD := ea.hit
          value := ea.offset
          when(ea.fault) { decodeCause := 5.U; decodeFaultValue := ea.badAddress }
        }.elsewhen(f3 === 1.U) {
          decodeFault := rs1 =/= 0.U || exWord(31, 20) =/= 0.U
          writeRD := true.B; value := eaInfo
        }.elsewhen(f3 === 2.U) {
          decodeFault := rd =/= 0.U || exWord(31, 20) =/= 0.U || a(1, 0) =/= 0.U || a > "hffffffe0".U
          eaConfigure := true.B
        }.elsewhen(f3 === 3.U) {
          // XJcc's rd bits are a B-immediate, not a register write. It uses
          // committed status and works identically with one or two RF banks.
          decodeFault := rs1(4) || rs2 =/= 0.U
          redirect := xCondition; target := branchTarget
        }
      }
    }
    val legalOpcode = Seq(0x37, 0x17, 0x6f, 0x67, 0x63, 0x13, 0x33,
      0x03, 0x23, 0x0f, 0x73, 0x0b, 0x2b, 0x5b, 0x7b).map(x => opcode === x.U).reduce(_ || _)
    val targetFault = redirect && !decodeFault && target(1, 0) =/= 0.U
    val pcFault = exPC(1, 0) =/= 0.U
    val fault = !legalOpcode || decodeFault || targetFault || exError || pcFault
    val cause = Mux(pcFault, 0.U, Mux(exError, 1.U, Mux(targetFault, 0.U, decodeCause)))
    val faultValue = Mux(pcFault || exError, exPC, Mux(targetFault, target, decodeFaultValue))
    val complete = exValid && (done || fault) && !stopped && !reset
    eaFinish := complete
    val flush = complete && (fault || redirect)
    val issue = idValid && (!exValid || complete) && !flush && !stopped && !reset
    val writeEnable = complete && !fault && writeRD && rd =/= 0.U
    val crossRead = (banks == 2).B && !bank && idWord(6, 0) === "h7b".U &&
      idWord(14, 12) === 6.U && idWord(31, 25) === 0.U
    rf.io.read_enable := issue
    rf.io.read_a := Cat(Mux(crossRead, true.B, bank), idWord(19, 15))
    rf.io.read_b := Cat(bank, idWord(24, 20))
    rf.io.write_enable := writeEnable
    rf.io.write_addr := Cat(writeBank, rd)
    rf.io.write_value := value
    val mStart = exValid && muldivOp && !mStarted && !fault && !stopped && !reset
    md.io.start := mStart

    i_valid := fetchValid
    i_addr := fetchAddress
    // Ready/error describe completion; neither can gate the request's valid.
    d_valid := exValid && memoryOp && validMemoryOp && !misalignedMemory &&
      !exError && !pcFault && !stopped && !reset
    // Same-page qualification proves this increment cannot change the native
    // page. Keep it to ten bits; faults still report the original byte address.
    d_addr := Cat(address(31, 12),
      Mux(loadSecond, address(11, 2) + 1.U, address(11, 2)), 0.U(2.W))
    d_wdata := b << Cat(address(1, 0), 0.U(3.W))
    d_wstrb := Mux(store, strobes, 0.U)
    when(eaOperation) {
      d_valid := ea.request
      d_addr := ea.address
      d_wdata := 0.U
      d_wstrb := 0.U
    }
    trap := stopped
    trap_pc := stoppedPC
    trap_cause := stoppedCause
    trap_value := stoppedValue
    retire_valid := complete && !fault
    retire_pc := exPC
    retire_word := exWord
    retire_bank := exBank
    retire_write := writeEnable
    retire_rd := Cat(writeBank, rd)
    retire_value := value
    retire_ea_hit := retire_valid && eaOperation && ea.hit

    when(mStart) { mStarted := true.B }
    when(d_valid && d_ready && !d_error && guestSplitWord && !loadSecond) {
      loadFirst := d_rdata
      loadSecond := true.B
    }
    when(complete && !fault) {
      when(eaConfigure) { eaRegisters := a; eaConfigured := true.B; eaInfo := "hffffffff".U }
      when(eaOperation) { eaInfo := ea.info }
      when(flagsAlu) {
        pendingFlags := guestStatus
        when(f7(3)) { guestFlags := guestStatus }
      }
      when(flagsLoad) {
        guestFlags := a & "h8d5".U
        pendingFlags := a & "h8d5".U
      }
      when(flagsCommit) { guestFlags := pendingFlags }
      when(leaveBank) { pendingFlags := guestFlags }
      when(tlbFill) {
        // Check the native aperture when publishing a mapping. No range adder
        // or comparison is inserted into the hit or ordinary memory paths.
        val allowed = b(31, 12) >= guestBasePage && b(31, 12) < guestLimitPage
        when(f7(0)) {
          guestITlb(itlbIndex) := Cat(a(31, 14), b(31, 12), b(2))
          guestITlbValid := (guestITlbValid & ~itlbMask) | Mux(allowed, itlbMask, 0.U)
        }.otherwise {
          guestTlb(tlbIndex) := Cat(a(31, 16), b(31, 12), b(1, 0))
          guestTlbValid := (guestTlbValid & ~tlbMask) | Mux(allowed, tlbMask, 0.U)
        }
      }
      when(tlbInvalidate) {
        guestTlbValid := guestTlbValid & ~tlbMask
        guestITlbValid := guestITlbValid & ~itlbMask
      }
      when(tlbFlush || tlbRegion) {
        guestTlbValid := 0.U
        guestITlbValid := 0.U
      }
      when(tlbRegion) {
        guestBasePage := a(31, 12)
        guestLimitPage := b(31, 12)
      }
      when(contextWrite) {
        when(!guestContextValid || guestControl =/= a(5, 0) || guestIdentity =/= b) {
          guestTlbValid := 0.U
          guestITlbValid := 0.U
          segmentRights.foreach(_ := 0.U)
        }
        guestContextValid := true.B
        guestControl := a(5, 0)
        guestIdentity := b
      }
      when(segmentWrite) {
        segmentBases(rd(2, 0)) := a
        segmentLimits(rd(2, 0)) := b
        segmentRights(rd(2, 0)) := f7(2, 0)
      }
      when(vectorWrite) { serviceVector := a }
      when(serviceEnter) {
        serviceRecord(0) := exPC
        serviceRecord(1) := a
        serviceRecord(2) := exWord
        serviceRecord(3) := guestReason
        serviceActive := true.B
      }
      when(serviceReturn) { serviceActive := false.B }
    }
    when(complete) {
      exValid := false.B
      mStarted := false.B
      loadSecond := false.B
      when(fault) {
        stopped := true.B
        stoppedPC := exPC
        stoppedCause := cause
        stoppedValue := faultValue
      }.elsewhen(enterBank) {
        bank := true.B
        runtimeReturn := exPC + 4.U
      }.elsewhen(leaveBank) {
        bank := false.B
        guestReturn := exPC + 4.U
        exitReason := exWord(31, 20)
      }
    }
    when(issue) {
      exValid := true.B
      exWord := idWord
      exPC := idPC
      exError := idError
      exBank := bank
      mStarted := false.B
      loadSecond := false.B
      idValid := false.B
    }
    when(fetchValid && i_ready) {
      fetchValid := false.B
      fetchKilled := false.B
      when(!fetchKilled && !flush && !stopped) {
        idValid := true.B
        idWord := i_data
        idPC := fetchAddress
        idError := i_error
        fetchPC := fetchAddress + 4.U
      }
    }
    when(!fetchValid && (!idValid || issue) && !flush && !stopped) {
      fetchValid := true.B
      fetchAddress := fetchPC
      fetchKilled := false.B
    }
    when(flush) {
      idValid := false.B
      fetchPC := target
      // An outstanding request cannot be withdrawn. Drain then discard it,
      // including an error on a wrong-path fetch, before fetching the new PC.
      when(fetchValid && !i_ready) { fetchKilled := true.B }
    }
  }
}
