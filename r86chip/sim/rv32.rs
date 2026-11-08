//! Executable ISA gold standard for r86chip: use `Core::new` for the current
//! model; `Core::step` defines instruction decoding and architectural effects.
//! Change this ISS and its behavior tests before changing the Chisel core.
//! Functional only: multiply/divide here do not specify hardware latency.
//!
//! # Reading the executable specification
//!
//! The current ISA is RV32IM + Zifencei + Zba + private x86 arithmetic/status (0x0b),
//! guest translation/context (0x2b), guest memory (0x5b), and address-calculation
//! and bank-control (0x7b) instructions.
//! All instructions occupy one little-endian
//! 32-bit word; there are no compressed instructions or internal instruction
//! sequences. `step` fetches one word, reads its operands, checks its effects,
//! and either completes it or returns a precise native trap.
//!
//! `Core` documents the architectural state. `address` and the beginning/end of
//! `step` establish fault ordering. The opcode arms describe ordinary RV words
//! and each custom operation next to the code that implements them. The M helper
//! spells out multiplication/division corner cases. `load_elf` is host setup,
//! outside instruction execution. Behavioral examples are in `rv32_tests.rs`;
//! `b1_tests.rs` compares emitted RV code with the shared x86 interpreter.
//!
//! # Instructions added by r86chip
//!
//! RV32IM and FENCE.I retain their ordinary meanings. RV32 Zba uses the standard
//! OP encoding (0x33), funct7=0x10, funct3=2/4/6 for SH1ADD/SH2ADD/SH3ADD:
//! rd = (OLD rs1 << 1/2/3) + OLD rs2, modulo 2^32. Both inputs are read before
//! rd changes; x0 still discards writes. No flags, memory, bank or guest context
//! changes occur. Other funct3 values with funct7=0x10 are illegal, and RV64's
//! .uw forms are absent. These are register address calculations, not accesses:
//! an x86 segment/TLB check still belongs to a subsequent guest memory word.
//!
//! The context operations
//! below use custom-3 (opcode 0x7b); arithmetic/status operations follow them.
//! `rd`/`rs1` refer to the normal RV encoding fields; "bank1_reg" names a
//! register index in the parked bank, not an address or an x86 register number.
//!
//! | Instruction | Encoding after opcode | Allowed bank and effect |
//! |---|---|---|
//! | XENTER rs1 | funct3=4, rd=0, imm12=0 | Bank 0: save PC+4, select bank 1, jump to bank-0 rs1's value |
//! | XLEAVE reason | funct3=5, rd=rs1=0, imm12=reason | Bank 1: record unsigned reason and PC+4, restore bank 0 and its continuation |
//! | XREAD rd, bank1_reg | funct3=6, funct7=0, rs2=0, rs1=bank1_reg | Bank 0: copy a parked register into active rd |
//! | XINFO rd, selector | funct3=6, funct7=1, rs2=0, rs1=selector | Bank 0: read leave reason (0), saved bank-1 PC (1), or ISA identity (2) |
//! | XWRITE bank1_reg, rs1 | funct3=7, imm12=0, rd=bank1_reg | Bank 0: copy active rs1's value into a parked register |
//!
//! Both banks have a read-zero/write-discard x0. Invalid operand fields, bank
//! usage and XINFO selectors trap. XENTER checks four-byte target alignment
//! before changing banks; nested entry is illegal. A native trap does not itself
//! perform XLEAVE. The opcode arms below define exact ordering and fault values.
//!
//! XJcc cc, offset also uses custom-3: funct3=3, rs1=cc (0..15), rs2=0,
//! and the ordinary signed 13-bit B-immediate fields (bit 0 implicit zero).
//! The rd field is part of that displacement, NOT a destination register.
//! Test COMMITTED x86 status, then branch relative to this native PC when true;
//! otherwise continue at PC+4. Both flag values and all GPRs remain unchanged.
//! This word is available in either bank, including a one-bank core. It does
//! not dispatch a guest address, advance guest EIP/time, validate a translation
//! or switch banks. The emitter/runtime owns those checks around native links.
//! Reserved cc/rs2 encodings are illegal even when the condition would be false.
//! A taken halfword-aligned target traps before changing PC; an untaken target
//! is not checked. An aligned inaccessible target completes the branch and
//! faults on the next fetch. Address addition wraps like an ordinary RV branch.
//!
//! # Arithmetic and EFLAGS: custom-0 (opcode 0x0b)
//!
//! All operations below have the same meaning in either RF bank. There is
//! ONE shared guest status pair, not one pair per native register context.
//! Ordinary RV instructions preserve both values. Status uses the x86 positions
//! CF=0, PF=2, AF=4, ZF=6, SF=7, OF=11 (mask 0x8d5); other bits are always zero.
//! Both committed and pending status reset to zero. No lazy flag recipe exists.
//!
//! | Instruction | Encoding after opcode | Effect |
//! |---|---|---|
//! | XALU[.C] rd, rs1, rs2, lane, op | funct3=lane (0..3); funct7 described below | Compute/merge a result into rd; replace pending status; .C also publishes it |
//! | XMERGE rd, rs1, rs2, lane, high_source | funct3=lane (0..3); funct7=0x40 or 0x50 | Copy rs2's selected lane into the OLD rs1 container, then write rd; preserve both status values |
//! | XFLOAD rs1 | funct3=4, rd=0, imm12=0 | Set both status values to rs1 & 0x8d5; discard any old pending flags |
//! | XFGET rd, rs1 | funct3=5, imm12=0 | rd = (rs1 & !0x8d5) OR committed status; no flag changes |
//! | XFCOMMIT rd, rs1 | funct3=6, imm12=0 | Copy rs1 to rd AND publish pending status atomically as one native instruction |
//! | XCOND rd, cc | funct3=7, imm12=0, rs1=cc (0..15) | Set rd to 0/1 from committed status and the x86 condition code |
//!
//! XALU lane: 0=32 bits, 1=low word, 2=low byte, 3=high byte (bits 15:8).
//! funct7 bits 2:0 select ADD/OR/ADC/SBB/AND/SUB/XOR/NEG respectively.
//! Bit 3 selects .C: commit the new status with the result in this native word.
//! Bit 4 selects rs2's high byte; it is legal only for byte lanes. Bit 5
//! preserves incoming CF and is legal only for ADD/SUB (INC/DEC use rhs=1).
//! Bit 6 is reserved zero. NEG requires rs2=0 and bits 4/5 zero;
//! it negates rs1's selected lane. Invalid forms trap before changing any state.
//!
//! XMERGE reuses the lane numbering and rs2 high-byte selector above, with
//! funct7 bit 6 set. Bits 3:0 and 5 must be zero; bit 4 selects source bits 15:8
//! and is legal only for byte lanes. Without it the source is the low 8/16/32
//! bits of rs2. Replace only rs1's destination lane: in particular AH <- BL and
//! AL <- AH read both OLD containers even when all operands alias. Lane 0 copies
//! all of rs2. No third RF read is needed: preserved bits come from rs1, never
//! implicitly from rd. rd=x0 discards the data but still checks the encoding.
//! Neither committed nor pending flags change, including between a staged
//! XALU and XFCOMMIT. XMERGE performs no sign extension or memory operation.
//!
//! Operands are read before any write, including rd aliases. XALU merges its
//! narrow result into the ORIGINAL rs1 value, so no third RF read is needed.
//! rd=x0 discards the result but still computes pending status: CMP uses SUB
//! and TEST uses AND this way. ADC/SBB read COMMITTED CF, not a previous pending
//! result. Each XALU replaces pending flags from the same committed input; an
//! ordinary RV calculation or XFGET does not publish them. XFCOMMIT leaves the
//! pending value equal to the newly committed value; it has no validity token.
//! .C lets a fault-free register operation publish result/status together without
//! an extra commit word. It must not precede a still-fallible guest operation.
//!
//! ADD/ADC/SUB/SBB/NEG define all six status bits. CF means carry for addition
//! and BORROW for subtraction. PF is even parity of the low result byte, AF the
//! carry/borrow across bit 3; ZF/SF/OF use the selected width before lane merging.
//! Both operands are narrowed before computing flags, including sign-extended
//! immediates. Logic sets PF/ZF/SF and clears CF/OF; undefined AF is cleared to
//! match the shared interpreter's policy, not an x86 guarantee. Preserved CF
//! comes from committed status. MOV/LEA/NOT use RV/Zba/XMERGE operations and
//! touch no flags.
//! Shifts/rotates remain software operations. Guest memory uses the separate
//! opcode family below; arithmetic words never hide a memory operation.
//!
//! A fault after XALU leaves committed status unchanged. The translator must
//! keep its result in a TEMPORARY until the guest instruction cannot fail;
//! XFCOMMIT can then copy that result into a live guest GPR with the flags. It
//! does not commit a store, EIP, time or multiple guest GPRs. These still require
//! the software boundary protocol. Native traps stop in place; they do not undo
//! earlier native register writes. XLEAVE discards pending flags by replacing
//! them with committed status, but performs no GPR rollback or flag export.
//! XENTER preserves both status values. Native banking is not a transaction.
//!
//! The separate JIT experiment's ABI retains full EFLAGS in Frame.flags. Entry loads it
//! into ordinary bank-1 x16 and uses XFLOAD; exit uses XFGET x16,x16 to merge the
//! committed arithmetic bits with preserved software-owned bits, then stores it.
//! `jit/runtime.rs` uses Cpu::effective_eflags()/Cpu::jit_commit_flags() across
//! this boundary. IF/DF/TF/IOPL/VM and privileged updates remain software-owned.
//! Status is global to this hart: a helper using these custom operations must
//! explicitly save/restore it if another guest computation is suspended.
//!
//! The compiler still qualifies fetches/events and emits register-only blocks.
//! The translation probe below can qualify a subsequent ordinary RAM access;
//! pending flags alone do not validate a future faultable memory path.
//! `r86chip/design/EFLAGS.md` records v0/expert provenance and remaining work;
//! the instruction contracts here supersede the retired v0 encodings/model.
//!
//! # Software-filled guest TLB: custom-1 (opcode 0x2b)
//!
//! Directly decoded words manage separate DATA and FETCH tables, shared by both
//! RF banks. Both cache 4 KiB mappings and reset invalid. Data has 16 direct-mapped
//! entries (linear index 15:12, tag 31:16); fetch has four (index 13:12, tag 31:14).
//! Fetch intent selects the fetch table; all other intents select data. There is
//! no search of the other table on a miss. Software qualifies a mapping
//! for the CURRENT guest access context before filling it. Entries contain no
//! CR3, CPL, segment, PTE, guest-physical identity or page-walker state. The
//! separately published context below invalidates entries when it changes.
//!
//! | Instruction | Encoding after opcode | Effect |
//! |---|---|---|
//! | XTLOOK rd, rs1, mode | funct3=0, imm12=mode | Probe guest linear address rs1; rd=native byte address on hit, 0xffffffff on service |
//! | XTFILL.D rs1, rs2 | funct3=1, rd=0, funct7=0 | Replace only rs1's DATA slot with its guest tag and rs2's native page/RW proof |
//! | XTFILL.I rs1, rs2 | funct3=1, rd=0, funct7=1 | Replace only rs1's FETCH slot with its guest tag and rs2's native page/X proof |
//! | XTINVAL rs1 | funct3=2, rd=0, imm12=0 | Invalidate rs1's indexed slot in BOTH tables, including their colliding pages |
//! | XTFLUSH | funct3=3, rd=rs1=0, imm12=0 | Invalidate every entry; no native instruction-cache effect |
//! | XTREGION rs1, rs2 | funct3=4, rd=0, funct7=0 | Set native guest-RAM window [rs1,rs2), then invalidate every entry |
//!
//! XTLOOK mode bits 1:0 select size: 0=byte, 1=word, 2=dword; 3 is illegal.
//! Bits 3:2 select intent: 0=read, 1=write, 2=instruction fetch, 3=read-modify-write.
//! All upper mode bits must be zero. Both XTFILL forms take any guest address in rs1
//! (its low 12 bits are ignored). rs2 bits 31:12 hold the NATIVE RAM page base;
//! data fills accept only bits 0/1 for read/write, and fetch fills accept only
//! bit 2 for execute. Every other low bit is reserved zero (including X in a
//! data fill and R/W in a fetch fill). Invalid metadata or funct7 > 1 traps
//! without changing EITHER table. Zero
//! rights are legal and can never hit. Rights are software proofs, not raw x86
//! PTE bits: e.g. RMW requires BOTH read and write, fetch requires the fetch bit.
//! Publication replaces rights; it never merges them with an older proof.
//! To establish data and execute proofs for the same page, issue both forms.
//! They have independent lifetimes: a data fill cannot grant/revoke execute,
//! nor can an execute fill grant/revoke data access. Software invalidation must
//! retire all affected proofs before their source assumptions stop being valid.
//!
//! A probe returns the service sentinel on an invalid/tag-mismatching entry,
//! insufficient rights, or an address not naturally aligned for its width.
//! Thus no probe certifies a page-split/wrapping access. No successful pointer
//! can equal the service sentinel because of the window rule below.
//! There is no restriction to positive/signed native addresses and
//! native page zero is valid if the configured window includes it.
//!
//! XTREGION's BASE and LIMIT are native byte addresses, both 4 KiB aligned,
//! BASE <= LIMIT. Invalid values cause IllegalInstruction and preserve the old
//! window/table. Equal bounds disable fills. Reset bounds are both zero, so no
//! fill can become usable until firmware configures a window. LIMIT is exclusive
//! and must fit u32; consequently the last native 4 KiB page is not cacheable.
//! This avoids an overflowing endpoint or an ambiguous successful -1 result.
//! A well-encoded XTFILL whose page lies outside [BASE,LIMIT) completes by
//! INVALIDATING its selected table's slot; the other table is untouched. It
//! cannot publish a pointer into firmware/metadata.
//! In-range pages are wholly contained because all bounds are page-aligned.
//! Region changes always flush BOTH tables, even for identical bounds. Context
//! changes and XTFLUSH also clear both. XTINVAL conservatively clears the two
//! indexed slots, so an invalidation can evict unrelated colliders in either.
//!
//! Firmware and r86's interpreter/JIT/device state use native physical addresses.
//! Guest paging translates x86 linear -> x86 physical; firmware then resolves
//! that physical address to eligible RAM backing (or emulated ROM/MMIO). A fill
//! contains the final native backing page, NEVER a guest PTE copied unchanged.
//! The TLB caches the composition, x86 linear -> native guest-RAM address.
//! Its window must exclude firmware code/stack/heap, the native code cache and
//! host-device registers. It is configured by trusted firmware at initialization.
//! All native RV code remains trusted: XTREGION/XTFILL have no privilege levels,
//! and ordinary RV loads/stores bypass the window. This protects the guest lookup
//! path, NOT against arbitrary/miscompiled native code; RF banks are not domains.
//!
//! Lookup performs NO data/fetch bus access, guest page walk, A/D write, fault
//! delivery, EIP advance, or flag update. A service result is a completed native
//! instruction, NOT a native trap or an x86 #PF. rd=x0 discards the probe result
//! but still checks the encoding. Only the subsequent RV load/store touches RAM
//! and can raise a native bus fault. Bad mapping metadata is a firmware bug;
//! native access faults must not be reinterpreted as guest page faults. Reserved
//! instruction fields trap before changing the table, either flag value or GPRs.
//!
//! Typical aligned load (x31 holds -1, segment checks have already succeeded):
//! `XTLOOK t0, linear, read32; BEQ t0,x31,service; LW value,0(t0)`.
//! Service runs ordinary RV code: use the shared x86 walker to report the real
//! guest exception, or qualify/fill ordinary RAM and retry, or handle MMIO/other
//! slow cases exactly once. It must never endlessly refill an ineligible access.
//! RMW probes write intent BEFORE loading; leave guest GPRs/status uncommitted
//! until the store completes. A pointer is invocation-local and cannot survive
//! a context change, software service, invalidation, remap, or writer handoff.
//!
//! Firmware obligations (the table cannot derive these proofs from RV operands):
//! - Qualify segmentation and the guest access context, including CPL/CR0.WP,
//!   before using a hit. XTLOOK alone does not check segments/context; XLD/XSW
//!   below consume published segment/context proofs. Flush on changed assumptions (CR3/control/privilege
//!   changes, backing/A20/overlay changes or guest replacement). XTINVAL can
//!   implement the cache part of guest INVLPG; a full flush is always allowed.
//! - Configure a disjoint native guest-RAM window, then fill only complete
//!   ordinary-RAM pages within it with stable native backing. Complete
//!   required page-table A/D effects first. Never speculate a write walk merely
//!   to fill the W bit; a read proof does not authorize writes. MMIO/partial RAM,
//!   unaligned accesses and complex cases use software. This is NOT Sv32.
//! - Deny W for watched guest-physical code or page-table pages. On first watch
//!   or changed write eligibility, stop guest writers and flush the WHOLE table
//!   before publication so every linear alias loses its old write permission.
//!   Refill consults the authoritative physical watch map. Native stores, page
//!   walkers, emulated DMA and other physical writers must use that protocol.
//!   No hardware snoop or SMC detector is implemented by this table.
//! - Never reuse a returned pointer across guest instruction/event boundaries
//!   without renewed qualification. Bank switches and FENCE.I do NOT flush the
//!   table: native banks are register contexts, not guest address spaces.
//!
//! The full PC interpreter's `r86chip-native` build uses XTLOOK through
//! `interp/native.rs`; the ordinary build keeps software lookup. Its batch
//! lease and software invalidation hooks enforce the proof lifetime above.
//! Selected reads and original instruction-byte fetches consume the corresponding
//! table through XLD, described below; software execute proofs supply X rights.
//! No decoded x86 bytes are stored.
//!
//! # Guest memory: different instructions, different views
//!
//! Opcode 0x5b accesses the current GUEST context; ordinary LW/SW still use
//! native addresses, including inside a guest service. No bank change occurs.
//! These instructions use a guest segment OFFSET in rs1 (no displacement).
//! Software has already applied x86 address-size wrapping to that offset.
//! Six segment proofs hold base, inclusive limit and R/W/X access rights for
//! ES=0, CS=1, SS=2, DS=3, FS=4, GS=5. They are qualified snapshots of software's
//! x86 segment state, not an independently loaded x86 descriptor table.
//!
//! | Instruction | Encoding after opcode 0x5b | Effect on a hit |
//! |---|---|---|
//! | XLD rd, rs1, segment, width | funct3=0/1/2 (8/16/32), rs2=0 | Zero-extended guest load into rd; preserves flags |
//! | XLD.R rd, rs1, rs2, width | funct3=0/1/2, funct7 bit 6=1, bits 2:0=0 | Same load, with segment number in the OLD rs2 value |
//! | XSW rs1, rs2, segment, width | funct3=4/5/6 (8/16/32), rd=0 | Store low rs2 bits; preserves flags |
//!
//! funct7 bits 2:0 select segment (0..5). Bit 3 is load-with-write-intent:
//! require R+W before reading an RMW operand. Bit 4 selects instruction fetch
//! instead of data read, legal only for a load through CS and without bit 3.
//! Bit 5 selects .C on stores: publish pending arithmetic flags WITH successful
//! store completion. On a CS/fetch load it selects .U: read unaligned original
//! instruction bytes wholly within ONE linear page. It is illegal on a data
//! load. .U has no cursor or decoded cache and never advances guest EIP. Width
//! still selects exactly 1/2/4 bytes, with the same segment limit and X proof.
//! The RTL may read one or two aligned native bus words and assemble the bytes;
//! software certifies the whole backing page as ordinary RAM, so extra lanes
//! have no guest fault or device effect. The destination changes only after all
//! required bus beats succeed, and the operation retires once. There is no
//! page walk, software execution, or internal uop sequence inside this word.
//! Bit 6 selects a register segment for loads:
//! rs2's FULL value must be 0..5, and funct7 bits 2:0 must be zero. This avoids a
//! software six-way dispatch when the x86 decoder supplies a dynamic segment.
//! Both address and segment operands are read before rd changes; rd may alias
//! either input. RMW and fetch intents work with this form too (fetch still
//! requires the selected segment to be CS). Stores cannot use bit 6: their rs2
//! already supplies data. Unsupported widths/segments and
//! incompatible operand fields are illegal before any guest checks or effects.
//! Loads into x0 still perform the access/checks; .C still commits when data=0.
//! Byte/word loads do not merge an existing x86 GPR; handlers stage/merge it.
//!
//! Additional custom-1 (0x2b) context/service operations:
//!
//! | Instruction | Encoding after opcode | Effect |
//! |---|---|---|
//! | XGCTX rs1, rs2 | funct3=5, rd=0, funct7=0 | Install access control rs1 and software context identity rs2 |
//! | XGSEG segment, rs1, rs2, rights | funct3=6, rd=segment (0..5), funct7=rights (0..7) | Install segment base rs1, limit rs2, independent R/W/X proof bits |
//! | XGSVEC rs1 | funct3=7, rd=0, rs2=0, funct7=0 | Configure aligned native service entry; zero disables it |
//! | XGGET rd, selector | funct3=7, rs2=0, funct7=1, rs1=selector | Read service/context metadata below |
//! | XGRET rs1 | funct3=7, rd=rs2=0, funct7=2 | Leave active service; rs1=x0 retries saved native PC, otherwise jump to rs1 value |
//!
//! XGCTX control bits: PE=0, PG=1, WP=2, VM=3, CPL=5:4; other bits reserved zero.
//! First installation or a CHANGED control/identity clears the TLB and ALL six
//! segment proofs. Identical installation preserves them. Reset is unconfigured.
//! Software assigns a new identity when CR3, relevant CR4 state, A20, backing,
//! debug/alignment-check conditions or other assumptions change. The numeric
//! identity is a coherence token, NOT guest CR3 or a hardware ASID allocator.
//! Reusing it without retiring old proofs is a firmware error. XGSEG updates a
//! segment proof in the current context; the linear-address TLB remains valid.
//! Segment permission proofs must include present/type/privilege checks. Supply
//! zero rights for unsupported expand-down/debug/alignment-check cases so they
//! cannot hit. Segment and paging bits are separate: protected paging-off RAM
//! can hit with software-filled identity/backing mappings too.
//!
//! The initial fast path requires configured PE=1, VM=0. Real/v8086 mode always
//! requests software regardless of cached entries. XLD/XSW checks in order:
//! context -> segment rights and widened offset+size-1 <= limit -> linear
//! natural alignment (or same-page span for fetch .U) -> TLB tag/rights.
//! Linear = (base+offset) modulo 2^32. Both forms reject page splits BEFORE
//! reading any data. A .U miss leaves byte-by-byte page/segment fault ordering
//! to the existing software fetcher, rather than inventing a fetch exception.
//! Required rights are R for a
//! data load, W for a store, X for fetch, R|W for an RMW load, for BOTH segment
//! and page proofs. Rejection is service, never a fabricated x86 #GP/#SS/#PF.
//! Software determines exact guest fault priority/code and required A/D effects.
//!
//! A hit accesses the TLB's native RAM pointer. All native bus checks complete
//! before publishing a load destination or .C flags. A native bus error means
//! broken firmware/backing, returns LoadAccess/StoreAccess with the NATIVE bad
//! address, and is not retried as a guest miss. A failing .U reports its original
//! effective native byte address even when only the second bus beat fails.
//! There is no hardware page walk,
//! MMIO access, hidden instruction stream, guest EIP/time advance or general
//! multi-register commit. The initial hardware may take extra bus cycles; the
//! functional ISS makes no CPI promise.
//!
//! A service completes the native word as a redirect to XGSVEC, with no data
//! access, GPR/flag modification, or bank switch. Record native retry PC, original
//! guest offset, instruction word and reason: 1=context, 2=segment, 3=alignment/page span,
//! 4=TLB. XGGET selectors 0..7 read those four values, current control, identity,
//! service vector, and active-service (0/1), respectively. All reset to zero.
//! With no vector or an already-active service, stop with native GuestService
//! (debug cause 12, value=guest offset), preserving any older service record.
//! This diagnoses missing/nested service handling, not a guest exception.
//!
//! The handler executes ordinary RV code on the SAME RF bank. It must preserve
//! native live registers before calling Rust. Qualified RAM may be filled then
//! XGRET x0 retries the original native word; a completed software guest access
//! or delivered exception instead resumes an explicit interpreter continuation.
//! XGRET requires active service and validates four-byte target alignment before
//! clearing active. An aligned inaccessible target faults on its next fetch.
//! Service records remain readable after return. Software preserves/discards
//! pending flags explicitly; neither a miss nor XGRET rolls back previous RV
//! words. Native side effects must already represent a recoverable guest boundary.
//!
//! The single PC interpreter's native build uses XLD and an in-place miss
//! continuation in `interp/native.rs`. Its byte fetcher uses CS/X; immediate
//! and displacement fetches use .U for words/dwords. The same leaf also serves
//! the existing fast ModR/M address decoder; it does not require a software
//! cursor refreshed by a previous byte miss. Software fetch-cache invalidation
//! also retires hardware proofs.
//! Each hit reads current guest bytes; no decoded instructions are retained.
//! Rejected accesses continue in the shared software helpers. Data fills and
//! fetch fills publish independently; firmware does not merge their rights.
//! Its stores retain the shared software
//! write hooks; this consumer does not issue XSW. Ordinary trusted RV accesses
//! can still reach all native RAM; guest instructions are not a
//! sandbox for arbitrary native code. Guest mode changes must publish XGCTX and
//! segment proofs BEFORE the next guest access. The hardware cannot observe a
//! write to a Rust Cpu field unless firmware performs this synchronization.
//!
//! # Bounded address calculation: custom-3 (opcode 0x7b)
//!
//! These three words calculate a 32-bit x86 memory operand's OFFSET and default
//! segment, without executing an x86 opcode. The caller fetched ModR/M and selected
//! 32-bit addressing. Software still applies segment overrides and performs
//! the operand access, exceptions, EIP/time accounting and architectural commit.
//! `ea_tests.rs` exercises the contract; Chisel's bounded `Ea32` data sequencer
//! implements it in the existing execute slot. Functional steps are not cycles.
//!
//! | Instruction | Encoding after opcode | Effect |
//! |---|---|---|
//! | XEAREGS rs1 | funct3=2, rd=rs2=funct7=0 | Set native address of eight little-endian u32 GPRs in EAX..EDI order; reset EAINFO to -1 |
//! | XEA32 rd, rs1, rs2 | funct3=0, funct7=0 | rs1=raw ModR/M byte, rs2=CS offset immediately after it; on success rd=offset, EAINFO=(consumed_bytes<<1)|default_SS |
//! | XEAINFO rd | funct3=1, rs1=rs2=funct7=0 | Read EAINFO; -1 means software continuation, otherwise length=0..5 and SS=0/1 |
//!
//! XEAREGS accepts aligned native bases <= 0xffffffe0; it performs no read.
//! Reset leaves the base UNCONFIGURED, not a pointer to native page zero. It
//! refers to the interpreter's actual software register array, not a copied
//! bank or a Rust Cpu layout. Each XEA32 reads the required current array slots
//! with ordinary NATIVE addresses. Ordinary RV stores immediately affect them.
//! Firmware must republish this pointer when its Cpu moves; a batch lease does
//! so at entry. No memory ownership or native privilege is inferred from it.
//!
//! XEA32 rejects reserved fields, rs1>255 and ModR/M.mod=3 as illegal BEFORE
//! checking context or accessing data. Legal operations require a configured
//! array and published PE=1, VM=0; otherwise EAINFO=-1, rd unchanged. A needed
//! SIB and signed disp8 or wrapping disp32 are read from original GUEST code
//! bytes using CS/X proofs, never native addresses. All consumed code bytes
//! must fit one linear 4 KiB page and CS's inclusive limit; no page/segment wrap
//! is accelerated. The same software fetch/walker handles every rejection.
//! Zero consumed bytes require no CS/X proof. Guest data permissions are never
//! used for code. No guest operand memory is accessed here.
//!
//! Check/fetch required code bytes first (SIB, then displacement), then read
//! native base/index GPRs. ESP is never an index; absent base in mod=0/rm=5 or
//! SIB.base=5 contributes zero and selects DS. A present EBP or ESP base selects
//! SS even with a nonzero index; an EBP INDEX alone does not. Compute base +
//! (index << scale) + displacement modulo 2^32. Index scale is 0..3.
//! The software POP-memory helper with an ESP override does not use XEA32.
//!
//! At most three code-bus beats and two GPR reads are needed, all ordinary RAM.
//! This fixed operation has no instruction stream, decoder cache, page walk,
//! store or x86 execution loop. Failed qualification retires once with
//! EAINFO=-1 and unchanged rd, flags, GPR array and service record. Already read
//! code bytes have no device effects. Native bus faults instead trap at the
//! ORIGINAL failing read address and preserve both rd and previous EAINFO.
//! They are firmware/backing bugs, never guest misses. rd=x0 still checks/reads;
//! rd may alias either RV input. Success publishes rd and EAINFO together.
//! The scratch EAINFO is shared by RF banks; consume it in the same inline-asm
//! primitive, not across arbitrary Rust calls. No guest EIP is held in hardware.
//!
//! # Execution environment and boundaries
//!
//! One hart owns a flat, byte-addressed physical RAM. Native accesses do not
//! perform x86 segmentation, paging, MMIO or privilege checks: the RV firmware
//! implements those for its x86 guest. There are no native CSRs, trap vectors,
//! interrupt delivery, atomics or floating-point instructions in this model.
//! Unsupported instruction forms return IllegalInstruction, not a service or
//! an implicit software implementation. ECALL/EBREAK return to the harness.
//!
//! Both register banks execute the same RV instructions and share PC and RAM.
//! Bank 0 is used for the software runtime and bank 1 for generated code by
//! convention; they are not different privilege levels or concurrently running
//! CPUs. Banking preserves native registers, not a rollback copy of x86 state.
//! Completing an RV word does not by itself commit an entire x86 instruction.
//!
//! For example, bank 0 can execute `XWRITE x5, x10`, then `XENTER x11`:
//! the parked bank receives bank-0 x10 in its x5; x11 supplies the native entry
//! address from bank 0. At that entry, `ADDI x5, x5, 1` modifies bank-1 x5.
//! `XLEAVE 7` restores bank 0 and resumes just after XENTER. `XREAD x12, x5`
//! retrieves the modified parked value into bank-0 x12; bank-0 x5/x10/x11
//! retain their own previous values. There is no implicit transfer of x86 state.
//!
//! This is a functional reference, not a pipeline/cache timing model. In
//! particular, the small fetched-word table only makes FENCE.I publication
//! testable. Its capacity and collisions impose no hardware cache geometry.
//! The harness reports native instruction attempts separately from x86 work;
//! neither count is a hardware clock count.
use super::memory::Memory;

#[path = "tlb_stats.rs"]
pub mod tlb_stats;

/// Arithmetic status only; EFLAGS control/system/reserved bits belong to software.
const STATUS_MASK: u32 = 0x8d5;

#[cfg(test)]
#[path = "flags_tests.rs"]
mod flags_tests;

#[cfg(test)]
#[path = "tlb_tests.rs"]
mod tlb_tests;

#[cfg(test)]
#[path = "guest_memory_tests.rs"]
mod guest_memory_tests;

#[cfg(test)]
#[path = "ea_tests.rs"]
mod ea_tests;

#[cfg(test)]
#[path = "jit_isa_tests.rs"]
mod jit_isa_tests;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct GuestSegment {
    base: u32,
    limit: u32,
    rights: u32,
}

/// One software-qualified 4 KiB mapping. Store the full linear page in the ISS
/// for readability; the selected table's low VPN bits form the hardware index.
/// `None` is invalid. No entry payload is observable before a completed fill.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct GuestMapping {
    linear_page: u32,
    native_page: u32,
    rights: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
/// Native execution outcomes. The enum's Rust discriminants are not a wire ABI;
/// the RTL trap port maps these meanings to RISC-V cause numbers.
pub enum Cause {
    /// Current fetch PC or a taken control-transfer target is not 4-byte aligned.
    InstructionMisaligned,
    /// The aligned instruction word is not wholly inside native physical RAM.
    InstructionAccess,
    /// Unsupported opcode, reserved operands, or an operation illegal in this bank.
    IllegalInstruction,
    /// Halfword/word data address is misaligned; checked before its RAM range.
    LoadMisaligned,
    /// An aligned load is not wholly in RAM. Loads into x0 still perform checks.
    LoadAccess,
    /// Halfword/word store address is misaligned; no bytes are written.
    StoreMisaligned,
    /// An aligned store is not wholly in RAM; no partial store is permitted.
    StoreAccess,
    /// Exact ECALL word; firmware-to-harness stop, with PC left on ECALL.
    Ecall,
    /// Exact EBREAK word; firmware-to-harness stop, with PC left on EBREAK.
    Breakpoint,
    /// Guest-memory software service is unconfigured or nested. Not x86 #PF.
    GuestService,
    /// Harness budget expired between words. Not an architectural exception.
    StepLimit,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Trap {
    /// Faulting instruction address. For StepLimit, the next instruction to run.
    pub pc: u32,
    pub cause: Cause,
    /// Bad address for address faults, the fetched word for IllegalInstruction,
    /// guest offset for GuestService, and zero for ECALL, EBREAK or StepLimit.
    /// A bad branch target therefore
    /// appears in `value`, while `pc` still identifies the branch/jump itself.
    pub value: u32,
}

/// One native execution context plus a parked register bank.
///
/// Successful instructions update the active registers and PC; native faults
/// leave architectural destinations untouched. Fetch-cache entries and diagnostic
/// counters may change on a fault, since neither is a committed guest result.
/// The harness stops on a returned trap; the ISS does not vector, switch banks
/// or reinterpret a failing native instruction on its behalf.
pub struct Core {
    /// Active x0..x31. Instruction writes to x0 are discarded. Harness code may
    /// initialize GPRs directly, but must preserve x[0] == 0 in legal states.
    pub x: [u32; 32],
    /// Byte address of the next native instruction, never guest x86 CS:EIP.
    pub pc: u32,
    /// Words fetched and attempted by the native decoder, including the word
    /// that returns a trap. A fault before fetch does not increment this count.
    /// Thus a terminal EBREAK adds one attempt but no RTL retirement.
    pub host_steps: u64,
    /// Completed native words, including guest-service redirects but excluding
    /// native faults. Keep distinct from x86 retirement and instruction attempts.
    pub retired_steps: u64,
    /// Retired primitive observations for firmware coverage. Not architectural
    /// state/CSRs and not synthesized counters; RTL's testbench counts retire.
    pub xalu_ops: u64,
    pub tlb_lookups: u64,
    pub tlb_hits: u64,
    pub tlb_fills: u64,
    pub xld_ops: u64,
    pub xld_hits: u64,
    pub xea_ops: u64,
    pub xea_hits: u64,
    /// Optional host-only miss/replacement attribution. Encoding-valid attempts
    /// include native faults; unlike the counters above these are not retirements.
    pub tlb_stats: Option<Box<tlb_stats::TlbStats>>,
    /// Completed FENCE.I operations; diagnostic only, not a CSR.
    pub fences: u64,
    /// Successful bank-0-to-bank-1 entries, for checking that emitted code ran.
    pub bank_enters: u64,
    /// false = bank 0 active, true = bank 1 active. Not an ISA decode-mode bit.
    bank: bool,
    /// The other bank's x0..x31. Swapping arrays models selecting another RF
    /// bank; it does not prescribe a hardware register-copy loop or its latency.
    inactive: [u32; 32],
    /// Unsigned 12-bit reason from the most recent completed XLEAVE, initially 0.
    bank_reason: u32,
    /// Native PC immediately after the most recent XLEAVE, initially 0.
    bank_pc: u32,
    /// Bank 0's continuation after XENTER. Only one continuation exists;
    /// nested entry is illegal rather than overwriting the suspended runtime.
    return_pc: u32,
    /// Guest arithmetic status visible to XFGET/XCOND and carry consumers.
    /// Shared between native banks; no automatic frame load/export on a switch.
    flags: u32,
    /// XALU's proposed arithmetic status. Publish with XFCOMMIT or discard with
    /// XFLOAD/XLEAVE. Native faults preserve both values for diagnosis/recovery.
    pending_flags: u32,
    /// Software-filled data table; ordinary RV fetches/loads/stores never use
    /// either guest table. Native bank switches preserve both tables.
    guest_dtlb: [Option<GuestMapping>; 16],
    /// Execute-only translation capacity, not cached x86 bytes or decoded code.
    guest_itlb: [Option<GuestMapping>; 4],
    /// Native [base,limit) aperture for table fills, page aligned and nonwrapping.
    /// Initially empty. It constrains guest lookups, not trusted native accesses.
    guest_region: (u32, u32),
    /// Current software-published x86 access assumptions. No native RV mode.
    guest_context: Option<(u32, u32)>,
    guest_segments: [GuestSegment; 6],
    /// Same-bank software service: retry PC, original offset, word, reason.
    guest_service: [u32; 4],
    guest_service_vector: u32,
    guest_service_active: bool,
    /// Native software GPR array; no copied guest registers or bank ownership.
    ea_regs: Option<u32>,
    /// Scratch result metadata; -1 means the latest legal XEA32 needs software.
    ea_info: u32,
    // Word-fetch cache deliberately exposes stale instruction visibility until
    // FENCE.I. This is not a model of the proposed RTL's line geometry/timing.
    fetched: [Option<(u32, u32)>; 256],
}
impl Core {
    /// Construct the current banked ISA at `pc`, with bank 0 active, empty fetch
    /// state and zero control metadata. Both RF arrays start at zero here for
    /// deterministic host setup. Actual RTL RAM is not reset: portable firmware
    /// and ISS/RTL comparisons must initialize every GPR they use, except x0.
    /// Entry alignment/access is checked by the first fetch, not this constructor.
    pub fn new(pc: u32) -> Self {
        Self {
            x: [0; 32],
            pc,
            host_steps: 0,
            retired_steps: 0,
            xalu_ops: 0,
            tlb_lookups: 0,
            tlb_hits: 0,
            tlb_fills: 0,
            xld_ops: 0,
            xld_hits: 0,
            xea_ops: 0,
            xea_hits: 0,
            tlb_stats: None,
            fences: 0,
            bank_enters: 0,
            bank: false,
            inactive: [0; 32],
            bank_reason: 0,
            bank_pc: 0,
            return_pc: 0,
            flags: 0,
            pending_flags: 0,
            guest_dtlb: [None; 16],
            guest_itlb: [None; 4],
            guest_region: (0, 0),
            guest_context: None,
            guest_segments: [GuestSegment::default(); 6],
            guest_service: [0; 4],
            guest_service_vector: 0,
            guest_service_active: false,
            ea_regs: None,
            ea_info: u32::MAX,
            fetched: [None; 256],
        }
    }
    fn trap(&self, cause: Cause, value: u32) -> Trap {
        Trap {
            pc: self.pc,
            cause,
            value,
        }
    }
    /// Probe capacity only; context/segment/rights qualification stays at the
    /// operation below. There is no fallback between these independent views.
    fn guest_mapping(&self, linear: u32, fetch: bool) -> Option<GuestMapping> {
        let table: &[_] = if fetch {
            &self.guest_itlb
        } else {
            &self.guest_dtlb
        };
        table[(linear >> 12) as usize & (table.len() - 1)]
    }

    fn flush_guest_mappings(&mut self, diagnostic_kind: usize) {
        if let Some(stats) = &mut self.tlb_stats {
            stats.invalidate(
                diagnostic_kind,
                self.guest_dtlb
                    .iter()
                    .chain(&self.guest_itlb)
                    .flatten()
                    .count(),
            );
        }
        self.guest_dtlb.fill(None);
        self.guest_itlb.fill(None);
    }
    /// Stopped-hart observation for native ISS/RTL comparisons. Return physical
    /// bank order, independent of which bank happens to be active. This is not
    /// an instruction, a new guest register mapping or a hardware read port.
    pub fn register_banks(&self) -> [[u32; 32]; 2] {
        if self.bank {
            [self.inactive, self.x]
        } else {
            [self.x, self.inactive]
        }
    }
    /// Check a complete native access before any read/write. Callers have already
    /// decoded a width of 1, 2 or 4 bytes and formed a wrapping 32-bit address.
    /// Alignment wins if the address is both misaligned and outside RAM. The
    /// widened end calculation prevents a word at the top of u32 from wrapping
    /// into low RAM. A failed store cannot expose an earlier byte of that store.
    fn address(&self, m: &Memory, addr: u32, bytes: u32, store: bool) -> Result<(), Trap> {
        if addr & (bytes - 1) != 0 {
            return Err(self.trap(
                if store {
                    Cause::StoreMisaligned
                } else {
                    Cause::LoadMisaligned
                },
                addr,
            ));
        }
        self.address_range(m, addr, bytes, store)
    }
    /// Native backing validity, after the instruction-specific alignment/span
    /// rule. Fetch .U permits an unaligned span; ordinary RV accesses do not.
    /// A failed multi-beat load reports its ORIGINAL effective native address
    /// and leaves rd unchanged. Backing is side-effect-free ordinary RAM.
    fn address_range(&self, m: &Memory, addr: u32, bytes: u32, store: bool) -> Result<(), Trap> {
        if addr as u64 + bytes as u64 > m.ram.len() as u64 {
            return Err(self.trap(
                if store {
                    Cause::StoreAccess
                } else {
                    Cause::LoadAccess
                },
                addr,
            ));
        }
        Ok(())
    }
    /// Qualified raw bytes for ONE EA operation. `start` pins the whole span
    /// to its first code page, even if an individually valid disp32 would lie
    /// on the following page. A miss has no architectural guest-fault meaning.
    fn ea_code(
        &self,
        m: &Memory,
        start: u32,
        offset: u32,
        bytes: u32,
    ) -> Result<Option<u32>, Trap> {
        let cs = self.guest_segments[1];
        let linear = cs.base.wrapping_add(offset);
        let first_linear = cs.base.wrapping_add(start);
        if cs.rights & 4 == 0
            || offset < start
            || u64::from(offset) + u64::from(bytes) - 1 > u64::from(cs.limit)
            || linear & !4095 != first_linear & !4095
            || (linear & 4095) + bytes > 4096
        {
            return Ok(None);
        }
        let Some(entry) = self
            .guest_mapping(linear, true)
            .filter(|e| e.linear_page == linear & !4095 && e.rights & 4 != 0)
        else {
            return Ok(None);
        };
        let native = entry.native_page | (linear & 4095);
        self.address_range(m, native, bytes, false)?;
        Ok(Some(m.read(native, (bytes * 8) as u8)))
    }

    /// ModR/M validity is checked by the instruction arm before this helper.
    /// Stage every output locally: even a fault on the second GPR read leaves
    /// both architectural results unchanged. No x86 instruction is executed.
    fn ea32(&self, m: &Memory, modrm: u32, start: u32) -> Result<Option<(u32, u32)>, Trap> {
        let Some(regs) = self.ea_regs else {
            return Ok(None);
        };
        if !self.guest_context.is_some_and(|(ctl, _)| ctl & 9 == 1) {
            return Ok(None);
        }
        let mode = modrm >> 6;
        let rm = modrm & 7;
        let mut consumed: u32 = 0;
        let mut base = Some(rm);
        let mut index = None;
        let mut scale = 0;
        if rm == 4 {
            let Some(sib) = self.ea_code(m, start, start, 1)? else {
                return Ok(None);
            };
            consumed = 1;
            base = Some(sib & 7);
            let reg = (sib >> 3) & 7;
            if reg != 4 {
                index = Some(reg);
            }
            scale = sib >> 6;
        }
        if mode == 0 && base == Some(5) {
            base = None;
        }
        let bytes = if mode == 1 {
            1
        } else if mode == 2 || base.is_none() {
            4
        } else {
            0
        };
        let disp = if bytes != 0 {
            let Some(value) = self.ea_code(m, start, start.wrapping_add(consumed), bytes)? else {
                return Ok(None);
            };
            consumed += bytes;
            if bytes == 1 {
                value as i8 as i32 as u32
            } else {
                value
            }
        } else {
            0
        };
        let mut offset = disp;
        if let Some(reg) = base {
            let native = regs + reg * 4;
            self.address(m, native, 4, false)?;
            offset = offset.wrapping_add(m.read(native, 32));
        }
        if let Some(reg) = index {
            let native = regs + reg * 4;
            self.address(m, native, 4, false)?;
            offset = offset.wrapping_add(m.read(native, 32) << scale);
        }
        let default_ss = base == Some(4) || base == Some(5);
        Ok(Some((offset, (consumed << 1) | u32::from(default_ss))))
    }
    /// Attempt at most `steps` native words, returning on the first trap. With a
    /// zero budget nothing executes. StepLimit preserves the next PC and can be
    /// resumed with another call; it is unrelated to guest x86 timer accounting.
    pub fn run(&mut self, m: &mut Memory, steps: u64) -> Result<Trap, String> {
        for _ in 0..steps {
            if let Some(trap) = self.step(m)? {
                return Ok(trap);
            }
        }
        Ok(self.trap(Cause::StepLimit, 0))
    }
    /// Execute one word: None means completion; Some(trap) means the harness
    /// must handle/stop at that native outcome. Err reports a host/model error,
    /// never an expected x86 guest exception or a request to try another engine.
    pub fn step(&mut self, m: &mut Memory) -> Result<Option<Trap>, String> {
        // Fetch faults precede decode and operand/data access. IALIGN=32 means
        // even a halfword-aligned PC is invalid. Bounds cover all four bytes.
        if self.pc & 3 != 0 {
            return Ok(Some(self.trap(Cause::InstructionMisaligned, self.pc)));
        }
        if self.pc as u64 + 4 > m.ram.len() as u64 {
            return Ok(Some(self.trap(Cause::InstructionAccess, self.pc)));
        }
        // A native store does not update this table. Re-fetch may see an older
        // word until FENCE.I or a colliding fetch evicts it. The PC tag prevents
        // another address in the same slot from supplying the wrong instruction.
        let slot = (self.pc as usize / 4) % self.fetched.len();
        let word = match self.fetched[slot] {
            Some((pc, w)) if pc == self.pc => w,
            _ => {
                let w = m.read(self.pc, 32);
                self.fetched[slot] = Some((self.pc, w));
                w
            }
        };
        self.host_steps += 1;
        // RV field positions: opcode[6:0], rd[11:7], funct3[14:12],
        // rs1[19:15], rs2[24:20], funct7[31:25]. d/f/k are rd/funct3/funct7.
        // Capture both source values before a destination write or bank change;
        // rd may alias either source. I-immediates sign-extend from bit 31.
        let d = ((word >> 7) & 31) as usize;
        let f = (word >> 12) & 7;
        let a_reg = ((word >> 15) & 31) as usize;
        let a = self.x[a_reg];
        let b = self.x[((word >> 20) & 31) as usize];
        let k = word >> 25;
        let imm = ((word as i32) >> 20) as u32;
        // Stage PC and the ordinary destination until all relevant checks pass.
        // Arithmetic/address overflow wraps modulo 2^32 and never traps.
        let mut next = self.pc.wrapping_add(4);
        let mut result = None;
        let illegal = self.trap(Cause::IllegalInstruction, word);
        macro_rules! check {
            ($e:expr) => {
                match $e {
                    Ok(v) => v,
                    Err(t) => return Ok(Some(t)),
                }
            };
        }
        match word & 127 {
            // LUI supplies bits 31:12 and clears the low 12 bits; AUIPC adds
            // that same U-immediate to this instruction's PC, not PC+4.
            0x37 => result = Some(word & 0xfffff000),
            0x17 => result = Some(self.pc.wrapping_add(word & 0xfffff000)),
            // JAL: reconstruct signed J-immediate [20|10:1|11|19:12], with
            // bit 0 implicit zero. Link = PC+4; misaligned target cancels link.
            0x6f => {
                let offset = ((word >> 31) << 20)
                    | (word & 0xff000)
                    | ((word >> 20) & 1) << 11
                    | ((word >> 21) & 1023) << 1;
                next = self.pc.wrapping_add(sign(offset, 21));
                result = Some(self.pc.wrapping_add(4));
            }
            // JALR: signed I-immediate plus the OLD rs1, then clear bit 0.
            // Bit 1 still must be zero: clearing bit 0 does not permit RVC.
            0x67 if f == 0 => {
                next = a.wrapping_add(imm) & !1;
                result = Some(self.pc.wrapping_add(4));
            }
            // BEQ/BNE, BLT/BGE (signed), BLTU/BGEU (unsigned). An untaken
            // branch completes at PC+4 without validating its unused target.
            // B-immediate bit 0 is implicit; bit 12 supplies its sign.
            0x63 => {
                let take = match f {
                    0 => a == b,
                    1 => a != b,
                    4 => (a as i32) < (b as i32),
                    5 => (a as i32) >= (b as i32),
                    6 => a < b,
                    7 => a >= b,
                    _ => return Ok(Some(illegal)),
                };
                if take {
                    let offset = ((word >> 31) << 12)
                        | ((word >> 7) & 1) << 11
                        | ((word >> 25) & 63) << 5
                        | ((word >> 8) & 15) << 1;
                    next = self.pc.wrapping_add(sign(offset, 13));
                }
            }
            // LB/LH/LW/LBU/LHU. All use rs1 + signed imm12; LB/LH sign-extend,
            // LBU/LHU zero-extend, LW keeps all bits. Check even when rd=x0.
            0x03 if matches!(f, 0..=2 | 4 | 5) => {
                let bytes = 1 << (f & 3);
                let addr = a.wrapping_add(imm);
                check!(self.address(m, addr, bytes, false));
                let v = m.read(addr, (bytes * 8) as u8);
                result = Some(if f < 2 { sign(v, bytes * 8) } else { v });
            }
            // SB/SH/SW: S-immediate is [31:25|11:7], sign-extended from 12 bits.
            // Write only the low 8/16/32 bits of rs2 in little-endian order,
            // after validating the entire access. Other bytes remain unchanged.
            0x23 if f <= 2 => {
                let offset = (word >> 25) << 5 | (word >> 7) & 31;
                let addr = a.wrapping_add(sign(offset, 12));
                let bytes = 1 << f;
                check!(self.address(m, addr, bytes, true));
                m.write(addr, &b.to_le_bytes()[..bytes as usize])?;
            }
            // ADDI/SLTI/SLTIU/XORI/ORI/ANDI use sign-extended imm12. SLTIU
            // compares unsigned values AFTER that extension (-1 is 0xffffffff).
            // Shifts use five count bits; upper immediate bits must encode the
            // exact SLLI/SRLI/SRAI form. SRAI replicates the input sign bit.
            0x13 => {
                result = Some(match f {
                    0 => a.wrapping_add(imm),
                    2 => u32::from((a as i32) < (imm as i32)),
                    3 => u32::from(a < imm),
                    4 => a ^ imm,
                    6 => a | imm,
                    7 => a & imm,
                    1 if k == 0 => a << (imm & 31),
                    5 if k == 0 => a >> (imm & 31),
                    5 if k == 32 => ((a as i32) >> (imm & 31)) as u32,
                    _ => return Ok(Some(illegal)),
                });
            }
            // Register ALU forms distinguish ADD/SUB and SRL/SRA by funct7.
            // Shift counts are rs2 & 31; SLT/SLTU produce exactly 0 or 1.
            // funct7=1 selects the eight M operations below, with no hidden
            // instruction expansion and no x86 EFLAGS effect.
            0x33 => {
                result = Some(if k == 1 {
                    multiply_divide(f, a, b)
                } else {
                    match (k, f) {
                        (0, 0) => a.wrapping_add(b),
                        (32, 0) => a.wrapping_sub(b),
                        (0, 1) => a << (b & 31),
                        (0, 2) => u32::from((a as i32) < (b as i32)),
                        (0, 3) => u32::from(a < b),
                        (0, 4) => a ^ b,
                        (0, 5) => a >> (b & 31),
                        (32, 5) => ((a as i32) >> (b & 31)) as u32,
                        (0, 6) => a | b,
                        (0, 7) => a & b,
                        // Standard RV32 Zba. Fixed shifts, then wrapping add;
                        // these do not consume/update either x86 status value.
                        (0x10, 2 | 4 | 6) => (a << (f / 2)).wrapping_add(b),
                        _ => return Ok(Some(illegal)),
                    }
                });
            }
            // FENCE: RAM effects already complete in instruction order on this
            // single hart, so no queue remains to drain. Reserved operands are
            // ignored; rd is not written. FENCE alone does not publish code.
            0x0f if f == 0 => (),
            // FENCE.I: preceding stores are complete, discard all fetched words
            // and continue at PC+4. Neither guest source-version checks nor an
            // x86 TLB invalidation is implied by native instruction publication.
            0x0f if f == 1 => {
                self.fetched.fill(None);
                self.fences += 1;
            }
            // Only these exact SYSTEM encodings exist. The faulting PC remains
            // visible, with no link/register write or automatic bank return.
            0x73 if word == 0x00000073 => return Ok(Some(self.trap(Cause::Ecall, 0))),
            0x73 if word == 0x00100073 => return Ok(Some(self.trap(Cause::Breakpoint, 0))),
            // A partial-register copy shares XALU's lane encoding but does not
            // even replace pending flags. Preserve the OLD rs1 container.
            0x0b if f < 4 && k & 0x40 != 0 => {
                if k & !0x50 != 0 || (k & 0x10 != 0 && f < 2) {
                    return Ok(Some(illegal));
                }
                let mask = [u32::MAX, 0xffff, 0xff, 0xff][f as usize];
                let shift = if f == 3 { 8 } else { 0 };
                let source = (b >> if k & 0x10 != 0 { 8 } else { 0 }) & mask;
                result = Some((a & !(mask << shift)) | (source << shift));
            }
            // Fixed x86 arithmetic/status words. Decode ALL reserved fields
            // before changing pending flags; even rd=x0 performs those checks.
            0x0b if f < 4 => {
                let kind = k & 7;
                if k & !0x3f != 0
                    || (k & 0x10 != 0 && f < 2)
                    || (k & 0x20 != 0 && kind != 0 && kind != 5)
                    || (kind == 7 && ((word >> 20) & 31 != 0 || k & 0x30 != 0))
                {
                    return Ok(Some(illegal));
                }
                let (value, flags) = xalu(a, b, f, k, self.flags);
                result = Some(value);
                self.pending_flags = flags;
                if k & 8 != 0 {
                    self.flags = flags;
                }
            }
            0x0b if word >> 20 == 0 => match f {
                4 if d == 0 => {
                    self.flags = a & STATUS_MASK;
                    self.pending_flags = self.flags;
                }
                5 => result = Some((a & !STATUS_MASK) | self.flags),
                6 => {
                    // One RF write plus a separate status write, with no third
                    // operand or extra general-register write port.
                    result = Some(a);
                    self.flags = self.pending_flags;
                }
                7 if a_reg < 16 => result = Some(u32::from(condition(a_reg, self.flags))),
                _ => return Ok(Some(illegal)),
            },
            // Translation-only assist. All miss causes are ordinary results:
            // the runtime branches to software while old guest state is intact.
            // Indexing the table cannot touch guest RAM or perform MMIO.
            0x2b => {
                let index = ((a >> 12) & 15) as usize;
                match f {
                    0 if word >> 24 == 0 && (word >> 20) & 3 != 3 => {
                        let mode = word >> 20;
                        let align_mask = (1 << (mode & 3)) - 1;
                        let required = [1, 2, 4, 3][(mode >> 2) as usize];
                        let entry = self.guest_mapping(a, mode >> 2 == 2);
                        if let Some(stats) = &mut self.tlb_stats {
                            stats.probe(
                                false,
                                (mode >> 2) as usize,
                                entry,
                                a,
                                if a & align_mask != 0 { 3 } else { 0 },
                            );
                        }
                        result = Some(match entry {
                            Some(entry)
                                if entry.linear_page == a & !0xfff
                                    && entry.rights & required == required
                                    && a & align_mask == 0 =>
                            {
                                // The fill's window check excludes the sentinel
                                // and proves this addition cannot wrap.
                                entry.native_page | (a & 0xfff)
                            }
                            _ => u32::MAX,
                        });
                    }
                    1 if d == 0 && k <= 1 && b & (if k == 0 { 0xffc } else { 0xffb }) == 0 => {
                        let page = b & !0xfff;
                        let table: &mut [_] = if k == 0 {
                            &mut self.guest_dtlb
                        } else {
                            &mut self.guest_itlb
                        };
                        let slot = (a >> 12) as usize & (table.len() - 1);
                        // The aperture is checked once at publication, not on
                        // every hit. Aligned exclusive bounds prove the entire
                        // page is in range. Rejecting a refill evicts the OLD
                        // mapping too, so a stale successful pointer cannot win.
                        let mapping = (page >= self.guest_region.0 && page < self.guest_region.1)
                            .then_some(GuestMapping {
                                linear_page: a & !0xfff,
                                native_page: page,
                                rights: b & 7,
                            });
                        if let Some(stats) = &mut self.tlb_stats {
                            stats.fill(table[slot], mapping, b & 7);
                        }
                        table[slot] = mapping;
                    }
                    2 if d == 0 && word >> 20 == 0 => {
                        if let Some(stats) = &mut self.tlb_stats {
                            stats.invalidate(
                                0,
                                usize::from(self.guest_dtlb[index].is_some())
                                    + usize::from(self.guest_itlb[index & 3].is_some()),
                            );
                        }
                        self.guest_dtlb[index] = None;
                        self.guest_itlb[index & 3] = None;
                    }
                    3 if d == 0 && a_reg == 0 && word >> 20 == 0 => {
                        self.flush_guest_mappings(1);
                    }
                    4 if d == 0 && k == 0 && (a | b) & 0xfff == 0 && a <= b => {
                        self.guest_region = (a, b);
                        self.flush_guest_mappings(2);
                    }
                    5 if d == 0 && k == 0 && a & !0x3f == 0 => {
                        if self.guest_context != Some((a, b)) {
                            self.flush_guest_mappings(3);
                            self.guest_segments.fill(GuestSegment::default());
                        }
                        self.guest_context = Some((a, b));
                    }
                    6 if d < 6 && k < 8 => {
                        self.guest_segments[d] = GuestSegment {
                            base: a,
                            limit: b,
                            rights: k,
                        };
                    }
                    7 if d == 0 && word >> 20 == 0 && a & 3 == 0 => {
                        self.guest_service_vector = a;
                    }
                    7 if (word >> 20) & 31 == 0 && k == 1 && a_reg < 8 => {
                        result = Some(match a_reg {
                            0..=3 => self.guest_service[a_reg],
                            4 => self.guest_context.unwrap_or_default().0,
                            5 => self.guest_context.unwrap_or_default().1,
                            6 => self.guest_service_vector,
                            _ => u32::from(self.guest_service_active),
                        });
                    }
                    7 if d == 0
                        && (word >> 20) & 31 == 0
                        && k == 2
                        && self.guest_service_active =>
                    {
                        next = if a_reg == 0 { self.guest_service[0] } else { a };
                        if next & 3 != 0 {
                            return Ok(Some(self.trap(Cause::InstructionMisaligned, next)));
                        }
                        self.guest_service_active = false;
                    }
                    _ => return Ok(Some(illegal)),
                }
            }
            0x5b => {
                let store = f & 4 != 0;
                let register_segment = k & 64 != 0;
                let segment = if register_segment { b } else { k & 7 };
                let rmw = k & 8 != 0;
                let fetch = k & 16 != 0;
                let extra = k & 32 != 0;
                let unaligned_fetch = fetch && extra;
                if f & 3 == 3
                    || segment >= 6
                    || (register_segment && k & 7 != 0)
                    || (store && (d != 0 || rmw || fetch || register_segment))
                    || (!store
                        && ((!register_segment && (word >> 20) & 31 != 0) || (extra && !fetch)))
                    || (fetch && (segment != 1 || rmw))
                {
                    return Ok(Some(illegal));
                }
                let bytes = 1 << (f & 3);
                let required = if store {
                    2
                } else if fetch {
                    4
                } else if rmw {
                    3
                } else {
                    1
                };
                let seg = self.guest_segments[segment as usize];
                let linear = seg.base.wrapping_add(a);
                let entry = self.guest_mapping(linear, fetch);
                let bad_span = if unaligned_fetch {
                    (linear & 0xfff) + bytes > 4096
                } else {
                    linear & (bytes - 1) != 0
                };
                let reason = if !self.guest_context.is_some_and(|(ctl, _)| ctl & 9 == 1) {
                    1
                } else if seg.rights & required != required
                    || u64::from(a) + u64::from(bytes) - 1 > u64::from(seg.limit)
                {
                    2
                } else if bad_span {
                    3
                } else if !entry.is_some_and(|e| {
                    e.linear_page == linear & !0xfff && e.rights & required == required
                }) {
                    4
                } else {
                    0
                };
                if let Some(stats) = &mut self.tlb_stats {
                    let intent = if store {
                        1
                    } else if fetch {
                        2
                    } else if rmw {
                        3
                    } else {
                        0
                    };
                    stats.probe(true, intent, entry, linear, reason);
                }
                if reason != 0 {
                    if self.guest_service_vector == 0 || self.guest_service_active {
                        return Ok(Some(self.trap(Cause::GuestService, a)));
                    }
                    self.guest_service = [self.pc, a, word, reason];
                    self.guest_service_active = true;
                    next = self.guest_service_vector;
                } else {
                    let native = entry.unwrap().native_page | (linear & 0xfff);
                    check!(self.address_range(m, native, bytes, store));
                    if store {
                        m.write(native, &b.to_le_bytes()[..bytes as usize])?;
                        if extra {
                            self.flags = self.pending_flags;
                        }
                    } else {
                        result = Some(m.read(native, (bytes * 8) as u8));
                    }
                }
            }
            // Private context words, using ordinary RV operand positions. Each
            // guard is part of the encoding: a disallowed bank or nonzero
            // reserved field traps instead of partially performing the operation.
            0x7b => match f {
                0 if k == 0 && a <= 255 && a >> 6 != 3 => {
                    // Code qualification failure is a try-result, not a native
                    // service redirect. The same Rust decoder continues once.
                    match self.ea32(m, a, b) {
                        Ok(Some((offset, info))) => {
                            result = Some(offset);
                            self.ea_info = info;
                        }
                        Ok(None) => self.ea_info = u32::MAX,
                        Err(trap) => return Ok(Some(trap)),
                    }
                }
                1 if a_reg == 0 && word >> 20 == 0 => result = Some(self.ea_info),
                2 if d == 0 && word >> 20 == 0 && a & 3 == 0 && a <= 0xffff_ffe0 => {
                    self.ea_regs = Some(a);
                    self.ea_info = u32::MAX;
                }
                3 if a_reg < 16 && (word >> 20) & 31 == 0 => {
                    if condition(a_reg, self.flags) {
                        let offset = ((word >> 31) << 12)
                            | ((word >> 7) & 1) << 11
                            | ((word >> 25) & 63) << 5
                            | ((word >> 8) & 15) << 1;
                        next = self.pc.wrapping_add(sign(offset, 13));
                    }
                }
                // XENTER rs1: funct3=4, rd=0, imm12=0, bank 0 only.
                // Validate target alignment before parking the runtime bank.
                // Save PC+4, select bank 1, then fetch rs1's OLD bank-0 value.
                // An aligned out-of-RAM target faults on the NEXT fetch, after
                // entry; bank 1 is not an automatic sandbox or x86 checkpoint.
                4 if !self.bank && d == 0 && word >> 20 == 0 => {
                    if a & 3 != 0 {
                        return Ok(Some(self.trap(Cause::InstructionMisaligned, a)));
                    }
                    self.return_pc = next;
                    self.bank = true;
                    self.bank_enters += 1;
                    core::mem::swap(&mut self.x, &mut self.inactive);
                    next = a;
                }
                // XLEAVE reason: funct3=5, rd=rs1=0, bank 1 only. Bits 31:20
                // are an unsigned reason, not a sign-extended address. Record
                // bank-1 PC+4 for inspection and return to the saved runtime PC.
                // This is not guest x86 EIP and does not validate a continuation.
                5 if self.bank && d == 0 && a_reg == 0 => {
                    self.pending_flags = self.flags;
                    self.bank_reason = word >> 20;
                    self.bank_pc = next;
                    self.bank = false;
                    core::mem::swap(&mut self.x, &mut self.inactive);
                    next = self.return_pc;
                }
                // Bank 0 only, funct3=6, rs2=0:
                //   funct7=0: XREAD rd, bank1_reg; rs1 is a register INDEX.
                //   funct7=1: XINFO rd, selector; rs1 is a selector, not x[rs1].
                // rd names the active bank's destination, with usual x0 discard.
                // XREAD can read parked x0; XINFO validates even if rd is zero.
                6 if !self.bank && (word >> 20) & 31 == 0 && k <= 1 => {
                    result = Some(if k == 0 {
                        self.inactive[a_reg]
                    } else {
                        // 0: last leave reason; 1: last leave's PC+4;
                        // 2: current experimental ISA identity. Other selectors
                        // are illegal. This ID is not a compatibility promise.
                        match a_reg {
                            0 => self.bank_reason,
                            1 => self.bank_pc,
                            2 => 0x4231_0008,
                            _ => return Ok(Some(illegal)),
                        }
                    });
                }
                // XWRITE bank1_reg, rs1: funct3=7, imm12=0, bank 0 only.
                // rd names the PARKED destination; rs1 supplies an ACTIVE value.
                // Preserve parked x0, leave active GPRs untouched, then PC += 4.
                7 if !self.bank && word >> 20 == 0 => {
                    if d != 0 {
                        self.inactive[d] = a;
                    }
                }
                _ => return Ok(Some(illegal)),
            },
            _ => return Ok(Some(illegal)),
        }
        // A taken branch/jump checks target alignment before publishing rd/PC.
        // Target ACCESS is checked by the next fetch, so an aligned out-of-RAM
        // JAL/JALR target preserves the completed link write. Sequential PC+4
        // and saved bank continuations are aligned by the execution invariants.
        if next & 3 != 0 {
            return Ok(Some(self.trap(Cause::InstructionMisaligned, next)));
        }
        // x0 suppresses only destination writeback, never instruction checks or
        // memory effects. All operands were read before this point (rd==rs1/rs2).
        if let Some(value) = result {
            if d != 0 {
                self.x[d] = value;
            }
        }
        self.pc = next;
        match (word & 0x7f, f) {
            (0x0b, 0..=3) if k & 0x40 == 0 => self.xalu_ops += 1,
            (0x2b, 0) => {
                self.tlb_lookups += 1;
                self.tlb_hits += u64::from(result != Some(u32::MAX));
            }
            (0x2b, 1) => self.tlb_fills += 1,
            (0x5b, 0..=2) => {
                self.xld_ops += 1;
                self.xld_hits += u64::from(result.is_some());
            }
            (0x7b, 0) => {
                self.xea_ops += 1;
                self.xea_hits += u64::from(result.is_some());
            }
            _ => (),
        }
        self.retired_steps += 1;
        Ok(None)
    }
}

/// XALU after encoding validation. The widened arithmetic below expresses
/// mathematical carry/borrow, rather than prescribing a 64-bit hardware adder.
/// RTL needs a width-selected result and carry from a 33-bit add/subtract path.
/// Tests compare independent integer mathematics and the shared interpreter;
/// v86 arith.rs agrees on arithmetic flags, INC/DEC CF and cleared logic AF.
fn xalu(a: u32, b: u32, lane: u32, control: u32, incoming: u32) -> (u32, u32) {
    let bits = match lane {
        0 => 32,
        1 => 16,
        _ => 8,
    };
    let shift = if lane == 3 { 8 } else { 0 };
    let mask = u32::MAX >> (32 - bits);
    let selected = (a >> shift) & mask;
    let kind = control & 7;
    let (lhs, rhs) = if kind == 7 {
        (0, selected)
    } else {
        (
            selected,
            (b >> if control & 0x10 != 0 { 8 } else { 0 }) & mask,
        )
    };
    let carry = if kind == 2 || kind == 3 {
        incoming & 1
    } else {
        0
    };
    let subtract = matches!(kind, 3 | 5 | 7);
    let logic = matches!(kind, 1 | 4 | 6);
    let result = match kind {
        1 => lhs | rhs,
        4 => lhs & rhs,
        6 => lhs ^ rhs,
        _ if subtract => lhs.wrapping_sub(rhs).wrapping_sub(carry),
        _ => lhs.wrapping_add(rhs).wrapping_add(carry),
    } & mask;
    let sign = 1 << (bits - 1);
    let mut flags = u32::from((result as u8).count_ones().is_multiple_of(2)) << 2
        | u32::from(result == 0) << 6
        | u32::from(result & sign != 0) << 7;
    if !logic {
        let cf = if subtract {
            (lhs as u64) < rhs as u64 + carry as u64
        } else {
            lhs as u64 + rhs as u64 + carry as u64 > mask as u64
        };
        let of = (lhs ^ result) & (if subtract { lhs ^ rhs } else { !(lhs ^ rhs) }) & sign != 0;
        flags |= u32::from(cf) | ((lhs ^ rhs ^ result) & 0x10) | u32::from(of) << 11;
    }
    if control & 0x20 != 0 {
        flags = (flags & !1) | (incoming & 1);
    }
    ((a & !(mask << shift)) | (result << shift), flags)
}

/// The x86 condition numbering pairs each predicate with its negation. AF has
/// no condition code. Only committed status is observable by this operation.
fn condition(cc: usize, flags: u32) -> bool {
    let cf = flags & 1 != 0;
    let pf = flags & 4 != 0;
    let zf = flags & 0x40 != 0;
    let sf = flags & 0x80 != 0;
    let of = flags & 0x800 != 0;
    [of, cf, zf, cf || zf, sf, pf, sf != of, zf || sf != of][cc >> 1] ^ (cc & 1 != 0)
}

/// Sign-extend the low `bits` into the RV32 bit pattern; widths are decoded
/// constants in 1..=32, not unchecked external operands.
fn sign(value: u32, bits: u32) -> u32 {
    ((value << (32 - bits)) as i32 >> (32 - bits)) as u32
}
/// RV32M funct3 order: MUL, MULH, MULHSU, MULHU, DIV, DIVU, REM, REMU.
/// MUL returns low 32 bits; high-half forms use signed/signed, signed/unsigned
/// and unsigned/unsigned 64-bit products respectively. MULHSU widens unsigned b
/// to a nonnegative i64, preserving values above i32::MAX.
///
/// Division by zero returns all ones for the quotient and the original dividend
/// for the remainder, without a trap. Signed division truncates toward zero;
/// nonzero remainder has the dividend's sign. MIN/-1 returns MIN and remainder
/// zero; wrapping_div/rem express this ISA result without a host overflow panic.
/// Each call completes one native word here; iterative hardware latency is
/// deliberately outside this functional model.
fn multiply_divide(f: u32, a: u32, b: u32) -> u32 {
    match f {
        0 => a.wrapping_mul(b),
        1 => (((a as i32 as i64) * (b as i32 as i64)) >> 32) as u32,
        2 => (((a as i32 as i64) * (b as i64)) >> 32) as u32,
        3 => ((a as u64 * b as u64) >> 32) as u32,
        4 if b == 0 => u32::MAX,
        4 => (a as i32).wrapping_div(b as i32) as u32,
        5 => a.checked_div(b).unwrap_or(u32::MAX),
        6 if b == 0 => a,
        6 => (a as i32).wrapping_rem(b as i32) as u32,
        7 => a.checked_rem(b).unwrap_or(a),
        _ => unreachable!(),
    }
}

/// Load a static little-endian RV32 executable into stopped physical RAM.
///
/// Host setup only: no guest instruction, native loader service or automatic
/// x86 translation happens here. Validate the complete program-header layout,
/// all load ranges and entry point before copying bytes, so a malformed image
/// cannot leave a partially loaded program. Copy file bytes and zero each
/// segment's BSS; the firmware entry stub initializes stack and ABI registers.
/// ELF permission flags qualify the entry point, not runtime RAM permissions.
pub fn load_elf(bytes: &[u8], m: &mut Memory) -> Result<u32, String> {
    let u16at = |n| -> Result<u16, String> {
        Ok(u16::from_le_bytes(
            bytes
                .get(n..n + 2)
                .ok_or("truncated ELF")?
                .try_into()
                .unwrap(),
        ))
    };
    let u32at = |n| -> Result<u32, String> {
        Ok(u32::from_le_bytes(
            bytes
                .get(n..n + 4)
                .ok_or("truncated ELF")?
                .try_into()
                .unwrap(),
        ))
    };
    // ELF32, little endian, current format, ET_EXEC, EM_RISCV, e_flags=0:
    // this accepts the plain integer ABI and rejects compressed/embedded/float
    // ABI flags. No relocations or dynamic linking are performed by the harness.
    if bytes.get(..7) != Some(b"\x7fELF\x01\x01\x01")
        || u16at(16)? != 2
        || u16at(18)? != 243
        || u32at(20)? != 1
        || u32at(36)? != 0
        || u16at(40)? != 52
        || u16at(42)? != 32
    {
        return Err("expected static RV32IM soft-float ELF32 (no RVC/RVE)".into());
    }
    let entry = u32at(24)?;
    let start = u32at(28)? as usize;
    let count = u16at(44)? as usize;
    if count == 0
        || start
            .checked_add(count * 32)
            .is_none_or(|end| end > bytes.len())
    {
        return Err("invalid ELF program header table".into());
    }
    // Collect validated PT_LOAD segments without modifying RAM. Virtual and
    // physical addresses must match because this core has no native MMU.
    let mut segments = Vec::new();
    let mut executable_entry = false;
    for n in 0..count {
        let h = start + n * 32;
        // Reject PT_DYNAMIC/PT_INTERP; non-load metadata is otherwise ignored.
        match u32at(h)? {
            2 | 3 => return Err("dynamic ELF is unsupported".into()),
            1 => (),
            _ => continue,
        }
        let offset = u32at(h + 4)? as usize;
        let addr = u32at(h + 8)?;
        let filesz = u32at(h + 16)? as usize;
        let size = u32at(h + 20)? as usize;
        if addr != u32at(h + 12)?
            || filesz > size
            || addr as u64 + size as u64 > m.ram.len() as u64
            || offset
                .checked_add(filesz)
                .is_none_or(|end| end > bytes.len())
        {
            return Err("ELF segment outside file/RAM or nonidentity address".into());
        }
        let end = addr as usize + size;
        if segments
            .iter()
            .any(|&(a, s, _, _)| (addr as usize) < a + s && a < end)
        {
            return Err("overlapping ELF segments".into());
        }
        // At least a whole aligned instruction at e_entry must fit in a segment
        // marked executable. The remaining RAM is still ordinary physical RAM.
        executable_entry |=
            u32at(h + 24)? & 1 != 0 && entry >= addr && entry as u64 + 4 <= end as u64;
        segments.push((addr as usize, size, offset, filesz));
    }
    if entry & 3 != 0 || !executable_entry {
        return Err("ELF entry outside aligned executable RAM".into());
    }
    for (addr, size, offset, filesz) in segments {
        m.write(addr as u32, &bytes[offset..offset + filesz])?;
        m.write((addr + filesz) as u32, &vec![0; size - filesz])?;
    }
    Ok(entry)
}
