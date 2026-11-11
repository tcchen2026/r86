package r86chip

import chisel3._
import chisel3.util._

class CoreIO(d: CacheShape) extends Bundle {
  val i_addr = Output(UInt(32.W))
  val i_check = Output(Bool())
  val i_hit = Input(Bool())
  val i_word = Input(UInt(32.W))
  val i_error = Input(Bool())
  val i_flush = Output(Bool())
  val d_addr = Output(UInt(32.W))
  val d_hit = Input(Bool())
  val d_way = Input(Bool())
  val d_word = Input(UInt(32.W))
  val d_words = Input(Vec(2, UInt(32.W)))
  val d_error = Input(Bool())
  val d_take = Output(Bool())
  val d_fill = Output(Bool())
  val d_fill_line = Output(UInt(27.W))
  val s_valid = Output(Bool())
  val s_slot = Output(UInt(d.slotBits.W))
  val s_way = Output(Bool())
  val s_data = Output(UInt(32.W))
  val s_strb = Output(UInt(4.W))
  val quiet = Output(Bool())
  val busy = Input(Bool())
  val dev_valid = Output(Bool())
  val dev_addr = Output(UInt(32.W))
  val dev_wdata = Output(UInt(32.W))
  val dev_wstrb = Output(UInt(4.W))
  val dev_ready = Input(Bool())
  val dev_rdata = Input(UInt(32.W))
  val dev_error = Input(Bool())
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
  // Event pulses for testbench counters; see `events` at the end of Core.
  val events = Output(UInt(16.W))
}

// Six stages shaped for translated x86 code: fetch and decode (Front), then
//   A  operands, address, first-level mapping; small ALU
//   M  cache answer, guest qualification, second-level mapping, jump table
//   X  full ALU with x86 status, branches, every state change: the commit point
//   W  register file and data cache writes
// Memory comes before the full ALU, so a load feeds an operation without a
// wait. The small ALU in A gives add/subtract/logic results two stages early,
// so pointer arithmetic feeds the next address without a wait either.
//
// Nothing before X changes architectural state, and nothing waits on a cache
// or table answer: an absent cache line, a first-level mapping refill or a
// load that passed a store still in flight makes X start the instruction
// again from fetch (replay). X is never undone, so its effects are final.
// Words that change what A or M read (mappings, segments, the jump table)
// and accesses done by the word-access unit restart fetch behind them.
class Core(banks: Int, d: CacheShape) extends Module {
  require(banks == 1 || banks == 2)
  override def desiredName: String = "r86_core"
  val io = IO(new CoreIO(d))

  // Architectural state; rv32.rs defines it.
  val bank = RegInit(false.B)
  val runtimeReturn = RegInit(0.U(32.W))
  val guestReturn = RegInit(0.U(32.W))
  val exitReason = RegInit(0.U(12.W))
  val guestFlags = RegInit(0.U(32.W))
  val pendingFlags = RegInit(0.U(32.W))
  // Guest data mappings: l2 is the architectural table, 1024 rows (tag10,
  // page20, R/W2) in block RAM; l1 is an invisible 64-row copy (tag14,
  // page20, R/W2) read inside A. Every l1 row is also in l2.
  val l1 = Module(new LutRam(6, 36))
  val l1Valid = RegInit(0.U(64.W))
  val l2 = new EpochTable(clock, 10, 32)
  // Four fetch rows (tag18, page20, X1) for the interpreter's code reads.
  val guestITlb = Reg(Vec(4, UInt(39.W)))
  val guestITlbValid = RegInit(0.U(4.W))
  val guestBasePage = RegInit(0.U(20.W))
  val guestLimitPage = RegInit(0.U(20.W))
  // Jump table: guest EIP (tag22) to native word (target30).
  val jt = new EpochTable(clock, 10, 52)
  // Guest return stack: (return EIP32, native30) rows.
  val ras = Module(new LutRam(3, 62))
  val rasTop = RegInit(0.U(3.W))
  val rasCount = RegInit(0.U(4.W))
  val tick = RegInit(0.U(32.W))
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
  val stopped = RegInit(false.B)
  val stoppedPC = RegInit(0.U(32.W))
  val stoppedCause = RegInit(0.U(4.W))
  val stoppedValue = RegInit(0.U(32.W))

  val front = Module(new Front)
  val access = Module(new WordAccess(d))
  val md = Module(new MulDiv)
  val rf = Seq.fill(2)(Module(new LutRam(6, 32)))

  // Pipeline registers. `Done` says Result already holds the word's value;
  // `Ok` says an operand has been captured.
  val aValid = RegInit(false.B)
  val aPC = Reg(UInt(32.W))
  val aWord = Reg(UInt(32.W))
  val aBank = Reg(Bool())
  val aError = Reg(Bool())
  val aPred = Reg(Bool())
  val aPredT = Reg(UInt(30.W))
  val aHit = Reg(Bool())
  val aCounter = Reg(UInt(2.W))
  val mValid = RegInit(false.B)
  val mPC = Reg(UInt(32.W))
  val mWord = Reg(UInt(32.W))
  val mBank = Reg(Bool())
  val mError = Reg(Bool())
  val mPred = Reg(Bool())
  val mPredT = Reg(UInt(30.W))
  val mHit = Reg(Bool())
  val mCounter = Reg(UInt(2.W))
  val mFixed = Reg(Bool())
  val mA = Reg(UInt(32.W))
  val mB = Reg(UInt(32.W))
  val mAok = Reg(Bool())
  val mBok = Reg(Bool())
  val mResult = Reg(UInt(32.W))
  val mDone = Reg(Bool())
  val mWrite = RegInit(false.B)
  val mRD = Reg(UInt(6.W))
  val mAddr = Reg(UInt(32.W))
  val mLinear = Reg(UInt(32.W))
  val xValid = RegInit(false.B)
  val xPC = Reg(UInt(32.W))
  val xWord = Reg(UInt(32.W))
  val xBank = Reg(Bool())
  val xError = Reg(Bool())
  val xPred = Reg(Bool())
  val xPredT = Reg(UInt(30.W))
  val xHit = Reg(Bool())
  val xCounter = Reg(UInt(2.W))
  val xFixed = Reg(Bool())
  val xA = Reg(UInt(32.W))
  val xB = Reg(UInt(32.W))
  val xResult = Reg(UInt(32.W))
  val xDone = Reg(Bool())
  val xWrite = RegInit(false.B)
  val xRD = Reg(UInt(6.W))
  val xAddr = Reg(UInt(32.W))
  val xLinearPage = Reg(UInt(20.W))
  val xLoad = Reg(UInt(32.W))
  val xWay = Reg(Bool())
  val xReason = Reg(UInt(3.W))
  val xRefill = Reg(Bool())
  val xRow = Reg(UInt(22.W))
  val xReplay = Reg(Bool())
  val xPassed = Reg(Bool())
  val xBusError = Reg(Bool())
  val xSlow = Reg(Bool())
  val xSplit = Reg(Bool())
  val xTlbHit = Reg(Bool())
  val xJtHit = Reg(Bool())
  // Where the word goes if it jumps, and whether fetch was sent there. For
  // JAL, JALR, branches and XJT this is known in M.
  val xTarget = Reg(UInt(32.W))
  val xTargetOk = Reg(Bool())
  val xRasMatch = Reg(Bool())
  val xTargetBad = Reg(Bool())
  // Decided in M from the word alone: its fault if any, which unit gives its
  // value, and that unit's controls.
  val xStatic = Reg(Bool())
  val xCause = Reg(UInt(4.W))
  val xFaultValue = Reg(UInt(32.W))
  val xUnit = Reg(UInt(7.W))
  val xLane = Reg(UInt(2.W))
  val xControl = Reg(UInt(7.W))
  val xShiftKind = Reg(UInt(3.W))
  val xSubtract = Reg(Bool())
  val xSystem = Reg(Bool())
  val wWrite = RegInit(false.B)
  val wRD = Reg(UInt(6.W))
  val wValue = Reg(UInt(32.W))
  // A committed store on its way into the cache, and the one before it.
  val sValid = RegInit(false.B)
  val sAddr = Reg(UInt(30.W))
  val sWay = Reg(Bool())
  val sData = Reg(UInt(32.W))
  val sStrb = Reg(UInt(4.W))
  val pValid = RegInit(false.B)
  val pAddr = Reg(UInt(30.W))
  val flush = RegInit(false.B)
  val flushTarget = Reg(UInt(32.W))
  // A's own correction of fetch, one clock after the branch left A. The word
  // then in A is from the wrong path.
  val redo = RegInit(false.B)
  val redoTarget = Reg(UInt(32.W))

  // Set by X below.
  val xOut = Wire(UInt(32.W))
  val xLeaves = Wire(Bool())
  val xRedirect = Wire(Bool())
  val live = !stopped
  val xFree = !xValid || xLeaves
  val mGo = mValid && xFree
  val mFree = !mValid || mGo

  val A = new Fields(aWord)
  val M = new Fields(mWord)
  val X = new Fields(xWord)
  // A load in X already holds its value, unless the word-access unit does it.
  val xQuick = X.memory && !X.store && !xSlow

  // ---- D to A: issue ----
  // Decided here, a clock early, so A starts from registers: which register
  // each operand is, where its newest producer will be when the word is in A,
  // and the constant part of its memory address.
  val H = new Fields(front.io.head.word)
  val fromRF :: fromM :: fromX :: fromW :: Nil = Enum(4)
  val aRA = Reg(UInt(6.W))
  val aRB = Reg(UInt(6.W))
  val aZeroA = Reg(Bool())
  val aZeroB = Reg(Bool())
  val aSrcA = Reg(UInt(2.W))
  val aSrcB = Reg(UInt(2.W))
  // Segment base plus displacement; aBaseOk is false while a segment named
  // by a register is still to be read (one extra clock in A).
  val aBase = Reg(UInt(32.W))
  val aBaseOk = Reg(Bool())
  val headCross = (banks == 2).B && !bank && H.is(0x7b) && H.f3 === 6.U && H.f7 === 0.U
  val headRA = Cat(headCross || bank, H.rs1)
  val headRB = Cat(bank, H.rs2)
  val aDest = Cat(A.crossWrite || aBank, A.dest)
  val aWrites = A.writes && A.dest =/= 0.U
  // Where the newest producer of register r is after this clock edge. The
  // word now in A, if any, moves to M whenever a new word is issued.
  def producer(r: UInt): UInt =
    Mux(aValid && aWrites && aDest === r, fromM,
      Mux(mValid && mWrite && mRD === r, Mux(mGo, fromX, fromM),
        Mux(xValid && xWrite && xRD === r, Mux(xLeaves, fromW, fromX), fromRF)))
  // A producer moves on with its stage.
  def follow(src: UInt): UInt = MuxLookup(src, fromRF)(Seq(
    fromM -> Mux(mGo, fromX, fromM), fromX -> Mux(xLeaves, fromW, fromX)))

  // ---- A: operands, small ALU, address ----
  rf(0).io.clk := clock
  rf(1).io.clk := clock
  rf(0).io.read_addr := aRA
  rf(1).io.read_addr := aRB
  def operand(src: UInt, zero: Bool, stored: UInt): (UInt, Bool, Bool) = {
    val value = Mux(zero, 0.U, MuxLookup(src, stored)(Seq(
      fromM -> mResult, fromX -> Mux(xDone, xResult, xLoad), fromW -> Mux(wWrite, wValue, stored))))
    val ok = zero || MuxLookup(src, true.B)(Seq(fromM -> mDone, fromX -> (xDone || xQuick)))
    // Not usable in this clock, but X delivers it at this edge.
    val arriving = !zero && src === fromX && !xDone && !xQuick && xLeaves
    (value, ok, arriving)
  }
  val (opA, okA, arrivingA) = operand(aSrcA, aZeroA, rf(0).io.read_data)
  val (opB, okB, arrivingB) = operand(aSrcB, aZeroB, rf(1).io.read_data)

  val smallB = Mux(A.earlyUsesB, opB, A.immI)
  val smallA = MuxLookup(Mux(A.zbaOk, A.f3, 0.U), opA)(Seq(
    2.U -> Cat(opA(30, 0), 0.U(1.W)),
    4.U -> Cat(opA(29, 0), 0.U(2.W)),
    6.U -> Cat(opA(28, 0), 0.U(3.W))))
  val smallSub = A.earlyKind === 4.U
  val smallSum = smallA + Mux(smallSub, ~smallB, smallB) + smallSub.asUInt
  val smallValue = Mux(A.is(0x37), A.immU, Mux(A.is(0x17), aPC + A.immU,
    Mux(A.is(0x6f) || A.is(0x67), aPC + 4.U,
      MuxLookup(A.earlyKind, smallSum)(Seq(
        1.U -> (opA | smallB), 2.U -> (opA & smallB), 3.U -> (opA ^ smallB))))))
  val smallOk = A.early && (A.earlyConstant || (okA && (!A.earlyUsesB || okB)))

  // The native address or guest linear address is one addition after the
  // operand; the guest offset (for the limit check in M) is another.
  val offset = opA + A.displacement
  val linear = opA + aBase
  l1.io.clk := clock
  l1.io.read_addr := linear(17, 12)
  val l1Slot = l1.io.read_data
  val itlbSlot = guestITlb(linear(13, 12))
  val fetchPresent = guestITlbValid(linear(13, 12)) && itlbSlot(38, 21) === linear(31, 14)
  val tlbPage = Mux(A.probeFetch, itlbSlot(20, 1), l1Slot(21, 2))
  val address = Mux(A.guestMemory || A.xtlook, Cat(tlbPage, linear(11, 0)), linear)

  // A waits for an operand it needs now, for XEA32 ahead of it (whose
  // register write is only known at commit and which uses the cache port),
  // and while a table is being swept.
  val aWait = (A.needA && !okA) || (A.needB && !aBaseOk) ||
    (mValid && M.ea) || (xValid && X.ea) || l2.sweeping || jt.sweeping ||
    (A.xjt && ((mValid && (M.pushRet || M.xjt)) || (xValid && (X.pushRet || X.xjt))))
  val aGo = aValid && !redo && !aWait && mFree
  when(!aBaseOk && okB) {
    aBase := segmentBases(opB(2, 0))
    aBaseOk := true.B
  }

  // JALR and RV conditional branches whose operands are ready are resolved
  // here, two stages before X: fetch is corrected at once and the word goes
  // on as correctly predicted. X still checks it. JALR waits for its operand.
  val aTaken = MuxLookup(A.f3, false.B)(Seq(
    0.U -> (opA === opB), 1.U -> (opA =/= opB),
    4.U -> (opA.asSInt < opB.asSInt), 5.U -> (opA.asSInt >= opB.asSInt),
    6.U -> (opA < opB), 7.U -> (opA >= opB)))
  val aJalr = A.is(0x67) && okA
  val aBranch = A.is(0x63) && okA && okB
  val aJump = aJalr || (aBranch && aTaken)
  val aNext = Mux(aJalr, (opA + A.immI) & "hfffffffe".U, Mux(aTaken, aPC + A.immB, aPC + 4.U))
  val aWrong = (aJalr || aBranch) && !aError && aNext(1, 0) === 0.U &&
    Mux(aPred, !aJump || aPredT =/= aNext(31, 2), aJump)
  redo := aGo && aWrong && !xRedirect
  redoTarget := aNext

  val aFree = !aValid || aGo
  front.io.take := front.io.valid && aFree && !io.busy && live
  aSrcA := follow(aSrcA)
  aSrcB := follow(aSrcB)
  when(front.io.take) {
    aValid := true.B
    aPC := front.io.head.pc
    aWord := front.io.head.word
    aBank := bank
    aError := front.io.head.error
    aPred := front.io.head.predicted
    aPredT := front.io.head.target
    aHit := front.io.head.hit
    aCounter := front.io.head.counter
    aRA := headRA
    aRB := headRB
    aZeroA := H.rs1 === 0.U
    aZeroB := H.rs2 === 0.U
    aSrcA := producer(headRA)
    aSrcB := producer(headRB)
    aBase := Mux(H.guestMemory, segmentBases(H.guestSegment(0.U)) + H.displacement,
      Mux(H.nativeMemory, H.displacement, 0.U))
    aBaseOk := !H.guestRegisterSegment
  }.elsewhen(aGo || redo) { aValid := false.B }

  // The block RAMs read in M keep M's own address while it waits there.
  val hold = mValid && !mGo
  io.d_addr := Mux(access.io.steer, access.io.d_addr,
    Cat(Mux(hold, mAddr(31, 2), address(31, 2)), 0.U(2.W)))
  l2.readIndex := Mux(hold, mLinear(21, 12), linear(21, 12))
  jt.readIndex := Mux(hold, mA(9, 0), opA(9, 0))

  // First-level row and fetch-row answer, compared in M.
  val mL1Tag = Reg(UInt(14.W))
  val mL1Rights = Reg(UInt(2.W))
  val mL1Valid = Reg(Bool())
  val mFetchPresent = Reg(Bool())
  val mFetchRight = Reg(Bool())
  val mLimit = Reg(UInt(32.W))
  val mRights = Reg(UInt(3.W))
  val segmentA = A.guestSegment(opB)
  when(aGo) {
    mValid := true.B
    mPC := aPC
    mWord := aWord
    mBank := aBank
    mError := aError
    mPred := Mux(aWrong, aJump, aPred)
    mPredT := Mux(aWrong, aNext(31, 2), aPredT)
    mHit := aHit
    mCounter := aCounter
    mFixed := aWrong
    mA := Mux(arrivingA, xOut, opA)
    mB := Mux(arrivingB, xOut, opB)
    mAok := okA || arrivingA
    mBok := okB || arrivingB
    mResult := Mux(A.guestMemory, offset, smallValue)
    mDone := Mux(A.guestMemory, A.guestUpdate, smallOk)
    mWrite := aWrites
    mRD := aDest
    mAddr := address
    mLinear := linear
    mL1Tag := l1Slot(35, 22)
    mL1Rights := l1Slot(1, 0)
    mL1Valid := l1Valid(linear(17, 12))
    mFetchPresent := fetchPresent
    mFetchRight := itlbSlot(0)
    mLimit := segmentLimits(segmentA)
    mRights := segmentRights(segmentA)
  }.elsewhen(mGo) { mValid := false.B }
  // An operand still owed to the word in M is the result of the word in X,
  // and M moves on exactly when X does: it is taken at that edge (below).

  // ---- M: cache answer and guest qualification ----
  val contextOk = guestContextValid && guestControl(0) && !guestControl(3)
  val l1Present = mL1Valid && mL1Tag === mLinear(31, 18)
  val tlbRightsOk = Mux(M.probeFetch, mFetchRight,
    (mL1Rights & M.probeRequired(1, 0)) === M.probeRequired(1, 0))
  val naturallyAligned = M.probeWidth === 0.U ||
    (M.probeWidth === 1.U && !mLinear(0)) ||
    (M.probeWidth === 2.U && mLinear(1, 0) === 0.U)
  val samePage = M.probeWidth === 0.U ||
    (M.probeWidth === 1.U && mLinear(11, 0) =/= "hfff".U) ||
    (M.probeWidth === 2.U && mLinear(11, 0) <= "hffc".U)
  val mSpanOk = Mux(M.guestMemory && M.guestSpan, samePage, naturallyAligned)
  val mTlbHit = Mux(M.probeFetch, mFetchPresent, l1Present) && tlbRightsOk && mSpanOk
  // Only an absent l1 row leaves the answer to l2; a present row is final.
  val mWalk = !M.probeFetch && !l1Present && mSpanOk
  val mSplit = M.guestMemory && M.guestSpan &&
    ((M.guestWidth === 2.U && mAddr(1, 0) =/= 0.U) ||
     (M.guestWidth === 1.U && mAddr(1, 0) === 3.U))
  // Inclusive limit: the offset must not exceed it, and the room above the
  // offset must hold the remaining bytes.
  val limitRoom = Cat(0.U(1.W), mLimit) - Cat(0.U(1.W), mResult)
  val segmentOk = (mRights & M.guestRequired) === M.guestRequired &&
    !limitRoom(32) && MuxLookup(M.guestWidth, true.B)(Seq(
      1.U -> limitRoom(31, 0).orR,
      2.U -> (limitRoom(31, 2).orR || limitRoom(1, 0) === 3.U)))
  val mReason = Mux(!contextOk, 1.U(3.W),
    Mux(!segmentOk, 2.U(3.W), Mux(!mSpanOk, 3.U(3.W), Mux(!mTlbHit, 4.U(3.W), 0.U(3.W)))))
  val walk = mWalk && Mux(M.guestMemory, contextOk && segmentOk, M.xtlook)
  val refill = walk && l2.live && l2.payload(31, 22) === mLinear(31, 22)
  val guestOk = !M.guestMemory || mReason === 0.U
  val slow = M.memory && guestOk && (mSplit || mAddr(31))
  val cached = M.memory && guestOk && !mSplit && !mAddr(31)
  val absent = cached && !io.d_hit && !io.d_error
  // A load must not pass a store to the same word that has not reached the
  // cache RAM yet: the store in X, the one being written, or the one written
  // on the edge this load's read was latched.
  val word = mAddr(31, 2)
  val passed = cached && !M.store &&
    ((xValid && X.memory && X.store && xAddr(31, 2) === word) ||
     (sValid && sAddr === word) || (pValid && pAddr === word))
  io.d_fill := (absent && mGo && !xRedirect) || access.io.d_fill
  io.d_fill_line := Mux(access.io.d_fill, access.io.addr(31, 5), mAddr(31, 5))
  io.d_take := (cached && mGo && !xRedirect) || access.io.d_take

  def extend(word: UInt, f: Fields): UInt = {
    val lane = word(15, 0)
    Mux(f.guestMemory,
      MuxLookup(f.guestWidth, word)(Seq(
        0.U -> Cat(Fill(24, f.guestSigned && lane(7)), lane(7, 0)),
        1.U -> Cat(Fill(16, f.guestSigned && lane(15)), lane))),
      MuxLookup(f.f3, word)(Seq(
        0.U -> Cat(Fill(24, lane(7)), lane(7, 0)),
        1.U -> Cat(Fill(16, lane(15)), lane),
        4.U -> lane(7, 0),
        5.U -> lane)))
  }
  // The word's jump target, and the return stack's opinion of an XJT.R. No
  // word that changes the return stack is ahead of an XJT in M.
  ras.io.clk := clock
  ras.io.read_addr := rasTop
  val rasTopEip = ras.io.read_data(61, 30)
  val rasTopNative = ras.io.read_data(29, 0)
  val mRasMatch = M.xjt && mWord(20) && rasCount =/= 0.U && rasTopEip === mA
  val mTarget = Mux(M.is(0x67), (mA + M.immI) & "hfffffffe".U,
    Mux(M.xjt, Cat(Mux(mRasMatch, rasTopNative, jt.payload(29, 0)), 0.U(2.W)),
      mPC + Mux(M.is(0x6f), M.immJ, M.immB)))
  // Faults that depend only on the word, its bank and (for a segment named
  // by a register) rs2, which such a word already had in A.
  def staticDecode(F: Fields, w: UInt, bk: Bool, b: UInt): Bool = {
    val f3 = F.f3
    val f7 = F.f7
    val rd = F.rd
    val rs1 = F.rs1
    val rs2 = F.rs2
    val hi = w(31, 20)
    val kind = f7(2, 0)
    val segment = F.guestSegment(b)
    // Dynamic selectors use the full rs2 value: 0x103 is illegal, not DS.
    val guestOk = Mux(F.guestOld,
      f3(1, 0) =/= 3.U && segment < 6.U &&
        (!F.guestRegisterSegment || (!b(31, 3).orR && f7(2, 0) === 0.U)) &&
        Mux(F.store, rd === 0.U && !f7(3) && !f7(4) && !F.guestRegisterSegment,
          (F.guestRegisterSegment || rs2 === 0.U) && (!f7(5) || f7(4))) &&
        (!f7(4) || (segment === 1.U && !f7(3))),
      F.guestLoadD || f3(1, 0) =/= 3.U || F.pushRet)
    val bad = WireDefault(true.B)
    switch(F.opcode) {
      is("h37".U, "h17".U, "h6f".U) { bad := false.B }
      is("h67".U) { bad := f3 =/= 0.U }
      is("h63".U) { bad := f3 === 2.U || f3 === 3.U }
      is("h13".U, "h33".U) {
        bad := Mux(F.muldiv, false.B, Mux(F.zba, f3 =/= 2.U && f3 =/= 4.U && f3 =/= 6.U,
          Mux(F.is(0x33), f7 =/= 0.U && !(f7 === "h20".U && (f3 === 0.U || f3 === 5.U)),
            (f3 === 1.U && f7 =/= 0.U) || (f3 === 5.U && f7 =/= 0.U && f7 =/= "h20".U))))
      }
      is("h03".U, "h23".U) { bad := !(f3 <= 2.U || (!F.store && (f3 === 4.U || f3 === 5.U))) }
      is("h0f".U) { bad := f3 =/= 0.U && f3 =/= 1.U }
      is("h0b".U) {
        bad := Mux(f3 < 4.U,
          (f7(4) && f3 < 2.U) || Mux(f7(6), f7(5) || f7(3, 0).orR,
            (f7(5) && kind =/= 0.U && kind =/= 5.U) ||
              (kind === 7.U && (rs2 =/= 0.U || f7(5, 4) =/= 0.U))),
          hi =/= 0.U || (f3 === 4.U && rd =/= 0.U) || (f3 === 7.U && rs1(4)))
      }
      is("h1b".U) { bad := F.incdec && hi =/= 1.U && hi =/= "hfff".U }
      is("h3b".U) { bad := f3 > 4.U || f7 > 1.U }
      is("h2b".U) {
        bad := MuxLookup(f3, true.B)(Seq(
          0.U -> (w(31, 24) =/= 0.U || w(21, 20) === 3.U),
          1.U -> (rd =/= 0.U || f7 > 1.U),
          2.U -> (rd =/= 0.U || hi =/= 0.U),
          3.U -> (rd =/= 0.U || rs1 =/= 0.U || hi =/= 0.U),
          4.U -> (rd =/= 0.U || f7 =/= 0.U),
          5.U -> (rd =/= 0.U || f7 =/= 0.U),
          6.U -> (rd >= 6.U || f7 >= 8.U),
          7.U -> MuxLookup(f7, true.B)(Seq(
            0.U -> (rd =/= 0.U || rs2 =/= 0.U),
            1.U -> (rs2 =/= 0.U || rs1 >= 8.U),
            2.U -> (rd =/= 0.U || rs2 =/= 0.U)))))
      }
      is("h5b".U, "h07".U, "h27".U, "h47".U, "h77".U) { bad := !guestOk }
      is("h6b".U) {
        bad := MuxLookup(f3, true.B)(Seq(
          0.U -> (rd =/= 0.U || w(31, 21) =/= 0.U),
          1.U -> (rd =/= 0.U || f7 =/= 0.U),
          2.U -> (rd =/= 0.U || hi =/= 0.U),
          3.U -> (rd =/= 0.U || rs1 =/= 0.U || hi =/= 0.U),
          4.U -> (!bk || rd =/= 0.U || rs1 =/= 0.U),
          5.U -> (rd =/= 0.U || hi =/= 0.U),
          6.U -> (rs1 =/= 0.U || hi =/= 0.U)))
      }
      is("h7b".U) {
        val banked = MuxLookup(f3, true.B)(Seq(
          4.U -> (bk || rd =/= 0.U || hi =/= 0.U),
          5.U -> (!bk || rd =/= 0.U || rs1 =/= 0.U),
          6.U -> (bk || rs2 =/= 0.U || f7 > 1.U || (f7 === 1.U && rs1 > 2.U)),
          7.U -> (bk || f7 =/= 0.U || rs2 =/= 0.U)))
        bad := MuxLookup(f3, (banks == 1).B || banked)(Seq(
          0.U -> false.B,
          1.U -> (rs1 =/= 0.U || hi =/= 0.U),
          2.U -> (rd =/= 0.U || hi =/= 0.U),
          3.U -> (rs1(4) || rs2 =/= 0.U)))
      }
    }
    bad
  }
  val mBad = staticDecode(M, mWord, mBank, mB)
  val mMisaligned = M.nativeMemory && !mBad &&
    ((M.f3(1, 0) === 1.U && mAddr(0)) || (M.f3(1, 0) === 2.U && mAddr(1, 0) =/= 0.U))
  val mPcFault = mPC(1, 0) =/= 0.U
  val mTargetFault = (M.is(0x6f) || M.is(0x67)) && !mBad && mTarget(1, 0) =/= 0.U
  val mEcall = mWord === "h00000073".U
  val mEbreak = mWord === "h00100073".U
  val mArith = (M.is(0x13) || M.is(0x33)) && !M.muldiv
  val mPlain = mArith && !M.zba
  val units = Seq(
    (M.is(0x0b) && M.f3 < 4.U) || M.xalui,
    M.is(0x3b) || (mPlain && (M.f3 === 1.U || M.f3 === 5.U)),
    M.memory && !M.store,
    mArith && (M.zba || M.f3 === 0.U),
    mPlain && (M.f3 === 4.U || M.f3 >= 6.U),
    mPlain && (M.f3 === 2.U || M.f3 === 3.U))
  // Both ways are aligned and extended; the hit way is chosen last.
  val loadWays = io.d_words.map(w => extend((w >> Cat(mAddr(1, 0), 0.U(3.W)))(31, 0), M))
  val loadValue = Mux(io.d_way, loadWays(1), loadWays(0))

  when(mGo) {
    xValid := true.B
    xPC := mPC
    xWord := mWord
    xBank := mBank
    xError := mError
    xPred := mPred
    xPredT := mPredT
    xHit := mHit
    xCounter := mCounter
    xFixed := mFixed
    xA := Mux(mAok, mA, xOut)
    // An immediate operand takes rs2's place.
    val immediate = M.is(0x13) || M.xalui
    val count = M.is(0x3b) && M.f7(0)
    xB := Mux(!immediate && !count && !mBok, xOut,
      Mux(immediate, Mux(M.incdec, 1.U(32.W), M.immI), Mux(count, M.rs2, mB)))
    xStatic := mPcFault || mError || mBad || mMisaligned || mTargetFault
    xCause := Mux(mPcFault, 0.U, Mux(mError, 1.U, Mux(mTargetFault, 0.U,
      Mux(mMisaligned, Mux(M.store, 6.U, 4.U), Mux(mEcall, 11.U, Mux(mEbreak, 3.U, 2.U))))))
    xFaultValue := Mux(mPcFault || mError, mPC, Mux(mTargetFault, mTarget,
      Mux(mMisaligned, mAddr, Mux(mEcall, 0.U, Mux(mEbreak, mPC, mWord)))))
    xTargetBad := mTarget(1, 0) =/= 0.U
    xUnit := Cat(!units.reduce(_ || _), Cat(units.reverse))
    xLane := Mux(M.xalui, 0.U, M.f3(1, 0))
    // XALUI is XALU on the dword lane with an immediate, always committed.
    // funct3 7 is INC (imm 1) or DEC (imm -1): ADD or SUB of one, CF kept.
    xControl := Mux(M.xalui,
      Cat(0.U(1.W), M.incdec, 0.U(1.W), 1.U(1.W), Mux(M.incdec, Mux(mWord(31), 5.U(3.W), 0.U(3.W)), M.f3)), M.f7)
    xShiftKind := Mux(M.is(0x3b), M.f3, Mux(M.f3 === 1.U, 0.U(3.W), Mux(mWord(30), 2.U(3.W), 1.U(3.W))))
    xSubtract := M.is(0x33) && M.f7 === "h20".U && M.f3 === 0.U
    xSystem := (M.is(0x2b) && M.f3 =/= 0.U) || (M.is(0x6b) && M.f3 >= 1.U && M.f3 <= 3.U) ||
      (M.is(0x7b) && (M.f3 === 2.U || M.f3 === 4.U))
    xResult := mResult
    xDone := mDone
    xWrite := mWrite
    xRD := mRD
    xAddr := mAddr
    xLinearPage := mLinear(31, 12)
    xLoad := loadValue
    xWay := io.d_way
    xReason := Mux(M.guestMemory, mReason, 0.U)
    xRefill := refill
    xRow := l2.payload(21, 0)
    xReplay := absent || passed
    xPassed := passed && !absent
    xBusError := cached && io.d_error
    xSlow := slow
    xSplit := mSplit
    xTlbHit := mTlbHit
    xJtHit := jt.live && jt.payload(51, 30) === mA(31, 10)
    xTarget := mTarget
    xTargetOk := mPredT === mTarget(31, 2)
    xRasMatch := mRasMatch
  }.elsewhen(xLeaves) { xValid := false.B }

  // ---- X: execute and commit ----
  // Everything that depends only on the word was decided in M (xStatic,
  // xUnit, xControl, the immediate in xB, the jump target). X adds what
  // depends on operand values and on state.
  val a = xA
  val b = xB
  val flagsNow = guestFlags
  val f3 = X.f3
  val f7 = X.f7
  val rd = X.rd
  val rs1 = X.rs1
  val rs2 = X.rs2
  val store = X.store
  val addA = MuxLookup(Mux(X.zba, f3, 0.U), a)(Seq(
    2.U -> Cat(a(30, 0), 0.U(1.W)),
    4.U -> Cat(a(29, 0), 0.U(2.W)),
    6.U -> Cat(a(28, 0), 0.U(3.W))))
  val addSub = addA + Mux(xSubtract, ~b, b) + xSubtract.asUInt
  val logic = MuxLookup(f3(1, 0), a ^ b)(Seq(2.U -> (a | b), 3.U -> (a & b)))
  val less = Mux(f3(0), a < b, a.asSInt < b.asSInt)
  val xCondition = XFlags.condition(rs1, flagsNow)
  val (guestResult, guestStatus) = XFlags.alu(a, b, xLane, xControl, flagsNow)
  val (shiftResult, shiftStatus) = XFlags.shift(a, b(4, 0), xShiftKind, flagsNow)
  val strobes = (MuxLookup(X.accessWidth, 15.U(8.W))(Seq(
    0.U -> 1.U(8.W), 1.U -> 3.U(8.W))) << xAddr(1, 0))(7, 0)
  val storeData = (Cat(0.U(32.W), b) << Cat(xAddr(1, 0), 0.U(3.W)))(63, 0)

  val mStarted = RegInit(false.B)
  val mdDone = RegInit(false.B)
  md.io.a := a
  md.io.b := b
  md.io.operation := f3
  md.io.start := xValid && X.muldiv && !mStarted && live
  md.io.abort := false.B
  when(md.io.start) { mStarted := true.B }
  when(md.io.done) { mdDone := true.B }

  // XEA32: the sequencer reads through the word-access unit, one registered
  // request at a time; its outcome is registered before the word completes.
  val eaEncodingValid = f7 === 0.U && !a(31, 8).orR && a(7, 6) =/= 3.U
  val eaEncodingOk = RegNext(eaEncodingValid)
  val eaDone = RegInit(false.B)
  val eaHit = Reg(Bool())
  val eaFault = Reg(Bool())
  val eaOffset = Reg(UInt(32.W))
  val eaBad = Reg(UInt(32.W))
  val eaInfoNext = Reg(UInt(32.W))
  val eaRequest = RegInit(false.B)
  val eaAddress = Reg(UInt(32.W))
  val eaBeatDone = eaRequest && access.io.done
  val eaFinish = Wire(Bool())
  val ea = Ea32(xValid && X.ea && eaEncodingValid && !xError && !eaDone && xPC(1, 0) === 0.U && live,
    eaFinish, a, b, eaConfigured, eaRegisters, contextOk,
    segmentBases(1), segmentLimits(1), segmentRights(1)(2), guestITlb, guestITlbValid,
    eaBeatDone, access.io.rdata, access.io.error)
  eaFinish := ea.done
  eaRequest := ea.request && !eaBeatDone
  eaAddress := ea.address
  when(xValid && X.ea && (!eaEncodingValid || xError || xPC(1, 0) =/= 0.U)) { eaDone := true.B }
  when(ea.done) {
    eaDone := true.B
    eaHit := ea.hit
    eaFault := ea.fault
    eaOffset := ea.offset
    eaBad := ea.badAddress
    eaInfoNext := ea.info
  }

  // Word-access sequence for a device word or an access spanning two words.
  // A spanning store first reads both words, so nothing is written unless
  // both are backed; then it writes them.
  val slowStep = RegInit(0.U(2.W))
  val slowDone = RegInit(false.B)
  val slowError = Reg(Bool())
  val slowLow = Reg(UInt(32.W))
  val slowValue = Reg(UInt(32.W))
  val slowNeeded = Wire(Bool())
  val slowRun = xValid && slowNeeded && !slowDone && live
  val slowHigh = xSplit && slowStep(0)
  val slowWrite = store && (!xSplit || slowStep(1))
  access.io.valid := Mux(X.ea, eaRequest && xValid, slowRun)
  access.io.addr := Mux(X.ea, eaAddress,
    Cat(xAddr(31, 12), Mux(slowHigh, xAddr(11, 2) + 1.U, xAddr(11, 2)), 0.U(2.W)))
  access.io.wstrb := Mux(X.ea || !slowWrite, 0.U, Mux(slowHigh, strobes(7, 4), strobes(3, 0)))
  access.io.wdata := Mux(slowHigh, storeData(63, 32), storeData(31, 0))
  access.io.d_hit := io.d_hit
  access.io.d_way := io.d_way
  access.io.d_word := io.d_word
  access.io.d_error := io.d_error
  access.io.busy := io.busy
  access.io.quick := X.ea
  access.io.hold := sValid
  access.io.dev_ready := io.dev_ready
  access.io.dev_rdata := io.dev_rdata
  access.io.dev_error := io.dev_error
  io.dev_valid := access.io.dev_valid
  io.dev_addr := access.io.d_addr
  io.dev_wdata := access.io.wdata
  io.dev_wstrb := access.io.wstrb
  when(slowRun && access.io.done) {
    val last = !xSplit || slowStep === Mux(store, 3.U, 1.U)
    when(access.io.error) {
      slowDone := true.B
      slowError := true.B
    }.elsewhen(last) {
      slowDone := true.B
      slowError := false.B
      slowValue := extend((Cat(access.io.rdata, Mux(xSplit, slowLow, access.io.rdata)) >>
        Cat(xAddr(1, 0), 0.U(3.W)))(31, 0), X)
    }.otherwise {
      slowStep := slowStep + 1.U
      when(slowStep === 0.U) { slowLow := access.io.rdata }
    }
  }
  val load = Mux(slowDone, slowValue, xLoad)

  val xjtReturn = X.xjt && xWord(20)
  val tickExpired = tick < xWord(31, 20)

  val done = WireDefault(true.B)
  // A fault that depends on operand values or state. System words register
  // it in their first X clock and act in the second.
  val dynamic = WireDefault(false.B)
  val writeRD = WireDefault(false.B)
  val writeIndex = WireDefault(rd)
  val writeBank = WireDefault(xBank)
  // Value of the rarer words; the common units are selected by xUnit.
  val other = WireDefault(0.U(32.W))
  // `jump`: the next word is at `target`, not PC+4. `transfer`: that target
  // came from operands and its alignment is checked. `forced`: fetch starts
  // again behind this word whatever was predicted.
  val jump = WireDefault(false.B)
  val transfer = WireDefault(false.B)
  val forced = WireDefault(false.B)
  val target = WireDefault(xTarget)
  val conditional = WireDefault(false.B)
  val taken = WireDefault(false.B)
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

  switch(X.opcode) {
    is("h37".U, "h17".U) { writeRD := true.B }
    is("h6f".U, "h67".U) { writeRD := true.B; jump := true.B; transfer := true.B }
    is("h63".U) {
      conditional := true.B
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
      when(X.muldiv) {
        done := mStarted && (md.io.done || mdDone)
        other := md.io.result
      }
    }
    is("h03".U, "h23".U) { writeRD := !store }
    is("h0f".U) { forced := f3 === 1.U } // FENCE.I: fetch again after the cache is emptied.
    is("h0b".U) {
      when(f3 < 4.U) {
        writeRD := true.B
        statusWrite := !f7(6); statusCommit := f7(3)
      }.otherwise {
        switch(f3) {
          is(4.U) { flagsLoad := true.B }
          is(5.U) { writeRD := true.B; other := (a & "hfffff72a".U) | flagsNow }
          is(6.U) { writeRD := true.B; other := a; flagsCommit := true.B }
          is(7.U) { writeRD := true.B; other := xCondition.asUInt }
        }
      }
    }
    is("h1b".U) { writeRD := true.B; statusWrite := true.B; statusCommit := true.B }
    is("h3b".U) {
      writeRD := true.B
      statusWrite := b(4, 0) =/= 0.U; statusCommit := true.B
      statusValue := shiftStatus
    }
    is("h2b".U) {
      when(f3 <= 5.U) { done := !l2.sweeping }
      // Words that change what A reads restart fetch behind them.
      forced := f3 =/= 0.U && f3 =/= 7.U
      switch(f3) {
        is(0.U) { writeRD := true.B; other := Mux(xTlbHit, xAddr, "hffffffff".U) }
        is(1.U) {
          dynamic := b(11, 3).orR || Mux(f7(0), b(1, 0).orR, b(2))
          tlbFill := true.B
        }
        is(2.U) { tlbInvalidate := true.B }
        is(3.U) { tlbFlush := true.B }
        is(4.U) {
          dynamic := a(11, 0) =/= 0.U || b(11, 0) =/= 0.U || a > b
          tlbRegion := true.B
        }
        is(5.U) {
          dynamic := a(31, 6) =/= 0.U
          contextWrite := true.B
        }
        is(6.U) { segmentWrite := true.B }
        is(7.U) {
          when(f7 === 0.U) {
            dynamic := a(1, 0) =/= 0.U
            vectorWrite := true.B
          }.elsewhen(f7 === 1.U) {
            writeRD := true.B
            other := MuxLookup(rs1, 0.U(32.W))((0 until 4).map(n => n.U -> serviceRecord(n)) ++ Seq(
              4.U -> guestControl, 5.U -> guestIdentity, 6.U -> serviceVector, 7.U -> serviceActive.asUInt))
          }.elsewhen(f7 === 2.U) {
            dynamic := !serviceActive
            jump := true.B; transfer := true.B; forced := true.B
            target := Mux(rs1 === 0.U, serviceRecord(0), a)
            serviceReturn := true.B
          }
        }
      }
    }
    is("h5b".U, "h07".U, "h27".U, "h47".U, "h77".U) {
      // Register write and status commit apply only when the access succeeds.
      writeRD := !store || X.guestUpdate
      storeCommitsFlags := X.guestOld && store && f7(5)
      when(X.guestUpdate) { writeIndex := rs1 }
    }
    is("h6b".U) {
      switch(f3) {
        // XJT rs1: continue at the native target filled for guest EIP rs1,
        // or at PC+4 when the table has none. XJT.R (imm 1) first pops the
        // return stack and uses its entry when the EIP matches.
        is(0.U) { jump := xRasMatch || xJtHit }
        is(1.U) {
          dynamic := b(1, 0) =/= 0.U
          done := !jt.sweeping
          jtFill := true.B; forced := true.B
        }
        is(2.U) { done := !jt.sweeping; jtInvalidate := true.B; forced := true.B }
        is(3.U) { done := !jt.sweeping; jtFlush := true.B; forced := true.B }
        is(4.U) {
          // XTICK n: spend n guest instructions, or leave bank 1 with reason
          // 0xfff and this word's own PC recorded.
          tickOp := true.B
          when(tickExpired) { jump := true.B; forced := true.B; target := runtimeReturn }
        }
        is(5.U) { tickSet := true.B }
        is(6.U) { writeRD := true.B; other := tick }
      }
    }
    is("h7b".U) {
      switch(f3) {
        is(0.U) {
          dynamic := !eaEncodingOk || eaFault
          done := eaDone
          writeRD := eaHit
          other := eaOffset
        }
        is(1.U) { writeRD := true.B; other := eaInfo }
        is(2.U) {
          dynamic := a(1, 0) =/= 0.U || a > "hffffffe0".U
          eaConfigure := true.B
        }
        // XJcc's rd bits are a B-immediate, not a register write.
        is(3.U) { conditional := true.B; taken := xCondition }
        is(4.U) {
          enterBank := true.B; jump := true.B; forced := true.B; transfer := true.B; target := a
        }
        is(5.U) { leaveBank := true.B; jump := true.B; forced := true.B; target := runtimeReturn }
        is(6.U) {
          writeRD := true.B
          other := Mux(f7 === 1.U, MuxLookup(rs1, 0.U)(Seq(
            0.U -> exitReason, 1.U -> guestReturn, 2.U -> "h4231000a".U)), a)
        }
        is(7.U) { writeRD := true.B; writeBank := true.B; other := a }
      }
    }
  }
  when(conditional) { jump := taken }
  val value = Mux(xDone, xResult, Mux1H(Seq(
    xUnit(0) -> guestResult, xUnit(1) -> shiftResult, xUnit(2) -> load,
    xUnit(3) -> addSub, xUnit(4) -> logic, xUnit(5) -> less.asUInt, xUnit(6) -> other)))
  xOut := value

  // System words take two clocks: operand checks are registered first.
  val checked = RegInit(false.B)
  val dynamicR = Reg(Bool())
  val misalignedR = Reg(Bool())
  when(xValid && xSystem && !checked) {
    checked := true.B
    dynamicR := dynamic
    misalignedR := target(1, 0) =/= 0.U
  }
  // Fault of the word in X: static (from M), or from values. A jump target
  // from operands must be word aligned.
  val eaWord = X.ea
  val valueFault = Mux(xSystem, dynamicR, eaWord && dynamic)
  val targetFault = !valueFault && Mux(xSystem, transfer && misalignedR, conditional && taken && xTargetBad)
  val fault = xStatic || valueFault || targetFault
  val cause = Mux(xStatic, xCause, Mux(targetFault, 0.U, Mux(eaWord && eaEncodingOk, 5.U, 2.U)))
  val faultValue = Mux(xStatic, xFaultValue,
    Mux(targetFault, target, Mux(eaWord && eaEncodingOk, eaBad, xWord)))
  val doneAll = done && (!xSystem || checked)

  // Outcome of the word in X, in priority order: its own fault; a mapping
  // refill (replay); a guest miss (software service, or stop when none can
  // run); a bus error; a replay; otherwise it commits.
  val doRefill = !fault && xRefill
  val guestMiss = !fault && !xRefill && X.guestMemory && xReason =/= 0.U
  val serviceBlocked = serviceVector === 0.U || serviceActive
  val serviceEnter = guestMiss && !serviceBlocked
  val memoryOk = !fault && !xRefill && !guestMiss && X.memory
  val replay = memoryOk && !xBusError && xReplay
  slowNeeded := memoryOk && !xBusError && !xReplay && xSlow
  val busFault = memoryOk && (xBusError || (slowDone && slowError))
  xLeaves := xValid && doneAll && (!slowNeeded || slowDone) && live
  val stop = fault || busFault || (guestMiss && serviceBlocked)
  val commit = xLeaves && !stop && !doRefill && !replay
  // Effects apply unless the word only enters the service.
  val effect = commit && !serviceEnter
  val nextPC = Mux(jump, target, xPC + 4.U)
  // Only JAL, JALR, branches and XJT jump without `forced`; their target was
  // compared in M.
  val mismatch = Mux(jump, !(xPred && xTargetOk), xPred)
  val slowUsed = X.memory && xSlow
  xRedirect := xLeaves && !stop &&
    (doRefill || replay || serviceEnter || forced || slowUsed || mismatch)
  flush := xRedirect
  flushTarget := Mux(doRefill || replay, xPC, Mux(serviceEnter, serviceVector, nextPC))

  front.io.flush := flush || redo
  front.io.flush_target := Mux(flush, flushTarget, redoTarget)
  front.io.stopped := stopped
  front.io.i_hit := io.i_hit
  front.io.i_word := io.i_word
  front.io.i_error := io.i_error
  io.i_addr := front.io.i_addr
  io.i_check := front.io.i_check
  io.i_flush := effect && X.is(0x0f) && xWord(12)
  // Training of the target table. A conditional branch keeps a two-bit
  // counter in its row, created when it is first taken; JALR keeps its last
  // target; a row found on a word that is not a branch is removed.
  val isJal = X.is(0x6f)
  val isJalr = X.is(0x67)
  val counted = Mux(taken, Mux(xCounter === 3.U, 3.U, xCounter + 1.U),
    Mux(xCounter === 0.U, 0.U, xCounter - 1.U))
  val offTarget = xPred && !xTargetOk
  val trainBranch = conditional && Mux(xHit, counted =/= xCounter || (taken && (xFixed || offTarget)), taken)
  val trainJalr = isJalr && (!xHit || xFixed || mismatch)
  val trainJal = isJal && offTarget
  val stray = xHit && !(isJal || isJalr || conditional)
  front.io.train := effect && (trainBranch || trainJalr || trainJal || stray)
  front.io.train_pc := xPC
  front.io.train_live := !stray
  front.io.train_row := Mux(conditional,
    Cat(xTarget(31, 2), Front.conditional.U(2.W), Mux(xHit, counted, 2.U(2.W))),
    Cat(xTarget(31, 2), Mux(isJalr, Front.indirect.U(2.W), Front.direct.U(2.W)), 3.U(2.W)))
  front.io.ras_valid := rasCount =/= 0.U
  front.io.ras_native := rasTopNative
  io.quiet := ((!aValid && !mValid && !xValid) || access.io.active) && !sValid

  // Table write ports, driven from X's operands.
  val l1Index = a(17, 12)
  val l1Mask = UIntToOH(l1Index, 64)
  val itlbIndex = a(13, 12)
  val itlbMask = UIntToOH(itlbIndex, 4)
  l1.io.write_enable := false.B
  l1.io.write_addr := l1Index
  l1.io.write_data := Cat(a(31, 18), b(31, 12), b(1, 0))
  l2.writeIndex := a(21, 12)
  l2.writePayload := Cat(a(31, 22), b(31, 12), b(1, 0))
  jt.writeIndex := a(9, 0)
  jt.writePayload := Cat(a(31, 10), b(31, 2))
  ras.io.write_enable := false.B
  ras.io.write_addr := rasTop + 1.U
  ras.io.write_data := Cat(b, xPC(31, 2) + X.immS(29, 0))

  when(xLeaves) {
    mStarted := false.B
    mdDone := false.B
    eaDone := false.B
    slowStep := 0.U
    slowDone := false.B
    checked := false.B
  }
  when(xLeaves && stop) {
    stopped := true.B
    stoppedPC := xPC
    stoppedCause := Mux(fault, cause, Mux(busFault, Mux(store, 7.U, 5.U), 12.U))
    stoppedValue := Mux(fault, faultValue, Mux(busFault, xAddr, xResult))
  }
  when(xLeaves && doRefill) {
    l1.io.write_enable := true.B
    l1.io.write_addr := xLinearPage(5, 0)
    l1.io.write_data := Cat(xLinearPage(19, 6), xRow)
    l1Valid := l1Valid | UIntToOH(xLinearPage(5, 0), 64)
  }
  when(commit && serviceEnter) {
    serviceRecord(0) := xPC
    serviceRecord(1) := xResult
    serviceRecord(2) := xWord
    serviceRecord(3) := xReason
    serviceActive := true.B
  }
  val tickLeave = tickOp && tickExpired
  when(effect) {
    when(eaConfigure) { eaRegisters := a; eaConfigured := true.B; eaInfo := "hffffffff".U }
    when(X.ea) { eaInfo := eaInfoNext }
    when(statusWrite) {
      pendingFlags := statusValue
      when(statusCommit) { guestFlags := statusValue }
    }
    when(flagsLoad) {
      guestFlags := a & "h8d5".U
      pendingFlags := a & "h8d5".U
    }
    when(flagsCommit || storeCommitsFlags) { guestFlags := pendingFlags }
    when(leaveBank || tickLeave) { pendingFlags := flagsNow }
    when(tickOp && !tickExpired) { tick := tick - xWord(31, 20) }
    when(tickSet) { tick := a }
    when(jtFill || jtInvalidate) { jt.write := true.B; jt.writeLive := jtFill }
    when(jtFlush) { jt.flush := true.B }
    when(X.pushRet) {
      ras.io.write_enable := true.B
      rasTop := rasTop + 1.U
      when(!rasCount(3)) { rasCount := rasCount + 1.U }
    }
    when(xjtReturn && rasCount =/= 0.U) {
      rasTop := rasTop - 1.U
      rasCount := rasCount - 1.U
    }
    // A dead translation must not be reachable through the return stack.
    when(jtFlush || jtInvalidate) { rasCount := 0.U }
    val flushTables = WireDefault(tlbFlush || tlbRegion)
    when(tlbFill) {
      // The native aperture is checked when a mapping is published, so no
      // range check sits in the hit path.
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
    when(enterBank) {
      bank := true.B
      runtimeReturn := xPC + 4.U
    }
    when(leaveBank || tickLeave) {
      bank := false.B
      guestReturn := Mux(tickOp, xPC, xPC + 4.U)
      exitReason := Mux(tickOp, "hfff".U, xWord(31, 20))
    }
  }

  // ---- W: register file and data cache writes ----
  wWrite := effect && writeRD && writeIndex =/= 0.U
  wRD := Cat(writeBank, writeIndex)
  wValue := value
  rf.foreach { ram =>
    ram.io.write_enable := wWrite
    ram.io.write_addr := wRD
    ram.io.write_data := wValue
  }
  sValid := effect && X.memory && store && !xSlow
  sAddr := xAddr(31, 2)
  sWay := xWay
  sData := storeData(31, 0)
  sStrb := strobes(3, 0)
  pValid := sValid
  pAddr := sAddr
  io.s_valid := sValid || access.io.s_valid
  io.s_slot := Mux(access.io.s_valid, d.slot(access.io.addr), sAddr(d.slotBits - 1, 0))
  io.s_way := Mux(access.io.s_valid, access.io.s_way, sWay)
  io.s_data := Mux(access.io.s_valid, access.io.wdata, sData)
  io.s_strb := Mux(access.io.s_valid, access.io.wstrb, sStrb)

  // Words behind a redirect or a stop never reach X.
  when(xRedirect || (xLeaves && stop) || stopped) {
    aValid := false.B
    mValid := false.B
    xValid := false.B
  }

  // Retirement and the stop report, registered. The report follows the last
  // retirement by a clock.
  val rValid = RegNext(commit, false.B)
  val rPC = RegNext(xPC)
  val rWord = RegNext(xWord)
  val rBank = RegNext(xBank)
  io.retire_valid := rValid
  io.retire_pc := rPC
  io.retire_word := rWord
  io.retire_bank := rBank
  io.retire_write := wWrite
  io.retire_rd := wRD
  io.retire_value := wValue
  io.trap := RegNext(stopped, false.B)
  io.trap_pc := stoppedPC
  io.trap_cause := stoppedCause
  io.trap_value := stoppedValue

  // 0 redirect after a wrong or missing prediction, 1 of those JALR, 2 of
  // those conditional, 3 replay for an absent line, 4 restart behind a word
  // (forced or word access), 5 A waits for an operand, 6 A waits behind
  // XEA32, 7 X holds XEA32, 8 X holds multiply/divide, 9 A empty, 10 decode
  // prediction, 11 fetch rewind, 12 X occupied, 13 XEA32 committed, 14
  // mapping refill, 15 replay of a load that passed a store.
  val plain = xRedirect && !(doRefill || replay || serviceEnter || forced || slowUsed)
  io.events := Cat(xLeaves && replay && xPassed, xLeaves && doRefill, effect && X.ea, xValid,
    front.io.events, !aValid && live, xValid && X.muldiv, xValid && X.ea,
    aValid && ((mValid && M.ea) || (xValid && X.ea)),
    aValid && ((A.needA && !okA) || (A.needB && !okB)),
    xRedirect && (forced || slowUsed) && !doRefill && !replay && !serviceEnter,
    xLeaves && replay && !xPassed, plain && conditional, plain && X.is(0x67), plain || redo)
}
