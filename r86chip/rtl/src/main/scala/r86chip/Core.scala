package r86chip

import chisel3._
import chisel3.util._

// B1 native slice; see design/RTL.md. Five stages: fetch (one request in
// flight), decode (instruction buffer with one skid slot, synchronous RF
// read, branch prediction), execute X, memory M and write-back W.
//
// X decides results, redirects and every fault except a native bus error. Its
// effects apply once, when it advances into M, which requires every older
// instruction to have completed. Whether X advances never depends on operand
// values: a fault stops the core at that edge, and a redirect or a guest miss
// is registered and acted on in M, which squashes the younger word in X. A
// guest access is qualified in one X clock; a second-level table hit replays
// the word, any other miss enters the software service. M moves data on the
// native bus; W writes the register file. M and W forward into X; a consumer
// directly behind a load waits one clock.
// BANKS is the only experiment parameter; this is intentionally not a
// configurable CPU framework.
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
  // Lookahead for synchronous-read memories and caches: the address each
  // port is expected to present after the coming clock edge. A hint only; a
  // memory must still check the address actually requested. i_flush pulses
  // when FENCE.I retires.
  val i_next_addr = IO(Output(UInt(32.W)))
  val d_next_addr = IO(Output(UInt(32.W)))
  val i_flush = IO(Output(Bool()))
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
    // Guest data mappings: the architectural table is l2, 1024 direct-mapped
    // rows (tag10, page20, R/W2) in block RAM. l1 is an invisible 64-row copy
    // (tag14, page20, R/W2) read inside X. Every l1 row is also in l2: fills
    // write both, and rows that collide in l2 collide in l1.
    val l1 = Module(new LutRam(6, 36))
    val l1Valid = RegInit(0.U(64.W))
    val l2 = new EpochTable(clk, 10, 32)
    // Four fetch rows (tag18, page20, X1) for the interpreter's code reads.
    val guestITlb = Reg(Vec(4, UInt(39.W)))
    val guestITlbValid = RegInit(0.U(4.W))
    val guestBasePage = RegInit(0.U(20.W))
    val guestLimitPage = RegInit(0.U(20.W))
    // Software-filled jump table: guest EIP (tag22) to native word (target30).
    val jt = new EpochTable(clk, 10, 52)
    val jtPhase = RegInit(0.U(2.W))
    val jtHit = Reg(Bool())
    val jtTarget = Reg(UInt(30.W))
    // Guest return stack: (return EIP, native entry of its block), pushed by
    // XPUSHRET when its store completes and popped by XJT.R. Architectural
    // and in program order; decode only reads the top as a prediction.
    // Rows are (EIP32, native30) in LUT RAM; rasCount says which are live.
    val ras = Module(new LutRam(3, 62))
    val rasTop = RegInit(0.U(3.W))
    val rasCount = RegInit(0.U(4.W))
    val rasMatch = Reg(Bool())
    val rasPredicted = Reg(Bool())
    val rasTarget = Reg(UInt(30.W))
    // Guest instruction budget consumed by XTICK.
    val tick = RegInit(0.U(32.W))
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
    // fetchPC holds a reset or redirect target until its request starts;
    // otherwise the next request follows fetchAddress sequentially.
    val fetchPC = RegInit("h1000".U(32.W))
    val fetchRedirected = RegInit(true.B)
    val fetchValid = RegInit(false.B)
    val fetchAddress = RegInit("h1000".U(32.W))
    val fetchKilled = RegInit(false.B)
    val idValid = RegInit(false.B)
    val idError = Reg(Bool())
    val idPC = Reg(UInt(32.W))
    val idWord = Reg(UInt(32.W))
    // The word in ID already redirected fetch to its predicted target.
    val idPredicted = RegInit(false.B)
    val idPredictedNative = Reg(UInt(30.W))
    // Branch targets learned at decode (tag24, target30, unconditional1),
    // used one stage earlier: the fetch after a known taken branch goes
    // straight to its target. Only a hint; X checks direction and target.
    val btb = Module(new LutRam(6, 55))
    val btbValid = RegInit(0.U(64.W))
    // A completed fetch cannot be refused. The skid slot receives the word
    // that arrives while ID still holds its predecessor.
    val skidValid = RegInit(false.B)
    val skidError = Reg(Bool())
    val skidPC = Reg(UInt(32.W))
    val skidWord = Reg(UInt(32.W))
    val skidPredicted = Reg(Bool())
    val skidNative = Reg(UInt(30.W))
    // Two-bit direction counters for conditional branches, indexed by PC.
    val bht = RegInit(VecInit(Seq.fill(64)(1.U(2.W))))
    val exValid = RegInit(false.B)
    val exError = Reg(Bool())
    val exBank = Reg(Bool())
    val exPC = Reg(UInt(32.W))
    val exWord = Reg(UInt(32.W))
    val exPredicted = Reg(Bool())
    val exPredictedNative = Reg(UInt(30.W))
    // RF addresses read at issue, including the bank tag, for forwarding.
    val exReadA = Reg(UInt(6.W))
    val exReadB = Reg(UInt(6.W))
    // RF outputs are sampled once at issue. A producer that leaves W while X
    // waits would be lost, so X keeps the newest resolved operand values.
    val heldA = Reg(UInt(32.W))
    val heldB = Reg(UInt(32.W))
    val heldValidA = RegInit(false.B)
    val heldValidB = RegInit(false.B)
    val mStarted = RegInit(false.B)
    val mdDone = RegInit(false.B)
    // M: one native bus transaction of one or two beats, a table walk, or a
    // pass-through result.
    val mValid = RegInit(false.B)
    val mPC = Reg(UInt(32.W))
    val mWord = Reg(UInt(32.W))
    val mBank = Reg(Bool())
    val mWrite = Reg(Bool())
    val mRD = Reg(UInt(6.W))
    val mResult = Reg(UInt(32.W))
    val mBus = Reg(Bool())
    val mStore = Reg(Bool())
    val mGuest = Reg(Bool())
    val mSigned = Reg(Bool())
    val mSplit = Reg(Bool())
    val mCommitFlags = Reg(Bool())
    // Redirect to mAddress when this word reaches M.
    val mLate = Reg(Bool())
    val mTarget = Reg(UInt(32.W))
    val mRasPush = Reg(Bool())
    val mRasNative = Reg(UInt(30.W))
    val mEaHit = Reg(Bool())
    val mWidth = Reg(UInt(3.W))
    val mAddress = Reg(UInt(32.W))
    val mData = Reg(UInt(64.W))
    val mStrobes = Reg(UInt(8.W))
    // Guest qualification result: 0 hit, otherwise the service reason. mWalk
    // asks for the second-level row before that reason (or an XTLOOK miss) is
    // final; the row arrives during the first M clock.
    val mReason = Reg(UInt(3.W))
    val mWalk = Reg(Bool())
    val mWalkDone = Reg(Bool())
    val mWalkHit = Reg(Bool())
    val mWalkRow = Reg(UInt(22.W))
    val mLinearPage = Reg(UInt(20.W))
    // An unaligned guest access may cross one native word, never a guest page.
    // Hold the first response until the second succeeds; no partial writeback.
    val secondBeat = RegInit(false.B)
    val loadFirst = Reg(UInt(32.W))
    // W: the register-file write, registered so bus data never reaches the RF
    // write port combinationally.
    val wWrite = RegInit(false.B)
    val wRD = Reg(UInt(6.W))
    val wValue = Reg(UInt(32.W))
    val stopped = RegInit(false.B)
    val stoppedPC = RegInit(0.U(32.W))
    val stoppedCause = RegInit(0.U(4.W))
    val stoppedValue = RegInit(0.U(32.W))

    val rf = Module(new RegFile(banks))
    rf.io.clk := clk
    // Newest producer wins: M, then W, then the value X already resolved, then
    // the RF output. x0 never matches because its writes are not flagged.
    val matchMA = mValid && mWrite && mRD === exReadA
    val matchMB = mValid && mWrite && mRD === exReadB
    val matchWA = wWrite && wRD === exReadA
    val matchWB = wWrite && wRD === exReadB
    val mLoad = mBus && !mStore
    val a = Mux(matchMA, mResult, Mux(matchWA, wValue, Mux(heldValidA, heldA, rf.io.value_a)))
    val b = Mux(matchMB, mResult, Mux(matchWB, wValue, Mux(heldValidB, heldB, rf.io.value_b)))
    val readyA = !(matchMA && mLoad)
    val readyB = !(matchMB && mLoad)
    val operandsReady = readyA && readyB
    // An XSW.C in M publishes pending status when its store completes. X is
    // younger, so it already reads the status that store will publish.
    val flagsNow = Mux(mValid && mCommitFlags, pendingFlags, guestFlags)
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
    val xCondition = XFlags.condition(rs1, flagsNow)
    // XALUI is XALU on the dword lane with an immediate, always committed.
    // funct3 7 is INC (imm 1) or DEC (imm -1): ADD or SUB of one, CF kept.
    val xalui = opcode === "h1b".U
    val incdec = xalui && f3 === 7.U
    val aluControl = Mux(xalui,
      Cat(0.U(1.W), incdec, 0.U(1.W), 1.U(1.W), Mux(incdec, Mux(exWord(31), 5.U(3.W), 0.U(3.W)), f3)), f7)
    val (guestResult, guestStatus) = XFlags.alu(a, Mux(xalui, Mux(incdec, 1.U(32.W), immI), b),
      Mux(xalui, 0.U, f3), aluControl, flagsNow)
    // XSH: funct3 SHL/SHR/SAR/ROL/ROR, funct7 0 = count in rs2's value, 1 =
    // count in the rs2 field. RV shifts use the same funnel.
    val xsh = opcode === "h3b".U
    val shiftKind = Mux(xsh, f3, Mux(f3 === 1.U, 0.U(3.W), Mux(exWord(30), 2.U(3.W), 1.U(3.W))))
    val shiftCount = Mux(opcode === "h13".U || (xsh && f7(0)), rs2, b(4, 0))
    val (shiftResult, shiftStatus) = XFlags.shift(a, shiftCount, shiftKind, flagsNow)

    // Guest memory words. 0x5b: offset in rs1, segment in funct7 (or rs2).
    // 0x07/0x27 load/store through DS and 0x47/0x77 through SS, with the RV
    // I/S displacement. Displacement loads use funct3 0/1 = sign-extended
    // byte/word, 2 = dword, 4/5 = zero-extended, 3/6/7 = dword/byte/word with
    // write intent. Displacement stores with funct3 bit 2 also write the
    // offset back to rs1 (PUSH). Displacement words may be unaligned inside a
    // page; 0x5b keeps natural alignment except for fetch .U.
    val guestOld = opcode === "h5b".U
    val guestLoadD = opcode === "h07".U || opcode === "h47".U
    val guestStoreD = opcode === "h27".U || opcode === "h77".U
    val guestMemory = guestOld || guestLoadD || guestStoreD
    val store = opcode === "h23".U || (guestOld && f3(2)) || guestStoreD
    val guestFetch = guestOld && f7(4)
    val guestRmw = Mux(guestOld, f7(3), guestLoadD && (f3 === 3.U || f3(2, 1) === 3.U))
    val guestSigned = guestLoadD && f3(2, 1) === 0.U
    val guestUpdate = guestStoreD && f3(2)
    // XPUSHRET (0x77, funct3 7): push rs2 as a dword at rs1-4 through SS and
    // record (rs2, PC + 4*imm) on the return stack. The S-immediate is the
    // word offset of the return block, not a displacement.
    val pushRet = opcode === "h77".U && f3 === 7.U
    val guestSpan = !guestOld || (guestFetch && f7(5))
    val guestWidth = Mux(guestLoadD,
      Mux(f3(2) || !f3(1), Cat(0.U(1.W), f3(0)), 2.U(2.W)), Mux(pushRet, 2.U(2.W), f3(1, 0)))
    val guestRegisterSegment = guestOld && f7(6)
    val guestSegment = Mux(guestOld, Mux(guestRegisterSegment, b(2, 0), f7(2, 0)),
      Mux(opcode(6), 2.U(3.W), 3.U(3.W)))
    val guestRequired = Mux(store, 2.U(3.W),
      Mux(guestFetch, 4.U(3.W), Mux(guestRmw, 3.U(3.W), 1.U(3.W))))
    // Dynamic selectors use the full old rs2 value: 0x103 is illegal, not DS.
    // Stores already need rs2 for data and cannot use the register-segment form.
    val guestEncodingValid = Mux(guestOld,
      f3(1, 0) =/= 3.U && guestSegment < 6.U &&
        (!guestRegisterSegment || (!b(31, 3).orR && f7(2, 0) === 0.U)) &&
        Mux(store, rd === 0.U && !f7(3) && !f7(4) && !guestRegisterSegment,
          (guestRegisterSegment || rs2 === 0.U) && (!f7(5) || f7(4))) &&
        (!f7(4) || (guestSegment === 1.U && !f7(3))),
      guestLoadD || f3(1, 0) =/= 3.U || pushRet)
    // One adder gives the native address or the guest offset; a three-input
    // sum gives the guest linear address. Table words probe rs1's value.
    val displacement = Mux(guestOld, 0.U(32.W),
      Mux(pushRet, "hfffffffc".U(32.W), Mux(opcode(5), immS, immI)))
    val offset = a + displacement
    val linear = Mux(guestMemory, segmentBases(guestSegment), 0.U(32.W)) + a +
      Mux(guestMemory, displacement, 0.U(32.W))
    // Inclusive limit without a third carry chain: the offset must not exceed
    // the limit, and the room above it must hold the remaining bytes.
    val limitRoom = Cat(0.U(1.W), segmentLimits(guestSegment)) - Cat(0.U(1.W), offset)
    val guestSegmentOk = (segmentRights(guestSegment) & guestRequired) === guestRequired &&
      !limitRoom(32) && MuxLookup(guestWidth, true.B)(Seq(
        1.U -> limitRoom(31, 0).orR,
        2.U -> (limitRoom(31, 2).orR || limitRoom(1, 0) === 3.U)))
    val xtlook = opcode === "h2b".U && f3 === 0.U
    val tlbFetch = Mux(guestMemory, guestFetch, exWord(23, 22) === 2.U)
    val tlbRequired = Mux(guestMemory, guestRequired,
      MuxLookup(exWord(23, 22), 1.U(3.W))(Seq(
        0.U -> 1.U(3.W), 1.U -> 2.U(3.W), 2.U -> 4.U(3.W), 3.U -> 3.U(3.W))))
    val tlbWidth = Mux(guestMemory, guestWidth, exWord(21, 20))
    val l1Index = linear(17, 12)
    val l1Mask = UIntToOH(l1Index, 64)
    l1.io.clk := clk
    l1.io.read_addr := l1Index
    val l1Slot = l1.io.read_data
    val l1Present = l1Valid(l1Index) && l1Slot(35, 22) === linear(31, 18)
    val itlbIndex = linear(13, 12)
    val itlbSlot = guestITlb(itlbIndex)
    val itlbMask = UIntToOH(itlbIndex, 4)
    val fetchPresent = guestITlbValid(itlbIndex) && itlbSlot(38, 21) === linear(31, 14)
    val tlbPage = Mux(tlbFetch, itlbSlot(20, 1), l1Slot(21, 2))
    val tlbRightsOk = Mux(tlbFetch, itlbSlot(0),
      (l1Slot(1, 0) & tlbRequired(1, 0)) === tlbRequired(1, 0))
    val naturallyAligned = tlbWidth === 0.U ||
      (tlbWidth === 1.U && !linear(0)) ||
      (tlbWidth === 2.U && linear(1, 0) === 0.U)
    val samePage = tlbWidth === 0.U ||
      (tlbWidth === 1.U && linear(11, 0) =/= "hfff".U) ||
      (tlbWidth === 2.U && linear(11, 0) <= "hffc".U)
    val tlbSpanOk = Mux(guestMemory && guestSpan, samePage, naturallyAligned)
    val tlbHit = Mux(tlbFetch, fetchPresent, l1Present) && tlbRightsOk && tlbSpanOk
    // Only an absent l1 row leaves the answer to l2; a present row is final.
    val tlbWalk = !tlbFetch && !l1Present && tlbSpanOk
    val contextOk = guestContextValid && guestControl(0) && !guestControl(3)
    val guestReason = Mux(!contextOk, 1.U(3.W),
      Mux(!guestSegmentOk, 2.U(3.W), Mux(!tlbSpanOk, 3.U(3.W), Mux(!tlbHit, 4.U(3.W), 0.U(3.W)))))
    val address = Mux(guestMemory, Cat(tlbPage, linear(11, 0)), offset)
    val guestSplit = guestMemory && guestSpan &&
      ((guestWidth === 2.U && address(1, 0) =/= 0.U) ||
       (guestWidth === 1.U && address(1, 0) === 3.U))
    val validMemoryOp = f3 <= 2.U || (!store && (f3 === 4.U || f3 === 5.U))
    val misalignedMemory = (f3(1, 0) === 1.U && address(0)) ||
      (f3(1, 0) === 2.U && address(1, 0) =/= 0.U)
    val accessWidth = Mux(guestMemory, guestWidth, f3(1, 0))
    val strobes = (MuxLookup(accessWidth, 15.U(8.W))(Seq(
      0.U -> 1.U(8.W), 1.U -> 3.U(8.W))) << address(1, 0))(7, 0)
    l2.readIndex := linear(21, 12)
    l2.writeIndex := linear(21, 12)
    l2.writePayload := Cat(a(31, 22), b(31, 12), b(1, 0))
    l1.io.write_enable := false.B
    l1.io.write_addr := l1Index
    l1.io.write_data := Cat(a(31, 18), b(31, 12), b(1, 0))
    jt.readIndex := a(9, 0)
    jt.writeIndex := a(9, 0)
    jt.writePayload := Cat(a(31, 10), b(31, 2))
    // M-stage load alignment. On a single beat only the low word's requested
    // lanes are used. On the second beat select the saved low word and
    // concatenate the new high word.
    val readBytes = (Cat(d_rdata, Mux(secondBeat, loadFirst, d_rdata)) >>
      Cat(mAddress(1, 0), 0.U(3.W)))(31, 0)
    val lane = readBytes(15, 0)
    val loadValue = Mux(mGuest,
      MuxLookup(mWidth(1, 0), readBytes)(Seq(
        0.U -> Cat(Fill(24, mSigned && lane(7)), lane(7, 0)),
        1.U -> Cat(Fill(16, mSigned && lane(15)), lane))),
      MuxLookup(mWidth, d_rdata)(Seq(
        0.U -> Cat(Fill(24, lane(7)), lane(7, 0)),
        1.U -> Cat(Fill(16, lane(15)), lane),
        4.U -> lane(7, 0),
        5.U -> lane)))
    val md = Module(new MulDiv)
    md.io.a := a
    md.io.b := b
    md.io.operation := f3
    val eaOperation = opcode === "h7b".U && f3 === 0.U
    val eaEncodingValid = f7 === 0.U && !a(31, 8).orR && a(7, 6) =/= 3.U
    val eaFinish = WireDefault(false.B)
    // XEA32 drives the data bus from X, so it waits for an empty M. Its
    // outcome is registered and the word completes one clock later, which
    // keeps the sequencer's address checks out of the completion path.
    val eaDone = RegInit(false.B)
    val eaHit = Reg(Bool())
    val eaFault = Reg(Bool())
    val eaOffset = Reg(UInt(32.W))
    val eaBad = Reg(UInt(32.W))
    val eaInfoNext = Reg(UInt(32.W))
    // Its bus requests are registered too, one clock after the sequencer forms
    // them, so nothing on the data port depends on X-stage operands.
    val eaRequest = RegInit(false.B)
    val eaAddress = Reg(UInt(32.W))
    val eaBeatDone = eaRequest && d_ready
    val ea = Ea32(exValid && eaOperation && eaEncodingValid && !exError && !eaDone &&
      exPC(1, 0) === 0.U && operandsReady && !mValid && !stopped && !reset, eaFinish, a, b,
      eaConfigured, eaRegisters, contextOk,
      segmentBases(1), segmentLimits(1), segmentRights(1)(2), guestITlb, guestITlbValid,
      eaBeatDone, d_rdata, d_error)
    eaRequest := ea.request && !eaBeatDone
    eaAddress := ea.address

    val done = WireDefault(true.B)
    val decodeFault = WireDefault(false.B)
    val decodeCause = WireDefault(2.U(4.W))
    val decodeFaultValue = WireDefault(exWord)
    val writeRD = WireDefault(false.B)
    val writeIndex = WireDefault(rd)
    val writeBank = WireDefault(exBank)
    val value = WireDefault(0.U(32.W))
    val redirect = WireDefault(false.B)
    val target = WireDefault(exPC + 4.U)
    // The word transfers control to a `target` computed from operands, whose
    // alignment must be checked. `redirect` says fetch is not on that path.
    val transfer = WireDefault(false.B)
    // A conditional branch reports its direction; `redirect` is then only the
    // correction of a wrong prediction.
    val conditional = WireDefault(false.B)
    val taken = WireDefault(false.B)
    val memoryOp = WireDefault(false.B)
    val muldivOp = WireDefault(false.B)
    val enterBank = WireDefault(false.B)
    val leaveBank = WireDefault(false.B)
    val statusWrite = WireDefault(false.B)
    val statusCommit = WireDefault(false.B)
    val statusValue = WireDefault(guestStatus)
    val flagsLoad = WireDefault(false.B)
    val flagsCommit = WireDefault(false.B)
    val storeCommitsFlags = WireDefault(false.B)
    val tlbFill = WireDefault(false.B)
    val tlbInvalidate = WireDefault(false.B)
    val tlbFlush = WireDefault(false.B)
    val tlbRegion = WireDefault(false.B)
    val contextWrite = WireDefault(false.B)
    val segmentWrite = WireDefault(false.B)
    val vectorWrite = WireDefault(false.B)
    val serviceReturn = WireDefault(false.B)
    val eaConfigure = WireDefault(false.B)
    val jtFill = WireDefault(false.B)
    val jtInvalidate = WireDefault(false.B)
    val jtFlush = WireDefault(false.B)
    val tickOp = WireDefault(false.B)
    val tickSet = WireDefault(false.B)
    val tickExpired = tick < exWord(31, 20)
    val xjt = opcode === "h6b".U && f3 === 0.U
    val xjtReturn = xjt && exWord(20)
    ras.io.clk := clk
    ras.io.read_addr := rasTop
    val rasTopEip = ras.io.read_data(61, 30)
    val rasTopNative = ras.io.read_data(29, 0)
    ras.io.write_enable := false.B
    ras.io.write_addr := rasTop + 1.U
    ras.io.write_data := Cat((mData >> Cat(mAddress(1, 0), 0.U(3.W)))(31, 0), mRasNative)

    // Directly decode the original 32-bit word. Decode failures, target checks,
    // and fetch errors have separate wires so priority cannot form feedback.
    switch(opcode) {
      is("h37".U) { writeRD := true.B; value := immU }
      is("h17".U) { writeRD := true.B; value := exPC + immU }
      is("h6f".U) {
        writeRD := true.B; value := exPC + 4.U
        transfer := true.B; target := exPC + immJ
        redirect := !(exPredicted && exPredictedNative === target(31, 2))
      }
      is("h67".U) {
        writeRD := true.B; value := exPC + 4.U
        target := (a + immI) & "hfffffffe".U
        transfer := true.B; redirect := true.B
        decodeFault := f3 =/= 0.U
      }
      is("h63".U) {
        conditional := true.B
        decodeFault := f3 === 2.U || f3 === 3.U
        switch(f3) {
          is(0.U) { taken := a === b }
          is(1.U) { taken := a =/= b }
          is(4.U) { taken := a.asSInt < b.asSInt }
          is(5.U) { taken := a.asSInt >= b.asSInt }
          is(6.U) { taken := a < b }
          is(7.U) { taken := a >= b }
        }
      }
      is("h13".U, "h33".U) {
        writeRD := true.B
        when(opcode === "h33".U && f7 === 1.U) {
          muldivOp := true.B
          done := mStarted && (md.io.done || mdDone)
          value := md.io.result
        }.elsewhen(zba) {
          value := addResult
          decodeFault := f3 =/= 2.U && f3 =/= 4.U && f3 =/= 6.U
        }.otherwise {
          switch(f3) {
            is(0.U) { value := Mux(opcode === "h33".U && f7 === "h20".U, a - b, addResult) }
            is(1.U, 5.U) { value := shiftResult }
            is(2.U) { value := (a.asSInt < operandB.asSInt).asUInt }
            is(3.U) { value := (a < operandB).asUInt }
            is(4.U) { value := a ^ operandB }
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
        when(!validMemoryOp) { decodeFault := true.B }
          .elsewhen(misalignedMemory) {
            decodeFault := true.B
            decodeCause := Mux(store, 6.U, 4.U)
            decodeFaultValue := address
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
          writeRD := true.B; value := guestResult
          statusWrite := !f7(6); statusCommit := f7(3)
        }.otherwise {
          decodeFault := exWord(31, 20) =/= 0.U ||
            (f3 === 4.U && rd =/= 0.U) || (f3 === 7.U && rs1(4))
          switch(f3) {
            is(4.U) { flagsLoad := true.B }
            is(5.U) { writeRD := true.B; value := (a & "hfffff72a".U) | flagsNow }
            is(6.U) { writeRD := true.B; value := a; flagsCommit := true.B }
            is(7.U) { writeRD := true.B; value := xCondition.asUInt }
          }
        }
      }
      is("h1b".U) {
        decodeFault := incdec && exWord(31, 20) =/= 1.U && exWord(31, 20) =/= "hfff".U
        writeRD := true.B; value := guestResult
        statusWrite := true.B; statusCommit := true.B
      }
      is("h3b".U) {
        decodeFault := f3 > 4.U || f7 > 1.U
        writeRD := true.B; value := shiftResult
        statusWrite := shiftCount =/= 0.U; statusCommit := true.B
        statusValue := shiftStatus
      }
      is("h2b".U) {
        // Every table word waits for a sweep of the data table to finish.
        when(f3 <= 5.U) { done := !l2.sweeping }
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
              redirect := true.B; transfer := true.B
              target := Mux(rs1 === 0.U, serviceRecord(0), a)
              serviceReturn := true.B
            }
          }
        }
      }
      is("h5b".U, "h07".U, "h27".U, "h47".U, "h77".U) {
        // One X clock. The qualification result travels to M with the word.
        done := !l2.sweeping
        value := offset
        // Bus use, register write and status commit are tentative: M applies
        // them only when the registered reason is zero.
        decodeFault := !guestEncodingValid
        memoryOp := true.B
        writeRD := !store || guestUpdate
        storeCommitsFlags := guestOld && store && f7(5)
        when(guestUpdate) { writeIndex := rs1 }
      }
      is("h6b".U) {
        // Translator control: jump table and guest instruction budget.
        switch(f3) {
          is(0.U) {
            // XJT rs1: continue at the native target filled for guest EIP
            // rs1, or at PC+4 when the table has none. XJT.R (imm 1) first
            // pops the return stack and uses its entry when the EIP matches;
            // a correct decode-time prediction then needs no redirect.
            decodeFault := rd =/= 0.U || exWord(31, 21) =/= 0.U
            done := jtPhase === 2.U || (jtPhase === 1.U && rasMatch)
            when(rasMatch) {
              redirect := !rasPredicted; target := Cat(rasTarget, 0.U(2.W))
            }.otherwise {
              redirect := jtHit || exPredicted
              target := Mux(jtHit, Cat(jtTarget, 0.U(2.W)), exPC + 4.U)
            }
          }
          is(1.U) {
            decodeFault := rd =/= 0.U || f7 =/= 0.U || b(1, 0) =/= 0.U
            done := !jt.sweeping
            jtFill := true.B
          }
          is(2.U) {
            decodeFault := rd =/= 0.U || exWord(31, 20) =/= 0.U
            done := !jt.sweeping
            jtInvalidate := true.B
          }
          is(3.U) {
            decodeFault := rd =/= 0.U || rs1 =/= 0.U || exWord(31, 20) =/= 0.U
            done := !jt.sweeping
            jtFlush := true.B
          }
          is(4.U) {
            // XTICK n: spend n guest instructions, or leave bank 1 with
            // reason 0xfff and this word's own PC recorded.
            decodeFault := !exBank || rd =/= 0.U || rs1 =/= 0.U
            tickOp := true.B
            redirect := tickExpired; target := runtimeReturn
          }
          is(5.U) {
            decodeFault := rd =/= 0.U || exWord(31, 20) =/= 0.U
            tickSet := true.B
          }
          is(6.U) {
            decodeFault := rs1 =/= 0.U || exWord(31, 20) =/= 0.U
            writeRD := true.B; value := tick
          }
          is(7.U) { decodeFault := true.B }
        }
      }
      is("h7b".U) {
        if (banks == 1) { decodeFault := true.B }
        else {
          decodeFault := f3 < 4.U
          switch(f3) {
            is(4.U) {
              decodeFault := exBank || rd =/= 0.U || exWord(31, 20) =/= 0.U
              enterBank := true.B; redirect := true.B; transfer := true.B; target := a
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
                  0.U -> exitReason, 1.U -> guestReturn, 2.U -> "h4231000a".U))
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
          decodeFault := !eaEncodingValid || (eaDone && eaFault)
          done := eaDone
          writeRD := eaHit
          value := eaOffset
          when(eaEncodingValid) { decodeCause := 5.U; decodeFaultValue := eaBad }
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
          conditional := true.B; taken := xCondition
        }
      }
    }
    when(conditional) {
      transfer := taken
      redirect := Mux(taken, !(exPredicted && exPredictedNative === branchTarget(31, 2)), exPredicted)
      target := Mux(taken, branchTarget, exPC + 4.U)
    }
    // A prediction attached to a word that is not a branch (stale table row
    // after code changed) is corrected like a wrong direction.
    val stray = exPredicted && !(opcode === "h6f".U || conditional || xjt)
    val wrongTarget = exPredicted && (stray ||
      ((opcode === "h6f".U || (conditional && taken)) && exPredictedNative =/= target(31, 2)))
    val legalOpcode = Seq(0x37, 0x17, 0x6f, 0x67, 0x63, 0x13, 0x33, 0x03, 0x23, 0x0f, 0x73,
      0x0b, 0x1b, 0x2b, 0x3b, 0x5b, 0x6b, 0x7b, 0x07, 0x27, 0x47, 0x77).map(x => opcode === x.U).reduce(_ || _)
    val targetFault = transfer && !decodeFault && target(1, 0) =/= 0.U
    val pcFault = exPC(1, 0) =/= 0.U
    val fault = !legalOpcode || decodeFault || targetFault || exError || pcFault
    val cause = Mux(pcFault, 0.U, Mux(exError, 1.U, Mux(targetFault, 0.U, decodeCause)))
    val faultValue = Mux(pcFault || exError, exPC, Mux(targetFault, target, decodeFaultValue))

    // M outcomes, all from registers. A walk takes one clock to register the
    // l2 row. A walk hit refills l1 and replays the word without retiring it.
    // A final guest miss retires as a redirect into the software service, or
    // stops when no service can run. A native bus error is terminal.
    val live = !stopped && !reset
    val mHit = !mGuest || mReason === 0.U
    val mWalking = mValid && mWalk && !mWalkDone
    val mReplay = mValid && mWalk && mWalkDone && mWalkHit
    val mMiss = mValid && !mHit && !mWalking && !mReplay
    val serviceBlocked = serviceVector === 0.U || serviceActive
    val mServiceEnter = mMiss && !serviceBlocked && live
    val mBusUse = mBus && mHit
    val busDone = d_ready && (!mSplit || secondBeat || d_error)
    val mComplete = mValid && !mWalking && (!mBusUse || busDone) && live
    val mBusFault = mValid && mBusUse && d_ready && d_error && live
    val mFault = mBusFault || (mMiss && serviceBlocked && live)
    val mRedirect = (mReplay && live) || mServiceEnter || (mValid && mLate && live)
    val mFree = !mValid || (mComplete && !mFault && !mRedirect)
    // X advances once: operands resolved, its own work done, M able to accept.
    // Every multi-clock word finishes whether or not it faults, so `fault`
    // (which depends on operand values) stays out of this signal.
    val complete = exValid && operandsReady && done && mFree && live
    eaFinish := ea.done
    val flush = mRedirect
    val flushTarget = Mux(mReplay, mPC, Mux(mServiceEnter, serviceVector, mTarget))
    val issue = idValid && (!exValid || complete) && !flush && live
    val crossRead = (banks == 2).B && !bank && idWord(6, 0) === "h7b".U &&
      idWord(14, 12) === 6.U && idWord(31, 25) === 0.U
    val readA = Cat(Mux(crossRead, true.B, bank), idWord(19, 15))
    val readB = Cat(bank, idWord(24, 20))
    rf.io.read_enable := issue
    rf.io.read_a := readA
    rf.io.read_b := readB
    rf.io.write_enable := wWrite
    rf.io.write_addr := wRD
    rf.io.write_value := wValue
    val mStart = exValid && muldivOp && !mStarted && operandsReady && live
    md.io.start := mStart
    md.io.abort := mRedirect

    i_valid := fetchValid
    i_addr := fetchAddress
    // M requests come from registers. Ready/error describe completion; neither
    // can gate a request's valid.
    d_valid := mValid && mBusUse && live
    // Same-page qualification proves this increment cannot change the native
    // page. Keep it to ten bits; faults still report the original byte address.
    // A split store writes its upper word first: if that word is not backed
    // (the end of RAM), nothing has been written when the store faults.
    val highBeat = mSplit && (mStore =/= secondBeat)
    d_addr := Cat(mAddress(31, 12),
      Mux(highBeat, mAddress(11, 2) + 1.U, mAddress(11, 2)), 0.U(2.W))
    d_wdata := Mux(highBeat, mData(63, 32), mData(31, 0))
    d_wstrb := Mux(mStore, Mux(highBeat, mStrobes(7, 4), mStrobes(3, 0)), 0.U)
    val beatAdvance = d_valid && d_ready && !d_error && mValid && mSplit && !secondBeat
    val nextHigh = mSplit && (mStore =/= (secondBeat || beatAdvance))
    d_next_addr := Mux(complete, Cat(address(31, 2), 0.U(2.W)),
      Cat(mAddress(31, 12), Mux(nextHigh, mAddress(11, 2) + 1.U, mAddress(11, 2)), 0.U(2.W)))
    when(!mValid && eaOperation) {
      d_valid := eaRequest
      d_addr := eaAddress
      d_wdata := 0.U
      d_wstrb := 0.U
      d_next_addr := eaAddress
    }
    trap := stopped
    trap_pc := stoppedPC
    trap_cause := stoppedCause
    trap_value := stoppedValue
    // Retirement is observed where the instruction's last effect completes.
    val mValue = Mux(mLoad, loadValue, mResult)
    val mRetire = mComplete && !mFault && !mReplay
    retire_valid := mRetire
    retire_pc := mPC
    retire_word := mWord
    retire_bank := mBank
    retire_write := mWrite && mHit
    retire_rd := mRD
    retire_value := mValue
    retire_ea_hit := mRetire && mEaHit
    i_flush := mRetire && mWord(6, 0) === "h0f".U && mWord(12)

    when(mStart) { mStarted := true.B }
    // An XEA32 the sequencer cannot start still completes, as a fault.
    when(exValid && eaOperation && operandsReady && !mValid &&
        (!eaEncodingValid || exError || exPC(1, 0) =/= 0.U)) { eaDone := true.B }
    when(ea.done) {
      eaDone := true.B
      eaHit := ea.hit
      eaFault := ea.fault
      eaOffset := ea.offset
      eaBad := ea.badAddress
      eaInfoNext := ea.info
    }
    when(md.io.done) { mdDone := true.B }
    when(exValid && readyA) { heldA := a; heldValidA := true.B }
    when(exValid && readyB) { heldB := b; heldValidB := true.B }
    // XJT: the row indexed by rs1 arrives one clock after the operand is
    // stable; its comparison is registered before it may redirect.
    // An XPUSHRET still in M has not pushed yet, so the lookup waits for it.
    when(exValid && xjt && operandsReady && !jt.sweeping && jtPhase =/= 2.U &&
        !(mValid && mRasPush)) {
      jtPhase := jtPhase + 1.U
      when(jtPhase === 0.U) {
        rasMatch := xjtReturn && rasCount =/= 0.U && rasTopEip === a
        rasPredicted := exPredicted && exPredictedNative === rasTopNative
        rasTarget := rasTopNative
      }
    }
    when(jtPhase === 1.U) {
      jtHit := jt.live && jt.payload(51, 30) === a(31, 10)
      jtTarget := jt.payload(29, 0)
    }
    when(mWalking) {
      mWalkDone := true.B
      mWalkHit := l2.live && l2.payload(31, 22) === mLinearPage(19, 10)
      mWalkRow := l2.payload(21, 0)
    }
    when(d_valid && d_ready && !d_error && mValid && mSplit && !secondBeat) {
      loadFirst := d_rdata
      secondBeat := true.B
    }
    // W always accepts. Its write happens one clock after retirement.
    wWrite := mRetire && mWrite && mHit
    wRD := mRD
    wValue := mValue
    when(mComplete) {
      mValid := false.B
      secondBeat := false.B
      when(mFault) {
        stopped := true.B
        stoppedPC := mPC
        stoppedCause := Mux(mBusFault, Mux(mStore, 7.U, 5.U), 12.U)
        stoppedValue := Mux(mBusFault, mAddress, mResult)
      }.elsewhen(mReplay) {
        l1.io.write_enable := true.B
        l1.io.write_addr := mLinearPage(5, 0)
        l1.io.write_data := Cat(mLinearPage(19, 6), mWalkRow)
        l1Valid := l1Valid | UIntToOH(mLinearPage(5, 0), 64)
      }.elsewhen(mServiceEnter) {
        serviceRecord(0) := mPC
        serviceRecord(1) := mResult
        serviceRecord(2) := mWord
        serviceRecord(3) := mReason
        serviceActive := true.B
      }.elsewhen(mCommitFlags && mHit) {
        // Older than anything X does on this edge; X's own updates follow.
        guestFlags := pendingFlags
      }
      when(mRetire && mRasPush && mHit) {
        ras.io.write_enable := true.B
        rasTop := rasTop + 1.U
        when(!rasCount(3)) { rasCount := rasCount + 1.U }
      }
    }
    val tickLeave = tickOp && tickExpired
    when(complete && !fault) {
      when(eaConfigure) { eaRegisters := a; eaConfigured := true.B; eaInfo := "hffffffff".U }
      when(eaOperation) { eaInfo := eaInfoNext }
      when(statusWrite) {
        pendingFlags := statusValue
        when(statusCommit) { guestFlags := statusValue }
      }
      when(flagsLoad) {
        guestFlags := a & "h8d5".U
        pendingFlags := a & "h8d5".U
      }
      when(flagsCommit) { guestFlags := pendingFlags }
      when(conditional) {
        val counter = bht(exPC(7, 2))
        when(taken && counter =/= 3.U) { counter := counter + 1.U }
        when(!taken && counter =/= 0.U) { counter := counter - 1.U }
      }
      mValid := true.B
      mPC := exPC
      mWord := exWord
      mBank := exBank
      mWrite := writeRD && writeIndex =/= 0.U
      mRD := Cat(writeBank, writeIndex)
      mResult := value
      mBus := memoryOp
      mStore := store
      mGuest := guestMemory
      mSigned := guestSigned
      mSplit := guestSplit
      mCommitFlags := storeCommitsFlags
      mLate := redirect || stray
      mTarget := Mux(redirect, target, exPC + 4.U)
      when(wrongTarget) { btbValid := btbValid & ~UIntToOH(exPC(7, 2), 64) }
      mRasPush := pushRet
      mRasNative := exPC(31, 2) + immS(29, 0)
      mEaHit := eaOperation && eaHit
      mWidth := Mux(guestMemory, Cat(0.U(1.W), guestWidth), f3)
      mAddress := address
      mData := (Cat(0.U(32.W), b) << Cat(address(1, 0), 0.U(3.W)))(63, 0)
      mStrobes := strobes
      mReason := Mux(guestMemory, guestReason, 0.U)
      mWalk := tlbWalk && Mux(guestMemory, contextOk && guestSegmentOk, xtlook)
      mWalkDone := false.B
      mLinearPage := linear(31, 12)
      when(leaveBank || tickLeave) { pendingFlags := flagsNow }
      when(tickOp && !tickExpired) { tick := tick - exWord(31, 20) }
      when(tickSet) { tick := a }
      when(jtFill || jtInvalidate) { jt.write := true.B; jt.writeLive := jtFill }
      when(jtFlush) { jt.flush := true.B }
      // A dead translation must not be reachable through the return stack.
      when(jtFlush || jtInvalidate) { rasCount := 0.U }
      when(xjtReturn && rasCount =/= 0.U) {
        rasTop := rasTop - 1.U
        rasCount := rasCount - 1.U
      }
      val flushTables = WireDefault(tlbFlush || tlbRegion)
      when(tlbFill) {
        // Check the native aperture when publishing a mapping. No range adder
        // or comparison is inserted into the hit or ordinary memory paths.
        val allowed = b(31, 12) >= guestBasePage && b(31, 12) < guestLimitPage
        when(f7(0)) {
          guestITlb(itlbIndex) := Cat(a(31, 14), b(31, 12), b(2))
          guestITlbValid := (guestITlbValid & ~itlbMask) | Mux(allowed, itlbMask, 0.U)
        }.otherwise {
          l1.io.write_enable := true.B
          l1Valid := (l1Valid & ~l1Mask) | Mux(allowed, l1Mask, 0.U)
          l2.write := true.B
          l2.writeLive := allowed
        }
      }
      when(tlbInvalidate) {
        l1Valid := l1Valid & ~l1Mask
        l2.write := true.B
        guestITlbValid := guestITlbValid & ~itlbMask
      }
      when(tlbRegion) {
        guestBasePage := a(31, 12)
        guestLimitPage := b(31, 12)
      }
      when(contextWrite) {
        when(!guestContextValid || guestControl =/= a(5, 0) || guestIdentity =/= b) {
          flushTables := true.B
          segmentRights.foreach(_ := 0.U)
        }
        guestContextValid := true.B
        guestControl := a(5, 0)
        guestIdentity := b
      }
      when(flushTables) {
        l1Valid := 0.U
        l2.flush := true.B
        guestITlbValid := 0.U
      }
      when(segmentWrite) {
        segmentBases(rd(2, 0)) := a
        segmentLimits(rd(2, 0)) := b
        segmentRights(rd(2, 0)) := f7(2, 0)
      }
      when(vectorWrite) { serviceVector := a }
      when(serviceReturn) { serviceActive := false.B }
    }
    when(complete) {
      exValid := false.B
      when(fault) {
        stopped := true.B
        stoppedPC := exPC
        stoppedCause := cause
        stoppedValue := faultValue
      }.elsewhen(enterBank) {
        bank := true.B
        runtimeReturn := exPC + 4.U
      }.elsewhen(leaveBank || tickLeave) {
        bank := false.B
        guestReturn := Mux(tickOp, exPC, exPC + 4.U)
        exitReason := Mux(tickOp, "hfff".U, exWord(31, 20))
      }
    }
    // An M redirect squashes the younger word in X before it had any effect.
    when(mRedirect) { exValid := false.B }
    when(complete || mRedirect) {
      mStarted := false.B
      eaDone := false.B
      mdDone := false.B
      jtPhase := 0.U
    }

    // Decode-time prediction: JAL always, conditional branches and XJcc by
    // their counter, XJT.R by the top of the guest return stack. Only a legal
    // word with an aligned target is predicted, so X never faults on one; X
    // corrects a wrong direction or return target by redirecting. (A return
    // stack for native JALR measured 0.7% on the interpreter firmware and was
    // not kept.)
    val idOpcode = idWord(6, 0)
    val idJal = idOpcode === "h6f".U
    val idConditional = (idOpcode === "h63".U && idWord(14, 13) =/= 1.U) ||
      (idOpcode === "h7b".U && idWord(14, 12) === 3.U && !idWord(19) && idWord(24, 20) === 0.U)
    val idTarget = idPC + Mux(idJal,
      Cat(Fill(11, idWord(31)), idWord(31), idWord(19, 12), idWord(20), idWord(30, 21), 0.U(1.W)),
      Cat(Fill(19, idWord(31)), idWord(31), idWord(7), idWord(30, 25), idWord(11, 8), 0.U(1.W)))
    val idReturn = idOpcode === "h6b".U && idWord(14, 7) === 0.U && idWord(31, 20) === 1.U
    val idPredictedTarget = Mux(idReturn, Cat(rasTopNative, 0.U(2.W)), idTarget)
    val predict = idValid && !idError && !idPredicted && !flush && live && idPC(1, 0) === 0.U &&
      Mux(idReturn, rasCount =/= 0.U,
        (idJal || (idConditional && bht(idPC(7, 2))(1))) && idTarget(1, 0) === 0.U)
    when(predict) { idPredictedNative := idPredictedTarget(31, 2) }
    btb.io.clk := clk
    btb.io.read_addr := fetchAddress(7, 2)
    btb.io.write_enable := predict && !idReturn
    btb.io.write_addr := idPC(7, 2)
    btb.io.write_data := Cat(idPC(31, 8), idTarget(31, 2), idJal)
    when(predict && !idReturn) { btbValid := btbValid | UIntToOH(idPC(7, 2), 64) }
    val btbRow = btb.io.read_data
    val btbTaken = btbValid(fetchAddress(7, 2)) && btbRow(54, 31) === fetchAddress(31, 8) &&
      (btbRow(0) || bht(fetchAddress(7, 2))(1))
    when(issue) {
      exValid := true.B
      exWord := idWord
      exPC := idPC
      exError := idError
      exBank := bank
      exPredicted := predict || idPredicted
      exPredictedNative := Mux(predict, idPredictedTarget(31, 2), idPredictedNative)
      exReadA := readA
      exReadB := readB
      heldValidA := false.B
      heldValidB := false.B
      mStarted := false.B
      eaDone := false.B
      mdDone := false.B
      jtPhase := 0.U
    }
    // Fetch runs ahead of execution: one request in flight plus the ID and
    // skid slots. A request starts only when its response is certain to have a
    // free slot, so the bus completion is never refused. A taken branch the
    // target table already knows costs nothing. With a zero-wait bus,
    // straight-line code issues one word per clock; a redirect from M costs
    // three clocks and a predicted one costs one. FENCE.I redirects from M,
    // so its successor is fetched after the fence and older stores finished. Prefetched words, including
    // fetch errors, have no effect unless issued.
    val fetchDone = fetchValid && i_ready
    val deliver = fetchDone && !fetchKilled && !flush && !predict && !stopped
    val idStays = idValid && !issue
    val buffered = idStays.asUInt +& skidValid.asUInt +& deliver.asUInt
    val launch = (!fetchValid || i_ready) && !stopped &&
      (flush || predict || buffered <= 1.U)
    // The word arriving now is a known taken branch and the next request
    // starts in this clock: fetch its target instead of the next word.
    val useBtb = deliver && launch && btbTaken
    val redirectTarget = Mux(flush, flushTarget, idPredictedTarget)
    when(flush) {
      idValid := false.B
      skidValid := false.B
      idPredicted := false.B
    }.elsewhen(idStays) {
      when(predict) {
        // Younger sequential words are on the wrong path.
        skidValid := false.B
        idPredicted := true.B
      }.elsewhen(deliver) {
        skidValid := true.B
        skidWord := i_data
        skidPC := fetchAddress
        skidError := i_error
        skidPredicted := useBtb
        skidNative := btbRow(30, 1)
      }
    }.elsewhen(skidValid && !predict) {
      idValid := true.B
      idWord := skidWord
      idPC := skidPC
      idError := skidError
      idPredicted := skidPredicted
      idPredictedNative := skidNative
      skidValid := deliver
      when(deliver) {
        skidWord := i_data
        skidPC := fetchAddress
        skidError := i_error
        skidPredicted := useBtb
        skidNative := btbRow(30, 1)
      }
    }.otherwise {
      idValid := deliver
      idPredicted := useBtb
      idPredictedNative := btbRow(30, 1)
      skidValid := false.B
      when(deliver) {
        idWord := i_data
        idPC := fetchAddress
        idError := i_error
      }
    }
    val launchAddress = Mux(flush || predict, redirectTarget,
      Mux(fetchRedirected, fetchPC, Mux(useBtb, Cat(btbRow(30, 1), 0.U(2.W)), fetchAddress + 4.U)))
    i_next_addr := Mux(launch, launchAddress, fetchAddress)
    when(fetchDone) {
      fetchValid := false.B
      fetchKilled := false.B
    }
    when(launch) {
      fetchValid := true.B
      fetchKilled := false.B
      fetchAddress := launchAddress
      fetchRedirected := false.B
    }.elsewhen(flush || predict) {
      fetchPC := redirectTarget
      fetchRedirected := true.B
      // An outstanding request cannot be withdrawn. Drain then discard it,
      // including an error on a wrong-path fetch, before fetching the new PC.
      when(fetchValid && !i_ready) { fetchKilled := true.B }
    }
  }
}
