package r86chip

import chisel3._
import chisel3.util._

// Layout of one cache: 32-byte lines, 2^setBits sets, two ways. A tag row
// holds one field per way: (present, tag). Native bit 31 is never cached.
case class CacheShape(setBits: Int) {
  require(setBits >= 3 && setBits <= 15)
  val tagBits = 26 - setBits
  val field = tagBits + 1
  val rowBits = 2 * field
  val slotBits = setBits + 3
  def index(a: UInt): UInt = a(4 + setBits, 5)
  def tag(a: UInt): UInt = a(30, 5 + setBits)
  def slot(a: UInt): UInt = a(4 + setBits, 2)
  def way(row: UInt, w: Int): UInt = row(w * field + field - 1, w * field)
  def present(f: UInt): Bool = f(field - 1)
  def tagOf(f: UInt): UInt = f(tagBits - 1, 0)
  def holds(row: UInt, w: Int, t: UInt): Bool = present(way(row, w)) && tagOf(way(row, w)) === t
  def put(row: UInt, w: Bool, f: UInt): UInt = Mux(w, Cat(f, way(row, 0)), Cat(way(row, 1), f))
}

class CacheStoreIO(shape: CacheShape, lanes: Int) extends Bundle {
  // Read: present an address now; the outputs describe it on the next clock.
  val read_addr = Input(UInt(32.W))
  val addr = Output(UInt(32.W))
  val row = Output(UInt(shape.rowBits.W))
  val hit = Output(Bool())
  val way = Output(Bool())
  val word = Output(UInt(32.W))
  val words = Output(Vec(2, UInt(32.W)))
  val sweeping = Output(Bool())
  val flush = Input(Bool())
  val row_write = Input(Bool())
  val row_index = Input(UInt(shape.setBits.W))
  val row_value = Input(UInt(shape.rowBits.W))
  // One enable per byte lane (or one for the whole word when lanes is 1).
  val word_enable = Input(UInt(lanes.W))
  val word_way = Input(Bool())
  val word_slot = Input(UInt(shape.slotBits.W))
  val word_data = Input(UInt(32.W))
}

// Storage of one cache in block RAM: an epoch-tagged row of tags per set and
// `lanes` data RAMs per way. A row that is not live reads as two absent ways.
// Reads colliding with a write on the same edge are the user's business.
class CacheStore(shape: CacheShape, lanes: Int) extends Module {
  require(lanes == 1 || lanes == 4)
  val io = IO(new CacheStoreIO(shape, lanes))
  val tags = new EpochTable(clock, shape.setBits, shape.rowBits)
  val width = 32 / lanes
  val read = Reg(UInt(32.W))
  read := io.read_addr
  tags.readIndex := shape.index(io.read_addr)
  tags.flush := io.flush
  tags.write := io.row_write
  tags.writeLive := true.B
  tags.writeIndex := io.row_index
  tags.writePayload := io.row_value
  val words = (0 until 2).map { w =>
    Cat((0 until lanes).reverse.map { n =>
      val ram = Module(new BlockRam(shape.slotBits, width))
      ram.io.clk := clock
      ram.io.read_addr := shape.slot(io.read_addr)
      ram.io.write_enable := io.word_enable(n) && (io.word_way === (w == 1).B)
      ram.io.write_addr := io.word_slot
      ram.io.write_data := io.word_data(width * n + width - 1, width * n)
      ram.io.read_data
    })
  }
  val row = Mux(tags.live, tags.payload, 0.U)
  // Liveness and the tag comparison are decided side by side.
  val hits = (0 until 2).map(w => tags.live && shape.holds(tags.payload, w, shape.tag(read)))
  io.addr := read
  io.row := row
  io.hit := hits(0) || hits(1)
  io.way := hits(1)
  io.words := VecInit(words)
  io.word := Mux(hits(1), words(1), words(0))
  io.sweeping := tags.sweeping
}
