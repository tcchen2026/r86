# Local assets

Not in Git, except this file and the FreeDOS kit's manifest and pin. The
public tools disc's sources are tracked in `r86-tools/` (see its README);
`script/build_tools_iso.py` writes both images here:

- `r86_tools.iso`: the public tools CD.
- `r86_tools_full.iso`: internal only, the public contents plus the original
  VBEMP and user-supplied Super PI packages listed in `r86_tools_full.json`,
  with `r86_tools_full.txt` as its README.TXT. **Never publish or distribute
  the full ISO.** VBEMP's license restricts bundling; Super PI also stays
  internal. Preserve their original licenses.

Source READMEs and checksums in each local asset directory record provenance.
The build fails on a missing source instead of silently removing a tool. Add
public tools under `r86-tools/disc/`; add private extras only to the full
manifest. r86-tool finds a user-provided VBEMP package on a CD or accepts its
extracted directory; the public ISO does not include VBEMP. The standalone
local VBEMP ISO is retained for driver tests.
