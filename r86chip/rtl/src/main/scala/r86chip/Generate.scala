package r86chip

import circt.stage.ChiselStage

object Generate extends App {
  require(args.length == 2, "usage: runMain r86chip.Generate BANKS OUTPUT_DIRECTORY")
  ChiselStage.emitSystemVerilogFile(
    new Core(args(0).toInt),
    Array("--target-dir", args(1)),
    Array(
      "--disable-all-randomization",
      "--lowering-options=disallowLocalVariables,disallowPackedArrays,noAlwaysComb,locationInfoStyle=wrapInAtSquareBracket"
    )
  )
}
